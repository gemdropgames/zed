//! The core drive loop, ported from `ggo-emu/src/lib.rs::run_cart` +
//! `ggo-emu/src/native.rs`. Nothing here emulates anything -- every step
//! is a `ggo_emu_core` call, in the same order and with the same
//! arguments the standalone binary uses.
//!
//! ## Why an OS thread and not `cx.background_spawn`
//!
//! `ggo_emu_core::perfsim::PerfSim` holds
//! `Option<Box<dyn CacheProfiler>>` (no `+ Send` bound), so `Peripherals`
//! -- and therefore the whole `(Cpu, Mmu, Peripherals)` triple -- is
//! `!Send`. `background_spawn` needs a `Send` future, which would hold
//! that state across the per-frame await. A plain `std::thread::spawn`
//! only requires the *closure captures* to be `Send`, and the captures
//! here are a `PathBuf`, a channel sender and two `Arc<Atomic*>`; the
//! emulator state is constructed inside the thread and never crosses a
//! boundary. Fixing this upstream would mean a `+ Send` bound in the
//! `ggo` repo, which is out of scope for a fork-side task.
//!
//! ## What is deliberately not ported
//!
//! - **Audio.** F3 defers it (constraints.md); the standalone binary
//!   stays the with-audio path. The APU still *runs* (`vsync_wait`
//!   advances the mixer inside `ggo_emu_core`), so cart timing and perf
//!   behaviour are unchanged -- nothing drains its sample ring.
//! - **The perf simulation.** `Peripherals::new` leaves `perf` disabled
//!   and this loop never enables it, so there is no wire-stall frame
//!   stretch (`run_cart`'s `stall_realtime`) and no `perf.db` write. The
//!   pane paces every frame at [`FRAME_TIME`]; profiling stays a
//!   standalone-binary job (that's what the charts panel reads).
//! - **Save-file persistence.** The save region exists and
//!   `save_read`/`save_write` work, but `savefile::flush_save` is not
//!   called, so a pane run's saves are in-memory only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ggo_emu_core::cart::Cart;
use ggo_emu_core::cpu::Cpu;
use ggo_emu_core::mmu::{CART_XIP_BASE, DEFAULT_MAIN_RAM_BYTES, DEFAULT_VRAM_BYTES, Mmu};
use ggo_emu_core::peripherals::{Peripherals, SCREEN_HEIGHT, SCREEN_WIDTH};
use ggo_emu_core::run::{FrameEvent, run_until_event};

/// One 60 Hz vsync period -- `ggo_emu::FRAME_TIME`, redeclared because it
/// lives in the `ggo-emu` binary crate (which drags in winit, cpal and two
/// SQLite engines) rather than in `ggo-emu-core`.
pub const FRAME_TIME: Duration = Duration::from_micros(16_667);

/// Instructions interpreted per driver turn before the loop comes up for
/// air. `ggo_emu::PER_TURN_BUDGET` verbatim: big enough to clear a
/// frame's work, small enough that a cart spinning without `vsync_wait`
/// still lets Stop take effect promptly.
pub const PER_TURN_BUDGET: u64 = 5_000_000;

/// Framebuffer geometry, re-exported so the panel doesn't have to depend
/// on `ggo_emu_core::peripherals` directly.
pub const WIDTH: u32 = SCREEN_WIDTH as u32;
pub const HEIGHT: u32 = SCREEN_HEIGHT as u32;

/// What the emulator thread sends the panel.
pub enum EmuMsg {
    /// One presented frame: `WIDTH * HEIGHT * 4` bytes of BGRA8 (gpui's
    /// `RenderImage` frame format) plus the cart-visible frame number.
    Frame { bgra: Vec<u8>, frame: u32 },
    /// The run is over -- cart exit, CPU fault, or a load failure. The
    /// thread has already dropped the core by the time this lands.
    Ended(String),
}

/// The panel's handle on a running emulator thread.
pub struct Session {
    /// The cart this session is running (rel path, for the header).
    pub cart: String,
    /// Latest button mask, published by the panel's key handlers and
    /// latched by the thread at each frame boundary. An atomic rather
    /// than a channel because input is level-triggered state, not a
    /// stream: the cart only ever asks "what is held right now".
    input: Arc<AtomicU32>,
    /// Checked once per driver turn. Set by [`Self::stop`] / `Drop`.
    stop: Arc<AtomicBool>,
}

impl Session {
    /// Signal the thread to stop. Deliberately does NOT join: a turn can
    /// take up to [`PER_TURN_BUDGET`] instructions, and blocking the UI
    /// thread on it would stall the whole window. The thread checks the
    /// flag at the top of its next turn, returns, and drops the core
    /// (and the sender, which is what makes the panel's pump task
    /// finish) on the way out.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn set_input(&self, mask: u32) {
        self.input.store(mask, Ordering::Release);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a run: spawn the emulator thread for `cart_path` and return its
/// handle plus the receiver the panel pumps.
///
/// The channel is bounded at one frame and the thread uses `try_send`, so
/// a UI that falls behind drops frames instead of back-pressuring the
/// emulator into slow motion -- the right trade for a video feed, and the
/// same effect `native::Display::present` gets from presenting straight
/// to a surface.
pub fn start(cart_path: PathBuf, cart: String) -> (Session, async_channel::Receiver<EmuMsg>) {
    let (tx, rx) = async_channel::bounded(1);
    let input = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let session = Session {
        cart,
        input: input.clone(),
        stop: stop.clone(),
    };
    std::thread::Builder::new()
        .name("ggo-emu-panel".into())
        .spawn(move || run(&cart_path, &tx, &input, &stop))
        .expect("spawning the ggo emulator thread");
    (session, rx)
}

/// The drive loop itself -- `run_cart`'s body minus the window, the
/// audio device, the perf report and the save flush.
fn run(cart_path: &Path, tx: &async_channel::Sender<EmuMsg>, input: &AtomicU32, stop: &AtomicBool) {
    let bytes = match std::fs::read(cart_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            let _ = tx.send_blocking(EmuMsg::Ended(format!("{}: {e}", cart_path.display())));
            return;
        }
    };
    let cart = match Cart::parse(&bytes) {
        Ok(cart) => cart,
        Err(e) => {
            let _ = tx.send_blocking(EmuMsg::Ended(format!("cart: {e}")));
            return;
        }
    };

    let mut mmu = Mmu::new(DEFAULT_MAIN_RAM_BYTES, DEFAULT_VRAM_BYTES);
    mmu.xip = cart.body;
    let mut cpu = Cpu::new(CART_XIP_BASE.wrapping_add(cart.header.entry_offset));

    // Same wall-clock RNG seed `run_cart` uses, so successive runs of the
    // same cart differ but a single run stays deterministic once started.
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut p = Peripherals::new(seed, cart.header.save_bytes);
    if let Some(toc) = cart.toc {
        p.assets.set_toc(toc);
    }
    // `asset_load` resolves relative to the cart's own directory, which
    // is `run_cart`'s default when no `--card-dir` is given.
    if let Some(dir) = cart_path.parent() {
        p.assets.set_card_dir(dir.to_path_buf());
    }

    let start = Instant::now();
    let mut last_present = Instant::now();

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let (event, _insns) = run_until_event(&mut cpu, &mut mmu, &mut p, PER_TURN_BUDGET, false);
        match event {
            // Frame boundary: the cart drew a complete frame and called
            // vsync_wait. Publish it, latch input, update the clock --
            // `run_cart`'s Vsync arm, minus the present and the stall.
            FrameEvent::Vsync(frame) => {
                p.input_mask = input.load(Ordering::Acquire);
                p.set_ticks_ms(start.elapsed().as_millis().min(u32::MAX as u128) as u32);
                let msg = EmuMsg::Frame {
                    bgra: rgb565_to_bgra(&p.default_fb),
                    frame,
                };
                // Full channel = the UI hasn't drained the previous
                // frame yet; drop this one. Closed = the panel dropped
                // the receiver (Stop, or the panel itself went away).
                if let Err(e) = tx.try_send(msg)
                    && e.is_closed()
                {
                    return;
                }
                if let Some(hold) = pace_sleep(last_present.elapsed(), FRAME_TIME) {
                    std::thread::sleep(hold);
                }
                last_present = Instant::now();
            }
            // Budget exhausted mid-frame: the framebuffer is half-drawn,
            // so do NOT publish it (`run_cart` likewise refuses to
            // present a partial buffer). Latch input and go round again;
            // the `stop` check at the top of the loop is what keeps this
            // interruptible for a cart that never reaches vsync_wait.
            FrameEvent::Budget => {
                p.input_mask = input.load(Ordering::Acquire);
            }
            FrameEvent::Exit(code) => {
                let _ = tx.send_blocking(EmuMsg::Ended(format!("cart exited with {code}")));
                return;
            }
            FrameEvent::Fault(trap) => {
                let _ = tx.send_blocking(EmuMsg::Ended(format!("cpu fault: {trap:?}")));
                return;
            }
        }
    }
}

/// How long to hold a just-published frame, given `elapsed` since the
/// previous one. `None` once the frame is already late -- a late frame is
/// shown immediately rather than compounding the delay, which is exactly
/// what `native::Display::present`'s `if elapsed < hold` does.
pub fn pace_sleep(elapsed: Duration, frame_time: Duration) -> Option<Duration> {
    frame_time.checked_sub(elapsed).filter(|d| !d.is_zero())
}

/// RGB565 framebuffer -> BGRA8, gpui's `RenderImage` frame format.
///
/// The 5/6-bit -> 8-bit expansion is `ggo_emu::dump_ppm`'s (replicate the
/// high bits into the low ones, so 0x1F -> 0xFF rather than 0xF8), and the
/// channel order is `ggo_common::rgba_to_bgra`'s target. Done on the
/// emulator thread so the UI thread only ever wraps an already-correct
/// buffer -- `image::ImageBuffer::from_raw` takes the `Vec` by value, so
/// there is no second copy anywhere in the path.
pub fn rgb565_to_bgra(fb: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(fb.len() * 4);
    for &px in fb {
        let r5 = (px >> 11) & 0x1F;
        let g6 = (px >> 5) & 0x3F;
        let b5 = px & 0x1F;
        out.push(((b5 << 3) | (b5 >> 2)) as u8);
        out.push(((g6 << 2) | (g6 >> 4)) as u8);
        out.push(((r5 << 3) | (r5 >> 2)) as u8);
        out.push(0xFF);
    }
    out
}

/// Hand-assembled cart fixtures, shared by this module's tests and the
/// panel's.
#[cfg(test)]
pub mod fixture {

    // There is no committed `.cart` anywhere in the ggo repo (checked:
    // `find . -name '*.cart' -not -path '*/target/*'` is empty, and
    // `tools/ggo-fixture` is a perf-database fixture, not a cart), and
    // packing one in test setup would mean a riscv32 toolchain plus
    // `emd`. So the drive test assembles its own eight-instruction cart
    // instead -- real `GGOC` header, real RV32I machine code, driven
    // through the real `drive::start` thread. It is a genuine end-to-end
    // check of the port (header parse -> XIP map -> interpret -> ecall
    // dispatch -> PPU compose -> RGB565 -> BGRA -> channel), just of a
    // cart small enough to write by hand.

    use ggo_emu_core::cart::{HEADER_LEN, MAGIC, SUPPORTED_HEADER_VERSION};
    use ggo_emu_core::crc32::crc32;

    /// `addi rd, x0, imm` -- the only instruction form the fixture needs
    /// besides `ecall` and a backwards `jal`.
    fn addi(rd: u32, imm: i32) -> u32 {
        ((imm as u32 & 0xFFF) << 20) | (rd << 7) | 0x13
    }

    /// `jal x0, offset` (offset in bytes, relative to this instruction).
    fn jal_x0(offset: i32) -> u32 {
        let imm = offset as u32;
        ((imm >> 20) & 1) << 31
            | ((imm >> 1) & 0x3FF) << 21
            | ((imm >> 11) & 1) << 20
            | ((imm >> 12) & 0xFF) << 12
            | 0x6F
    }

    const ECALL: u32 = 0x0000_0073;
    const A0: u32 = 10;
    const A1: u32 = 11;
    const A2: u32 = 12;
    const A7: u32 = 17;

    /// Syscall numbers, from `gemdrop_sdk::sys` / `ggo_emu_core::abi`.
    const SYS_PRESENT: i32 = 0x00;
    const SYS_VSYNC_WAIT: i32 = 0x01;
    const SYS_SET_PALETTE: i32 = 0x05;

    /// RGB565 green -- 0x07E0, which happens to fit in a 12-bit signed
    /// immediate, so the program needs no `lui`.
    const GREEN: u16 = 0x07E0;

    /// A cart that paints the whole screen green and then presents
    /// forever:
    ///
    /// ```text
    ///     set_palette(bank 0, entry 0, 0x07E0)   ; the backdrop colour
    /// loop:
    ///     present()                              ; PPU -> default_fb
    ///     vsync_wait()                           ; frame boundary
    ///     j loop
    /// ```
    ///
    /// No tile layer is enabled and no sprite is shown, so every pixel
    /// falls through to the backdrop -- `ppu.rs`'s
    /// `compose_sprite_over_backdrop_with_transparency` documents the
    /// same "no tile layer enabled -> backdrop = bg/fg palette 0 entry 0"
    /// path.
    pub fn green_screen_cart() -> Vec<u8> {
        let body: Vec<u32> = vec![
            addi(A0, 0),               // bank 0 (BANK_BGFG)
            addi(A1, 0),               // entry 0 (the backdrop)
            addi(A2, GREEN as i32),    // colour
            addi(A7, SYS_SET_PALETTE), //
            ECALL,                     //
            addi(A7, SYS_PRESENT),     // loop:
            ECALL,                     //
            addi(A7, SYS_VSYNC_WAIT),  //
            ECALL,                     //
            jal_x0(-16),               // back to `loop`
        ];
        let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();

        // Header layout copied from `ggo_emu_core::cart`'s own
        // `make_cart_flags` test helper.
        let mut h = [0u8; HEADER_LEN];
        h[0x00..0x04].copy_from_slice(&MAGIC);
        h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
        h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
        h[0x08..0x08 + 9].copy_from_slice(b"Green Fix");
        h[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes()); // entry_offset
        h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
        h[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // save_bytes
        h[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // ram_needed
        h[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes()); // flags
        let crc = crc32(&h[0x00..0x3C]);
        h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());

        let mut out = h.to_vec();
        out.extend_from_slice(&body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::green_screen_cart;
    use super::*;

    // ------------------------------------------------------- pacing math

    #[test]
    fn pace_sleep_holds_the_remainder_of_the_frame() {
        assert_eq!(
            pace_sleep(Duration::from_millis(4), FRAME_TIME),
            Some(FRAME_TIME - Duration::from_millis(4))
        );
    }

    #[test]
    fn pace_sleep_does_not_hold_a_late_frame() {
        assert_eq!(pace_sleep(FRAME_TIME, FRAME_TIME), None, "exactly on time");
        assert_eq!(
            pace_sleep(FRAME_TIME * 3, FRAME_TIME),
            None,
            "a frame that took three periods must not sleep at all"
        );
    }

    /// A zero-cost frame sleeps the whole period: 60 fps, not a spin.
    #[test]
    fn pace_sleep_of_an_instant_frame_is_a_full_period() {
        assert_eq!(pace_sleep(Duration::ZERO, FRAME_TIME), Some(FRAME_TIME));
    }

    /// The cadence itself, pinned against `ggo_emu::FRAME_TIME` -- 60 Hz
    /// to the microsecond the standalone binary uses.
    #[test]
    fn frame_time_is_the_standalone_binarys_60hz_period() {
        assert_eq!(FRAME_TIME, Duration::from_micros(16_667));
        let fps = 1.0 / FRAME_TIME.as_secs_f64();
        assert!((fps - 60.0).abs() < 0.01, "{fps} fps");
    }

    // -------------------------------------------------- pixel conversion

    #[test]
    fn rgb565_to_bgra_expands_channels_and_orders_them_bgra() {
        // Pure red, pure green, pure blue, black, white.
        let fb = vec![0xF800u16, 0x07E0, 0x001F, 0x0000, 0xFFFF];
        let out = rgb565_to_bgra(&fb);
        assert_eq!(out.len(), fb.len() * 4);
        assert_eq!(&out[0..4], &[0x00, 0x00, 0xFF, 0xFF], "red -> B,G,R,A");
        assert_eq!(&out[4..8], &[0x00, 0xFF, 0x00, 0xFF], "green");
        assert_eq!(&out[8..12], &[0xFF, 0x00, 0x00, 0xFF], "blue");
        assert_eq!(&out[12..16], &[0x00, 0x00, 0x00, 0xFF], "black");
        assert_eq!(
            &out[16..20],
            &[0xFF, 0xFF, 0xFF, 0xFF],
            "white must reach 0xFF, not 0xF8 -- the high bits replicate"
        );
    }

    #[test]
    fn rgb565_to_bgra_is_always_opaque_and_screen_sized() {
        let out = rgb565_to_bgra(&vec![0x1234u16; (WIDTH * HEIGHT) as usize]);
        assert_eq!(out.len(), (WIDTH * HEIGHT * 4) as usize);
        assert!(out.chunks_exact(4).all(|px| px[3] == 0xFF));
    }

    /// The port, end to end: `start` boots the synthetic cart on the
    /// emulator thread and the panel side receives real, non-blank,
    /// correctly-formatted frames at increasing frame numbers -- then
    /// `Session::stop` actually ends the thread.
    #[test]
    fn start_drives_frames_and_stop_ends_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();

        let (session, rx) = start(path, "green.cart".to_string());

        let mut frames = Vec::new();
        // Five frames at 60 Hz is ~83 ms of pacing; the recv itself
        // blocks, so this can't spin.
        while frames.len() < 5 {
            match rx.recv_blocking().expect("the emulator thread must run") {
                EmuMsg::Frame { bgra, frame } => frames.push((bgra, frame)),
                EmuMsg::Ended(reason) => panic!("the cart must not end: {reason}"),
            }
        }

        for (bgra, _) in &frames {
            assert_eq!(
                bgra.len(),
                (WIDTH * HEIGHT * 4) as usize,
                "one BGRA8 pixel per screen pixel"
            );
            assert!(
                bgra.chunks_exact(4).any(|px| px[..3] != [0, 0, 0]),
                "the composed framebuffer must not be blank"
            );
            // Every pixel is the backdrop the cart set: BGRA of 0x07E0.
            assert!(
                bgra.chunks_exact(4)
                    .all(|px| px == [0x00, 0xFF, 0x00, 0xFF]),
                "every pixel should be the green backdrop the cart set"
            );
        }

        // The cart's own frame counter advances one per presented frame.
        let numbers: Vec<u32> = frames.iter().map(|(_, n)| *n).collect();
        assert!(
            numbers.windows(2).all(|w| w[1] > w[0]),
            "frame numbers must increase: {numbers:?}"
        );

        // Stop: the thread returns at the top of its next turn, drops
        // the core, and with it the sender -- which is what closes the
        // channel. A dangling thread would leave `recv_blocking`
        // yielding frames forever instead.
        session.stop();
        let mut drained = 0;
        while rx.recv_blocking().is_ok() {
            drained += 1;
            assert!(drained < 1000, "the channel never closed after stop()");
        }
    }

    /// A path that isn't a cart fails loudly on the `Ended` channel
    /// rather than silently producing no frames.
    #[test]
    fn a_malformed_cart_ends_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.cart");
        std::fs::write(&path, b"not a cart at all").unwrap();

        let (_session, rx) = start(path, "junk.cart".to_string());
        match rx.recv_blocking().expect("a reason must arrive") {
            EmuMsg::Ended(reason) => assert!(reason.starts_with("cart: "), "{reason}"),
            EmuMsg::Frame { .. } => panic!("junk must not produce a frame"),
        }
    }

    #[test]
    fn a_missing_cart_file_ends_with_a_reason() {
        let (_session, rx) = start("/definitely/not/here.cart".into(), "here.cart".to_string());
        match rx.recv_blocking().expect("a reason must arrive") {
            EmuMsg::Ended(reason) => assert!(reason.contains("here.cart"), "{reason}"),
            EmuMsg::Frame { .. } => panic!("a missing file must not produce a frame"),
        }
    }

    /// Input published through the session reaches the cart's
    /// `poll_buttons` mask. Drives the same atomic the panel's key
    /// handlers write.
    #[test]
    fn input_published_on_the_session_is_visible_to_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();

        let (session, rx) = start(path, "green.cart".to_string());
        // Wait for the run to be genuinely under way before publishing,
        // so the store can't race the thread's construction.
        rx.recv_blocking().unwrap();
        session.set_input(0b1010);
        // The next frame latched it; there is no read-back channel, so
        // the assertion is on the API contract holding without panic
        // plus the run surviving the store.
        match rx.recv_blocking().unwrap() {
            EmuMsg::Frame { .. } => {}
            EmuMsg::Ended(reason) => panic!("publishing input must not end the run: {reason}"),
        }
    }
}
