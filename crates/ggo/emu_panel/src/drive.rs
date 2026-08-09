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
//! here are a `PathBuf`, a channel sender and a handful of `Arc`s; the
//! emulator state is constructed inside the thread and never crosses a
//! boundary. Fixing this upstream would mean a `+ Send` bound in the
//! `ggo` repo, which is out of scope for a fork-side task.
//!
//! ## How a run ends, and where its perf data comes from
//!
//! The perf sim IS enabled (`Peripherals::perf.enable()`, exactly as
//! `ggo-ide`'s `CartStepper::new` does), so every frame the cart presents
//! appends a `FrameRecord` to `p.perf.frames`. On the way out -- cart
//! exit, CPU fault, or the panel's stop flag -- the thread serialises the
//! whole run with `ggo_emu_core::perfsim::perf_json` (the same call
//! `CartStepper::perf_json` makes, same arguments) into a
//! [`PerfSnapshot`], stores it in the shared [`Session`] slot, and
//! returns. [`Session::wait`] joins the thread and hands the panel the
//! snapshot plus the run's diagnostic lines, which is what
//! [`crate::ingest`] writes to `ggo_ide.db`.
//!
//! This is deliberately NOT ggo-ide's `EmuCmd::Snapshot` request/reply
//! round trip. That shape exists because its emu thread is persistent and
//! reused across runs, which is also why its own review had to guard a
//! "snapshot answered by the wrong stepper" race and make the end-of-run
//! flow idempotent per run generation. Here the thread is per-run and
//! terminates, so storing the snapshot on the way out is both simpler and
//! raceless: there is exactly one snapshot per thread, produced by the
//! only stepper that thread ever had.
//!
//! ## What is deliberately not ported
//!
//! - **Audio.** F3 defers it (constraints.md); the standalone binary
//!   stays the with-audio path. The APU still *runs* (`vsync_wait`
//!   advances the mixer inside `ggo_emu_core`), so cart timing and perf
//!   behaviour are unchanged -- nothing drains its sample ring.
//! - **`run_cart`'s wire-stall frame stretch** (`stall_realtime`). The
//!   perf sim's cycle model is recorded, but the pane paces every frame
//!   at [`FRAME_TIME`] rather than stretching a frame that blew its wire
//!   budget. `ggo-ide` makes the same call for the same reason (a UI
//!   pane wants real-time video; the budget overrun is data to plot, not
//!   something to act on).
//! - **Save-file persistence.** The save region exists and
//!   `save_read`/`save_write` work, but `savefile::flush_save` is not
//!   called, so a pane run's saves are in-memory only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ggo_emu_core::cart::Cart;
use ggo_emu_core::cpu::Cpu;
use ggo_emu_core::mmu::{CART_XIP_BASE, DEFAULT_MAIN_RAM_BYTES, DEFAULT_VRAM_BYTES, Mmu};
use ggo_emu_core::peripherals::{Peripherals, SCREEN_HEIGHT, SCREEN_WIDTH};
use ggo_emu_core::run::{FrameEvent, run_until_event};

use crate::uart::UartLog;

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

/// Milliseconds per second, for reporting a frame's emulation cost the
/// way `ggo-ide`'s `FrameMsg::emu_ms` does.
const MILLIS_PER_SEC: f32 = 1_000.0;

/// One presented frame, as the panel receives it.
pub struct Frame {
    /// `WIDTH * HEIGHT * 4` bytes of BGRA8 (gpui's `RenderImage` frame
    /// format).
    pub bgra: Vec<u8>,
    /// The cart's own frame counter.
    pub number: u32,
    /// Wall-clock cost of emulating this frame plus converting its
    /// pixels, in milliseconds -- the pacing hold is deliberately NOT
    /// included, so this measures the emulator, not the clock.
    /// `ggo-ide`'s `run_loop` times exactly the same span for its
    /// `emu_ms`.
    pub step_ms: f32,
}

/// The perf half of a finished run: what [`crate::ingest`] writes.
#[derive(Debug, Clone, PartialEq)]
pub struct PerfSnapshot {
    /// The perf-JSON `cart` identity -- the cart header's own title,
    /// matching `ggo-ide`'s `CartStepper::perf_json` (which passes
    /// `cart.header.title` with no prefix), so a cart profiled from
    /// either tool lands on the same `cart` row.
    pub cart: String,
    /// `ggo_emu_core::perfsim::perf_json`'s output for the whole run.
    pub perf_json: String,
    /// Frames the perf sim actually recorded. Zero means the cart never
    /// reached a single `vsync_wait`, which is `ggo-ide`'s "no frames
    /// recorded" case -- nothing worth ingesting.
    pub frames: u64,
}

/// What the emulator thread leaves behind when it returns.
#[derive(Debug, Clone, PartialEq)]
struct RunOutcome {
    reason: String,
    /// `None` when the run never built a core at all (unreadable file,
    /// unparseable cart) -- there is no perf sim to serialise.
    perf: Option<PerfSnapshot>,
}

/// A run that has ended, as [`Session::wait`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedRun {
    /// Human-readable end-of-run line for the pane's status.
    pub reason: String,
    pub perf: Option<PerfSnapshot>,
    /// The run's diagnostic lines, ingested into the `uart` table (see
    /// [`crate::uart`] for what does and does not reach this in cart
    /// mode).
    pub uart: Vec<String>,
}

/// The panel's handle on a running emulator thread.
pub struct Session {
    /// The cart this session is running (rel path, for the header and for
    /// the ingested `run.label`).
    pub cart: String,
    /// Latest button mask, published by the panel's key handlers and
    /// latched by the thread at each frame boundary. An atomic rather
    /// than a channel because input is level-triggered state, not a
    /// stream: the cart only ever asks "what is held right now".
    input: Arc<AtomicU32>,
    /// Checked once per driver turn. Set by [`Self::stop`] / `Drop`.
    stop: Arc<AtomicBool>,
    /// The run's diagnostic log, shared with the emulator thread. Cloned
    /// out by the panel so the console survives the session.
    uart: UartLog,
    /// Filled in by the thread immediately before it returns.
    outcome: Arc<Mutex<Option<RunOutcome>>>,
    /// `None` only after [`Self::wait`] has taken it.
    join: Option<JoinHandle<()>>,
}

impl Session {
    /// Signal the thread to stop. Deliberately does NOT join: a turn can
    /// take up to [`PER_TURN_BUDGET`] instructions, and blocking the UI
    /// thread on it would stall the whole window. The thread checks the
    /// flag at the top of its next turn, stores its outcome, and returns.
    /// [`Self::wait`] -- which the panel only ever calls from a
    /// background thread -- is what actually collects it.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn set_input(&self, mask: u32) {
        self.input.store(mask, Ordering::Release);
    }

    /// The live diagnostic log, for the pane's console.
    pub fn uart(&self) -> &UartLog {
        &self.uart
    }

    /// Signal the run to stop and BLOCK until the thread has exited,
    /// returning everything the end-of-run ingest needs.
    ///
    /// Blocking is the point: the caller gets a snapshot that is
    /// guaranteed complete (including the terminal diagnostic line the
    /// thread writes on its way out), with no timeout to tune and no
    /// "did it answer yet" polling. The wait is bounded by one driver
    /// turn. The panel runs this inside `cx.background_spawn`, never on
    /// the UI thread -- the same rule `ggo_charts_panel::loader` follows
    /// for its blocking db reads.
    pub fn wait(mut self) -> FinishedRun {
        self.stop();
        if let Some(join) = self.join.take() {
            // A panicked emulator thread must not poison the panel: an
            // `Err` here just means there is no outcome to read, which
            // the `unwrap_or_else` below already handles.
            let _ = join.join();
        }
        let outcome = self.outcome.lock().unwrap().take();
        let (reason, perf) = match outcome {
            Some(o) => (o.reason, o.perf),
            None => ("the emulator thread ended unexpectedly".to_string(), None),
        };
        FinishedRun {
            reason,
            perf,
            uart: self.uart.lines(),
        }
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
/// to a surface. The thread NEVER blocks on this channel, so a panel that
/// drops the receiver can never wedge it.
///
/// The receiver closing is also how the panel learns a run ended on its
/// own: the thread drops the sender as it returns, which ends the panel's
/// pump loop. There is no terminal message on the wire.
pub fn start(cart_path: PathBuf, cart: String) -> (Session, async_channel::Receiver<Frame>) {
    let (tx, rx) = async_channel::bounded(1);
    let input = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let uart = UartLog::new();
    let outcome: Arc<Mutex<Option<RunOutcome>>> = Arc::new(Mutex::new(None));

    uart.push_line(format!("[run] {cart}"));

    let join = {
        let (input, stop, uart, outcome) =
            (input.clone(), stop.clone(), uart.clone(), outcome.clone());
        std::thread::Builder::new()
            .name("ggo-emu-panel".into())
            .spawn(move || {
                let result = run(&cart_path, &tx, &input, &stop, &uart);
                uart.push_line(format!("[run ended] {}", result.reason));
                *outcome.lock().unwrap() = Some(result);
            })
            .expect("spawning the ggo emulator thread")
    };

    let session = Session {
        cart,
        input,
        stop,
        uart,
        outcome,
        join: Some(join),
    };
    (session, rx)
}

/// The drive loop itself -- `run_cart`'s body minus the window, the
/// audio device and the save flush.
fn run(
    cart_path: &Path,
    tx: &async_channel::Sender<Frame>,
    input: &AtomicU32,
    stop: &AtomicBool,
    uart: &UartLog,
) -> RunOutcome {
    let bytes = match std::fs::read(cart_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return RunOutcome {
                reason: format!("{}: {e}", cart_path.display()),
                perf: None,
            };
        }
    };
    let cart = match Cart::parse(&bytes) {
        Ok(cart) => cart,
        Err(e) => {
            // Mirrors `CartStepper::drain_uart`'s one synthetic line: a
            // cart that failed to load must say why in the console, not
            // just vanish behind a status word.
            uart.push_line(format!("[cart load failed] {e}"));
            return RunOutcome {
                reason: format!("cart: {e}"),
                perf: None,
            };
        }
    };

    let title = cart.header.title.clone();
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
    // The cache + wire perf sim, enabled exactly as `ggo-ide`'s
    // `CartStepper::new` enables it (and as the browser/native cart runs
    // do by default). Cart mode keeps `PerfSim`'s default cache base
    // (`CART_XIP_BASE`) -- only the full-system boot path moves it.
    p.perf.enable();
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

    let reason = loop {
        if stop.load(Ordering::Acquire) {
            break "stopped".to_string();
        }
        let turn_started = Instant::now();
        let (event, _insns) = run_until_event(&mut cpu, &mut mmu, &mut p, PER_TURN_BUDGET, false);
        match event {
            // Frame boundary: the cart drew a complete frame and called
            // vsync_wait. Publish it, pace, then latch input --
            // `run_cart`'s Vsync arm, minus the present and the stall.
            FrameEvent::Vsync(number) => {
                p.set_ticks_ms(start.elapsed().as_millis().min(u32::MAX as u128) as u32);
                let bgra = rgb565_to_bgra(&p.default_fb);
                let step_ms = turn_started.elapsed().as_secs_f32() * MILLIS_PER_SEC;
                // Full channel = the UI hasn't drained the previous
                // frame yet; drop this one. Closed = the panel dropped
                // the receiver (Stop, or the panel itself went away).
                if let Err(e) = tx.try_send(Frame {
                    bgra,
                    number,
                    step_ms,
                }) && e.is_closed()
                {
                    break "stopped".to_string();
                }
                if let Some(hold) = pace_sleep(last_present.elapsed(), FRAME_TIME) {
                    std::thread::sleep(hold);
                }
                last_present = Instant::now();
                // AFTER the hold, not before it. `native::refresh_input`
                // runs after `Display::present` has already slept out the
                // rest of the period, so the cart sees the pad as it was
                // at the START of the frame it is about to run, not as it
                // was ~16 ms earlier. Latching before the sleep costs a
                // whole frame of input latency.
                p.input_mask = input.load(Ordering::Acquire);
            }
            // Budget exhausted mid-frame: the framebuffer is half-drawn,
            // so do NOT publish it (`run_cart` likewise refuses to
            // present a partial buffer). Latch input and go round again;
            // the `stop` check at the top of the loop is what keeps this
            // interruptible for a cart that never reaches vsync_wait.
            FrameEvent::Budget => {
                p.input_mask = input.load(Ordering::Acquire);
            }
            FrameEvent::Exit(code) => break format!("cart exited with {code}"),
            FrameEvent::Fault(trap) => break format!("cpu fault: {trap:?}"),
        }
    };

    RunOutcome {
        reason,
        perf: Some(PerfSnapshot {
            cart: title,
            // `ggo-ide`'s `CartStepper::perf_json`, argument for
            // argument. `idump`/`ddump` are `None` for the same reason it
            // gives: function-level I$/D$ attribution needs the cart's
            // companion ELF and the `ggo-emu` `profile` module's
            // DWARF/addr2line tooling, which lives one level above
            // `ggo-emu-core` and is not a dependency here. The perf JSON
            // simply omits the optional `"profile"`/`"dprofile"`
            // sections, which `ingest::parse_output` treats as "no rows",
            // not an error.
            perf_json: ggo_emu_core::perfsim::perf_json(
                &cart.header.title,
                &p.perf.frames,
                p.perf.wire_wait,
                None,
                None,
            ),
            frames: p.perf.frames.len() as u64,
        }),
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
    // `emd`. So the drive test assembles its own ten-instruction cart
    // instead -- real `GGOC` header, real RV32I machine code, driven
    // through the real `drive::start` thread. It is a genuine end-to-end
    // check of the port (header parse -> XIP map -> interpret -> ecall
    // dispatch -> PPU compose -> RGB565 -> BGRA -> channel), just of a
    // cart small enough to write by hand.

    use ggo_emu_core::cart::{HEADER_LEN, MAGIC, SUPPORTED_HEADER_VERSION};
    use ggo_emu_core::crc32::crc32;

    /// The cart header title `green_screen_cart` stamps -- also the
    /// perf-JSON `cart` identity, and therefore the `cart.name` row an
    /// ingested run of it lands on.
    pub const GREEN_CART_TITLE: &str = "Green Fix";

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
        h[0x08..0x08 + GREEN_CART_TITLE.len()].copy_from_slice(GREEN_CART_TITLE.as_bytes());
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

/// Helpers other modules' tests drive a real run through.
#[cfg(test)]
pub mod tests_support {
    use super::*;

    /// Run the green fixture cart until `frames` frames have arrived,
    /// then stop it and return the finished run -- perf snapshot and all.
    /// Used by `crate::ingest`'s tests to ingest genuinely-emitted perf
    /// JSON rather than a hand-written imitation of it.
    pub fn run_green_cart_briefly(frames: usize) -> FinishedRun {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, fixture::green_screen_cart()).unwrap();

        let (session, rx) = start(path, "green.cart".to_string());
        for _ in 0..frames {
            rx.recv_blocking().expect("the emulator thread must run");
        }
        // Drop the receiver first so the thread can never sit on a full
        // channel while `wait` joins it.
        drop(rx);
        session.wait()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{GREEN_CART_TITLE, green_screen_cart};
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

    // ------------------------------------------------------ the run loop

    /// The port, end to end: `start` boots the synthetic cart on the
    /// emulator thread and the panel side receives real, non-blank,
    /// correctly-formatted frames at increasing frame numbers -- then
    /// `Session::wait` actually ends the thread.
    #[test]
    fn start_drives_frames_and_wait_ends_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();

        let (session, rx) = start(path, "green.cart".to_string());

        let mut frames = Vec::new();
        // Five frames at 60 Hz is ~83 ms of pacing; the recv itself
        // blocks, so this can't spin.
        while frames.len() < 5 {
            let frame = rx.recv_blocking().expect("the emulator thread must run");
            frames.push(frame);
        }

        for frame in &frames {
            assert_eq!(
                frame.bgra.len(),
                (WIDTH * HEIGHT * 4) as usize,
                "one BGRA8 pixel per screen pixel"
            );
            assert!(
                frame.bgra.chunks_exact(4).any(|px| px[..3] != [0, 0, 0]),
                "the composed framebuffer must not be blank"
            );
            // Every pixel is the backdrop the cart set: BGRA of 0x07E0.
            assert!(
                frame
                    .bgra
                    .chunks_exact(4)
                    .all(|px| px == [0x00, 0xFF, 0x00, 0xFF]),
                "every pixel should be the green backdrop the cart set"
            );
            assert!(
                frame.step_ms >= 0.0 && frame.step_ms < 1_000.0,
                "step cost must be a sane millisecond figure, got {}",
                frame.step_ms
            );
        }

        // The cart's own frame counter advances one per presented frame.
        let numbers: Vec<u32> = frames.iter().map(|f| f.number).collect();
        assert!(
            numbers.windows(2).all(|w| w[1] > w[0]),
            "frame numbers must increase: {numbers:?}"
        );

        // Stop: the thread returns at the top of its next turn, stores its
        // outcome, and drops the core. A dangling thread would leave
        // `wait` blocked forever instead.
        drop(rx);
        let finished = session.wait();
        assert_eq!(finished.reason, "stopped");
    }

    /// The perf half: a stopped run carries a real `perfsim::perf_json`
    /// snapshot identified by the cart header's own title, with one
    /// recorded frame per presented frame.
    #[test]
    fn a_stopped_run_carries_a_perf_snapshot_and_its_diagnostics() {
        let finished = tests_support::run_green_cart_briefly(4);
        assert_eq!(finished.reason, "stopped");

        let perf = finished.perf.expect("a run that started has a perf sim");
        assert_eq!(perf.cart, GREEN_CART_TITLE);
        assert!(
            perf.frames >= 4,
            "the perf sim records one frame per vsync, got {}",
            perf.frames
        );
        assert!(
            perf.perf_json
                .contains(&format!("\"cart\":\"{GREEN_CART_TITLE}\"")),
            "{}",
            &perf.perf_json[..perf.perf_json.len().min(120)]
        );
        assert!(perf.perf_json.contains("\"frames\":{"));

        assert_eq!(
            finished.uart.first().map(String::as_str),
            Some("[run] green.cart"),
            "the console opens with the run marker"
        );
        assert_eq!(
            finished.uart.last().map(String::as_str),
            Some("[run ended] stopped"),
            "and closes with the terminal reason"
        );
    }

    /// A path that isn't a cart fails loudly -- in the outcome AND in the
    /// console -- rather than silently producing no frames.
    #[test]
    fn a_malformed_cart_ends_with_a_reason_and_no_perf_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.cart");
        std::fs::write(&path, b"not a cart at all").unwrap();

        let (session, rx) = start(path, "junk.cart".to_string());
        assert!(
            rx.recv_blocking().is_err(),
            "junk must not produce a frame; the channel just closes"
        );
        let finished = session.wait();
        assert!(finished.reason.starts_with("cart: "), "{}", finished.reason);
        assert!(
            finished.perf.is_none(),
            "there is no perf sim for a cart that never loaded -- nothing to ingest"
        );
        assert!(
            finished
                .uart
                .iter()
                .any(|l| l.starts_with("[cart load failed] ")),
            "{:?}",
            finished.uart
        );
    }

    #[test]
    fn a_missing_cart_file_ends_with_a_reason() {
        let (session, rx) = start("/definitely/not/here.cart".into(), "here.cart".to_string());
        assert!(rx.recv_blocking().is_err());
        let finished = session.wait();
        assert!(finished.reason.contains("here.cart"), "{}", finished.reason);
        assert!(finished.perf.is_none());
    }

    /// A cart that exits reports the exit code -- and still hands back
    /// whatever perf frames it managed, which is what an ingest wants.
    #[test]
    fn an_exiting_cart_reports_its_code() {
        use ggo_emu_core::cart::{HEADER_LEN, MAGIC, SUPPORTED_HEADER_VERSION};
        use ggo_emu_core::crc32::crc32;

        // `ebreak`: `run.rs` maps `Trap::Ebreak` to `FrameEvent::Exit(0)`.
        let body = 0x0010_0073u32.to_le_bytes();
        let mut h = [0u8; HEADER_LEN];
        h[0x00..0x04].copy_from_slice(&MAGIC);
        h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
        h[0x08..0x08 + 4].copy_from_slice(b"Quit");
        h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
        let crc = crc32(&h[0x00..0x3C]);
        h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());
        let mut image = h.to_vec();
        image.extend_from_slice(&body);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quit.cart");
        std::fs::write(&path, image).unwrap();

        let (session, rx) = start(path, "quit.cart".to_string());
        // Let the run reach its own terminus first. `wait` sets the stop
        // flag before it joins (it is the "finish this run" call, not a
        // passive read), so racing it against the cart would report
        // "stopped" instead of the exit -- exactly what the panel avoids
        // by only calling it once the frame channel has closed.
        assert!(
            rx.recv_blocking().is_err(),
            "a cart that exits on its first instruction presents no frame"
        );
        let finished = session.wait();
        assert_eq!(finished.reason, "cart exited with 0");
        let perf = finished.perf.expect("the core was built, so perf exists");
        assert_eq!(perf.cart, "Quit");
        assert_eq!(
            perf.frames, 0,
            "a cart that never reached vsync recorded no frames -- ggo-ide's \
             'nothing to upload' case"
        );
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
        assert!(
            rx.recv_blocking().is_ok(),
            "publishing input must not end the run"
        );
    }
}
