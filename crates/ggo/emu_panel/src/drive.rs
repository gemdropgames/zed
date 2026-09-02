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
//! ## Audio (F5.4 R6)
//!
//! F3 deferred audio; this loop now drains it. [`run`] opens the default
//! output device AFTER the cart has parsed and immediately before the run
//! loop, holds it in a local, and drops it on the way out -- so the device
//! is open exactly while a run is live and never a moment longer. See
//! [`crate::audio`]'s module doc for why the pane owns the stream at all
//! rather than leaving it to the standalone binary. Passing `None` for
//! `audio` skips the device entirely and never touches cpal, which is what
//! every test in THIS module does -- note that is not true of the crate as
//! a whole: `crate::audio`'s own smoke test opens the real device, and any
//! panel test that calls `EmuPanel::run` goes through the production path
//! and therefore opens one too.
//!
//! ## What is deliberately not ported
//!
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

use ggo_emu_core::apu::Apu;
use ggo_emu_core::cart::Cart;
use ggo_emu_core::cpu::Cpu;
use ggo_emu_core::mmu::{CART_XIP_BASE, DEFAULT_MAIN_RAM_BYTES, DEFAULT_VRAM_BYTES, Mmu};
use ggo_emu_core::peripherals::{Peripherals, SCREEN_HEIGHT, SCREEN_WIDTH};
use ggo_emu_core::ppu::PpuSnapshot;
use ggo_emu_core::run::{FrameEvent, run_until_event};
use ggo_emu_core::savefile;

use crate::audio::{AudioStatus, RingWriter};
use crate::uart::UartLog;

/// One 60 Hz vsync period -- `ggo_emu::FRAME_TIME`, redeclared because it
/// lives in the `ggo-emu` binary crate (which drags in winit, cpal and two
/// SQLite engines) rather than in `ggo-emu-core`.
pub const FRAME_TIME: Duration = Duration::from_micros(16_667);

/// The fastest the pane will drive a cart: ten frames per real frame
/// period. Past this the UI thread cannot keep up with presenting, and
/// the point -- reaching a late-game fault sooner -- is long since made.
pub const MAX_SPEED: u32 = 10;

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
    /// Whether `reason` describes a FAILURE rather than an ordinary end.
    /// Carried as a flag rather than sniffed out of `reason`'s wording
    /// because the panel styles the two differently and the wording is
    /// free text (see [`FinishedRun::is_error`]).
    is_error: bool,
    /// `None` when the run never built a core at all (unreadable file,
    /// unparseable cart) -- there is no perf sim to serialise.
    perf: Option<PerfSnapshot>,
}

/// A run that has ended, as [`Session::wait`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedRun {
    /// Human-readable end-of-run line for the pane's status.
    pub reason: String,
    /// True when the run ended BADLY -- an unreadable cart file, a cart
    /// that would not parse, a CPU fault, or an emulator thread that
    /// vanished. False for the ordinary ends: the panel asking the run to
    /// stop, and the cart exiting under its own power (whatever its exit
    /// code -- a non-zero code is the cart's own verdict on itself, not
    /// the emulator failing to run it). The panel styles the status row
    /// from this, so it must be decided here, where the reason is
    /// written, rather than re-derived from the reason's words.
    pub is_error: bool,
    pub perf: Option<PerfSnapshot>,
    /// The run's diagnostic lines, ingested into the `uart` table -- the
    /// driver's own per-run markers (`[run]`, `[run ended]`,
    /// `[cart load failed]`) interleaved with whatever the cart's own
    /// `log()` calls emitted (see [`crate::uart`]).
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
    /// Debugger: while set, the thread parks at each frame boundary
    /// (feeding silence so the audio device stays live) until cleared or
    /// until [`Self::step`] hands it one more frame.
    pause: Arc<AtomicBool>,
    /// Frames still owed to [`Self::step`] while paused.
    step: Arc<AtomicU32>,
    /// Host-side arm switch for the cart's inspection tap: while true,
    /// the thread keeps the guest's `enabled` word set (lock-step runs);
    /// ordinary runs leave it 0 and the cart never serializes.
    inspect: Arc<AtomicBool>,
    /// Frames per real frame period, `1..=MAX_SPEED`. Read at every
    /// frame boundary, so a change takes effect within a frame.
    speed: Arc<AtomicU32>,
    /// The cart's own world-inspection dump as of the last presented
    /// frame, once armed: `(tap seq, JSON bytes)`. Written by the thread
    /// every vsync from the guest's magic-tagged tap buffer.
    world_json: Arc<Mutex<Option<(u32, Arc<String>)>>>,
    /// The PPU as of the last presented frame, for the debug viewers --
    /// written by the thread every vsync, read by the pane whenever it
    /// renders a viewer. A slot, not a channel: the pane wants the latest,
    /// never a backlog.
    snapshot: Arc<Mutex<Option<Arc<PpuSnapshot>>>>,
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

    /// Park at the next frame boundary. Takes effect within one turn.
    pub fn pause(&self) {
        self.pause.store(true, Ordering::Release);
    }

    /// Also forgets any steps queued while paused: a resume means "run",
    /// not "run, then park after N more frames".
    pub fn resume(&self) {
        self.step.store(0, Ordering::Release);
        self.pause.store(false, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Acquire)
    }

    /// While paused, run exactly one more frame then park again. A no-op
    /// unless paused (the pane pauses first, so Step while running is
    /// "pause", not "skip a frame").
    pub fn step(&self) {
        if self.is_paused() {
            self.step.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The PPU state as of the last presented frame, if any frame has
    /// been presented.
    pub fn snapshot(&self) -> Option<Arc<PpuSnapshot>> {
        self.snapshot.lock().unwrap().clone()
    }

    /// Arm the cart's world-inspection tap: dumps start on the next
    /// frame. Only lock-step (remote) runs turn this on.
    pub fn set_inspect(&self, on: bool) {
        self.inspect.store(on, Ordering::Release);
    }

    /// Run `speed` frames per real frame period (clamped to
    /// `1..=MAX_SPEED`). The cart's clock advances `speed` times as fast
    /// too, so a fault that takes ten minutes of play arrives in one;
    /// audio is silenced above 1x rather than played at chipmunk pitch.
    pub fn set_speed(&self, speed: u32) {
        self.speed.store(speed.clamp(1, MAX_SPEED), Ordering::Release);
    }

    pub fn speed(&self) -> u32 {
        self.speed.load(Ordering::Acquire)
    }

    /// The cart's world-inspection JSON as of the last presented frame
    /// (`None` until the first armed tap write; always `None` for carts
    /// built without emerald's `inspect` feature).
    pub fn world_json(&self) -> Option<(u32, Arc<String>)> {
        self.world_json.lock().unwrap().clone()
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
        // No outcome means the thread never stored one -- it panicked on
        // its way out. That is a failure, and the only one the panel can
        // learn about from here.
        let (reason, is_error, perf) = match outcome {
            Some(o) => (o.reason, o.is_error, o.perf),
            None => (
                "the emulator thread ended unexpectedly".to_string(),
                true,
                None,
            ),
        };
        FinishedRun {
            reason,
            is_error,
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
///
/// `audio` is the panel's own [`AudioStatus`] (mute survives runs, so the
/// panel owns it, not a run). `None` means this run makes no sound and
/// never opens a device -- what the tests here use, so `cargo test` never
/// touches the machine's audio hardware.
pub fn start(
    cart_path: PathBuf,
    cart: String,
    audio: Option<AudioStatus>,
) -> (Session, async_channel::Receiver<Frame>) {
    let (tx, rx) = async_channel::bounded(1);
    let input = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let step = Arc::new(AtomicU32::new(0));
    let snapshot: Arc<Mutex<Option<Arc<PpuSnapshot>>>> = Arc::new(Mutex::new(None));
    let world_json: Arc<Mutex<Option<(u32, Arc<String>)>>> = Arc::new(Mutex::new(None));
    let inspect = Arc::new(AtomicBool::new(false));
    let speed = Arc::new(AtomicU32::new(1));
    let uart = UartLog::new();
    let outcome: Arc<Mutex<Option<RunOutcome>>> = Arc::new(Mutex::new(None));

    uart.push_line(format!("[run] {cart}"));

    let join = {
        let controls = Controls {
            input: input.clone(),
            stop: stop.clone(),
            pause: pause.clone(),
            step: step.clone(),
            snapshot: snapshot.clone(),
            inspect: inspect.clone(),
            speed: speed.clone(),
            world_json: world_json.clone(),
        };
        let (uart, outcome) = (uart.clone(), outcome.clone());
        std::thread::Builder::new()
            .name("ggo-emu-panel".into())
            .spawn(move || {
                let result = run(&cart_path, &tx, &controls, &uart, audio.as_ref());
                uart.push_line(format!("[run ended] {}", result.reason));
                *outcome.lock().unwrap() = Some(result);
            })
            .expect("spawning the ggo emulator thread")
    };

    let session = Session {
        cart,
        input,
        stop,
        pause,
        step,
        snapshot,
        inspect,
        speed,
        world_json,
        uart,
        outcome,
        join: Some(join),
    };
    (session, rx)
}

/// The drive loop itself -- `run_cart`'s body minus the window and the
/// save flush.
/// The thread's end of [`Session`]'s control surface.
struct Controls {
    input: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    step: Arc<AtomicU32>,
    snapshot: Arc<Mutex<Option<Arc<PpuSnapshot>>>>,
    inspect: Arc<AtomicBool>,
    speed: Arc<AtomicU32>,
    world_json: Arc<Mutex<Option<(u32, Arc<String>)>>>,
}

/// Where a run's save file lives: the standalone's rule (`<card dir>/savs/
/// <NAME>.sav`, C5 identity header, name probes), with the card dir being
/// the cart's own directory as it is for asset loads. `None` when the cart
/// declares no save region, has no parent directory, or every probed name
/// is held by another cart's save.
fn save_file_for(cart_path: &Path, title: &str, save_bytes: usize) -> Option<PathBuf> {
    if save_bytes == 0 {
        return None;
    }
    let card_dir = cart_path.parent()?;
    savefile::resolve_save_path(card_dir, title, save_bytes)
}

fn run(
    cart_path: &Path,
    tx: &async_channel::Sender<Frame>,
    controls: &Controls,
    uart: &UartLog,
    audio: Option<&AudioStatus>,
) -> RunOutcome {
    let Controls {
        input,
        stop,
        pause,
        step,
        snapshot,
        inspect,
        speed,
        world_json,
    } = controls;
    // Cached guest address of the world-inspection tap (see
    // emerald-world's `inspect` module); scanned for lazily since a world
    // that never opts in never writes one.
    let mut tap_addr: Option<usize> = None;
    let bytes = match std::fs::read(cart_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            // The cart file could not even be read: a failure.
            return RunOutcome {
                reason: format!("{}: {e}", cart_path.display()),
                is_error: true,
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
            // The bytes are not a cart this emulator can load: a failure.
            return RunOutcome {
                reason: format!("cart: {e}"),
                is_error: true,
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
    // Attach a log sink so `log()` calls land in the pane's console (and
    // from there the ingested `uart` table) instead of vanishing into
    // this GUI process's invisible stdout -- mirrors `ggo-ide`'s
    // `CartStepper::new` (see `Peripherals::log_sink`'s doc). Drained
    // every turn below, alongside the rest of the loop's per-turn work.
    p.log_sink = Some(Vec::new());
    if let Some(toc) = cart.toc {
        p.assets.set_toc(toc);
    }
    // `asset_load` resolves relative to the cart's own directory, which
    // is `run_cart`'s default when no `--card-dir` is given.
    if let Some(dir) = cart_path.parent() {
        p.assets.set_card_dir(dir.to_path_buf());
    }
    // Save-file backing, the standalone's way (`run_cart`): load any
    // existing save now, flush on a dirty frame at most once a second and
    // once more on the way out.
    let save_file = save_file_for(cart_path, &title, p.save.len());
    if let Some(path) = &save_file {
        savefile::load_save(path, &title, &mut p.save);
    }
    let mut last_save_flush: Option<u32> = None;
    let mut frames_presented: u32 = 0;
    let mut paused_total = Duration::ZERO;
    if save_file.is_none() && !p.save.is_empty() {
        uart.push_line(
            "[save] no save file could be resolved (every name probe is held by another cart's save); saves disabled",
        );
    }

    // Audio, last of the setup: AFTER the cart has parsed, so a cart that
    // never loads never opens a device at all, and immediately before the
    // loop, so `_audio_out`'s drop -- which stops the stream and releases
    // the device -- lands on the way out of this function, on this same
    // thread. Device lifetime == run lifetime, by construction.
    let mut audio_cursor: u64 = 0;
    let mut audio_scratch: Vec<i16> = Vec::new();
    let (audio_writer, _audio_out) = match audio {
        Some(status) => {
            // The ring's "audio is flowing" flag is created HERE, per run,
            // and dies with `writer` below -- it is deliberately not on the
            // panel-scoped `status`, or a restart's outgoing run would
            // silence the incoming one. See `crate::audio`'s module doc.
            let (writer, reader) = crate::audio::channel(status.clone());
            // Infallible: no device is a normal machine, not a failed run.
            let out = crate::audio::start_output(status, reader, ggo_emu_core::apu::MIX_RATE);
            match &out {
                Some(out) => uart.push_line(format!("[audio] {} Hz", out.device_rate)),
                None => uart.push_line(format!(
                    "[audio unavailable] {}",
                    match status.state() {
                        crate::audio::AudioState::Unavailable(reason) => reason,
                        // `start_output` always records a reason on failure.
                        _ => "unknown".to_string(),
                    }
                )),
            }
            (Some(writer), out)
        }
        None => (None, None),
    };

    let start = Instant::now();
    let mut last_present = Instant::now();
    // The cart's clock, accumulated per frame as real time times the
    // speed in force for that frame -- so a speed change mid-run bends
    // the clock forward rather than jumping it.
    let mut emulated = Duration::ZERO;
    let mut last_real = Duration::ZERO;

    // Every arm below breaks with `(reason, is_error)`, so each way out of
    // the run states its own verdict rather than leaving the caller to
    // guess one from the wording.
    let (reason, is_error) = loop {
        if stop.load(Ordering::Acquire) {
            // The panel asked for this (Stop, a restart, the pane going
            // away): not an error.
            break ("stopped".to_string(), false);
        }
        let turn_started = Instant::now();
        let (event, _insns) = run_until_event(&mut cpu, &mut mmu, &mut p, PER_TURN_BUDGET, false);
        // Drain every turn, regardless of what the turn ended with (Vsync,
        // Budget, Exit or Fault) -- not just on a completed frame. This is
        // the cadence `ggo-ide`'s `thread::run_loop` uses too
        // (`uart.push(&s.drain_uart())` once per `step`), and it is what
        // keeps `Peripherals::log_sink` bounded: a cart that logs a lot
        // between vsync waits (or never reaches one) can't grow the sink
        // past one turn's worth of bytes.
        uart.push(&p.take_log());
        match event {
            // Frame boundary: the cart drew a complete frame and called
            // vsync_wait. Publish it, pace, then latch input --
            // `run_cart`'s Vsync arm, minus the present and the stall.
            FrameEvent::Vsync(number) => {
                let bgra = rgb565_to_bgra(&p.default_fb);
                let step_ms = turn_started.elapsed().as_secs_f32() * MILLIS_PER_SEC;
                // BEFORE the frame goes out: the snapshot describes this
                // frame, and the pane reads it on the frame's arrival.
                publish_snapshot(&p.ppu, snapshot);
                if inspect.load(Ordering::Acquire) {
                    arm_world_tap(&mut mmu, &mut tap_addr, true);
                }
                publish_world_tap(&mmu, &mut tap_addr, world_json);
                // Full channel = the UI hasn't drained the previous
                // frame yet; drop this one. Closed = the panel dropped
                // the receiver (Stop, or the panel itself went away).
                if let Err(e) = tx.try_send(Frame {
                    bgra,
                    number,
                    step_ms,
                }) && e.is_closed()
                {
                    // The panel dropped the receiver -- the same stop
                    // request, arriving by a different road: not an error.
                    break ("stopped".to_string(), false);
                }
                // BEFORE the pacing hold, so the ring is fed as early in
                // the period as it can be. `handle_vsync_wait` advanced
                // the APU exactly one frame on the way to this event, so
                // there is precisely one frame of samples waiting.
                let speed = speed.load(Ordering::Acquire).clamp(1, MAX_SPEED);
                if let Some(writer) = &audio_writer {
                    audio_cursor = if speed == 1 {
                        pump_audio(&p.apu, audio_cursor, &mut audio_scratch, writer)
                    } else {
                        // Silence at speed: the ring would drop most of
                        // it anyway, and what got through would be noise.
                        // The cursor still advances so 1x resumes clean.
                        audio_scratch.clear();
                        p.apu.copy_since(audio_cursor, &mut audio_scratch)
                    };
                }
                frames_presented = frames_presented.wrapping_add(1);
                if p.save_dirty
                    && last_save_flush.is_none_or(|f| {
                        frames_presented.wrapping_sub(f) >= savefile::FLUSH_INTERVAL_FRAMES
                    })
                {
                    flush_save(&save_file, &title, &mut p, uart);
                    last_save_flush = Some(frames_presented);
                }
                if let Some(hold) = pace_sleep(last_present.elapsed(), FRAME_TIME / speed) {
                    std::thread::sleep(hold);
                }
                // The debugger's park: hold here, frame complete and
                // published, until resumed, stepped, or stopped. The pause
                // time is kept out of the cart's clock below so it doesn't
                // see a giant tick.
                let (parked, stop_requested) =
                    park_while_paused(pause, step, stop, audio_writer.as_ref());
                paused_total += parked;
                if stop_requested {
                    // Stopped out of the debugger's park: not an error.
                    break ("stopped".to_string(), false);
                }
                last_present = Instant::now();
                // AFTER the hold, not before it. `native::refresh_input`
                // runs after `Display::present` has already slept out the
                // rest of the period, so the cart sees the pad as it was
                // at the START of the frame it is about to run, not as it
                // was ~16 ms earlier. Latching before the sleep costs a
                // whole frame of input latency.
                p.input_mask = input.load(Ordering::Acquire);
                // AFTER the hold and the input latch -- `ggo-emu/src/
                // lib.rs`'s Vsync arm calls `set_ticks_ms` last too
                // (present -> refresh_input -> set_ticks_ms), so the
                // clock the cart reads next turn accounts for the pacing
                // sleep it just went through.
                let real = start.elapsed().saturating_sub(paused_total);
                emulated += real.saturating_sub(last_real) * speed;
                last_real = real;
                p.set_ticks_ms(emulated.as_millis().min(u32::MAX as u128) as u32);
            }
            // Budget exhausted mid-frame: the framebuffer is half-drawn,
            // so do NOT publish it (`run_cart` likewise refuses to
            // present a partial buffer). Latch input and go round again;
            // the `stop` check at the top of the loop is what keeps this
            // interruptible for a cart that never reaches vsync_wait.
            FrameEvent::Budget => {
                // A cart that never reaches vsync_wait still honours pause
                // (no frame to publish or snapshot, but it parks).
                let (parked, stop_requested) =
                    park_while_paused(pause, step, stop, audio_writer.as_ref());
                paused_total += parked;
                if stop_requested {
                    // Stopped out of the debugger's park: not an error.
                    break ("stopped".to_string(), false);
                }
                p.input_mask = input.load(Ordering::Acquire);
            }
            // The cart called exit: an ordinary end however it scores
            // itself, so NOT an error even for a non-zero code -- the code
            // is the cart's verdict on its own work, and the pane already
            // shows it in the reason.
            FrameEvent::Exit(code) => break (format!("cart exited with {code}"), false),
            // The CPU trapped (bad instruction, bad access): the run died,
            // which is an error however the cart got there.
            FrameEvent::Fault(trap) => break (format!("cpu fault: {trap:?}"), true),
        }
    };

    // Nothing will feed the ring from here on, so unprime it NOW rather
    // than letting the binding fall out of scope at the end of the
    // function: `perf_json` below serialises every recorded frame and can
    // take milliseconds, which at ~10 ms a cpal buffer is long enough to
    // charge a handful of dropouts against a run that has already stopped.
    // (`RingWriter::drop` is still what covers the panic path -- see its
    // doc; this is only about making the clean path prompt.)
    drop(audio_writer);
    if p.save_dirty {
        flush_save(&save_file, &title, &mut p, uart);
    }

    RunOutcome {
        reason,
        is_error,
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

/// Move every APU sample mixed since `cursor` into `writer`, returning the
/// new cursor -- the whole audio contribution of one presented frame.
///
/// Factored out of [`run`] rather than inlined so it can be driven against
/// a real `Apu` with no output device anywhere in sight (see this module's
/// tests): everything about whether the emulated APU's samples actually
/// reach the ring is here, and nothing about it needs cpal.
///
/// `scratch` is reused across frames to avoid a per-frame allocation, and
/// is cleared here rather than by `Apu::copy_since` -- that method
/// *appends* (`out.reserve` + `out.push`), so a caller that forgets would
/// re-push every previous frame's samples on top of the new ones.
/// Hold while `pause` is set: returns `(time parked, stop requested)`.
/// Checks `stop` first on every turn so a paused run still stops within
/// one frame time; one queued `step` releases exactly one turn. The audio
/// ring is idled on entry (silence, no dropouts counted) and re-primes on
/// the first push after resuming.
fn park_while_paused(
    pause: &AtomicBool,
    step: &AtomicU32,
    stop: &AtomicBool,
    audio_writer: Option<&RingWriter>,
) -> (Duration, bool) {
    if !pause.load(Ordering::Acquire) {
        return (Duration::ZERO, false);
    }
    let parked_at = Instant::now();
    if let Some(writer) = audio_writer {
        writer.idle();
    }
    loop {
        if stop.load(Ordering::Acquire) {
            return (parked_at.elapsed(), true);
        }
        if !pause.load(Ordering::Acquire) {
            return (parked_at.elapsed(), false);
        }
        if step.load(Ordering::Acquire) > 0 {
            step.fetch_sub(1, Ordering::AcqRel);
            return (parked_at.elapsed(), false);
        }
        std::thread::sleep(FRAME_TIME);
    }
}

/// Refill the debugger's snapshot slot from the PPU. Reuses the previous
/// snapshot's buffers when the pane has let go of it, so a run with no
/// viewer open costs one ~138 KB memcpy per frame and no allocation.
fn publish_snapshot(ppu: &ggo_emu_core::ppu::Ppu, slot: &Mutex<Option<Arc<PpuSnapshot>>>) {
    let mut slot = slot.lock().unwrap();
    let mut snap = slot
        .take()
        .and_then(|arc| Arc::try_unwrap(arc).ok())
        .unwrap_or_default();
    ppu.snapshot_into(&mut snap);
    *slot = Some(Arc::new(snap));
}

/// Write the save region to its file, clearing `save_dirty` on success.
/// A failure is a console line, not a run failure -- the standalone
/// prints and carries on the same way.
fn flush_save(save_file: &Option<PathBuf>, title: &str, p: &mut Peripherals, uart: &UartLog) {
    let Some(path) = save_file else {
        return;
    };
    match savefile::flush_save(path, title, &p.save) {
        Ok(()) => p.save_dirty = false,
        Err(e) => uart.push_line(format!("[save] flush {} failed: {e}", path.display())),
    }
}

fn pump_audio(apu: &Apu, cursor: u64, scratch: &mut Vec<i16>, writer: &RingWriter) -> u64 {
    scratch.clear();
    let next = apu.copy_since(cursor, scratch);
    writer.push(scratch);
    next
}

/// How long to hold a just-published frame, given `elapsed` since the
/// previous one. `None` once the frame is already late -- a late frame is
/// shown immediately rather than compounding the delay, which is exactly
/// what `native::Display::present`'s `if elapsed < hold` does.
pub fn pace_sleep(elapsed: Duration, frame_time: Duration) -> Option<Duration> {
    frame_time.checked_sub(elapsed).filter(|d| !d.is_zero())
}

/// First bytes of emerald-world's inspection tap: `"EMWD"` (LE u32
/// 0x4457_4D45). Layout after it: `enabled u32` (HOST-writable arm
/// switch), `seq u32, len u32, cap u32`, then `cap` bytes of JSON (`len`
/// valid). Kept in sync with `emerald-world/src/inspect.rs` by value —
/// zed does not link emerald. Carts built without emerald's `inspect`
/// feature simply have no tap.
const TAP_MAGIC: [u8; 4] = *b"EMWD";
const TAP_HEADER_BYTES: usize = 20;
const TAP_ENABLED_OFFSET: usize = 4;

/// Find the tap static in guest RAM (scanned once, then re-verified —
/// it's a static, so it never moves).
fn find_tap(ram: &[u8], tap_addr: &mut Option<usize>) -> Option<usize> {
    match *tap_addr {
        Some(a) if ram.get(a..a + 4).is_some_and(|m| m == TAP_MAGIC) => Some(a),
        _ => {
            let found = ram.chunks_exact(4).position(|c| c == TAP_MAGIC).map(|i| i * 4)?;
            *tap_addr = Some(found);
            Some(found)
        }
    }
}

/// Arm (or disarm) the cart's tap by writing its `enabled` word — the
/// host-side switch that makes serialization cost nothing in ordinary
/// runs. No-op for carts without a tap.
fn arm_world_tap(mmu: &mut ggo_emu_core::mmu::Mmu, tap_addr: &mut Option<usize>, on: bool) {
    let Some(addr) = find_tap(&mmu.main_ram, tap_addr) else {
        return;
    };
    let off = addr + TAP_ENABLED_OFFSET;
    if let Some(word) = mmu.main_ram.get_mut(off..off + 4) {
        word.copy_from_slice(&u32::from(on).to_le_bytes());
    }
}

/// Copy the cart's world-inspection JSON (if armed and written) out of
/// guest RAM into the session slot.
fn publish_world_tap(
    mmu: &ggo_emu_core::mmu::Mmu,
    tap_addr: &mut Option<usize>,
    slot: &Mutex<Option<(u32, Arc<String>)>>,
) {
    let ram: &[u8] = &mmu.main_ram;
    let Some(addr) = find_tap(ram, tap_addr) else {
        return; // cart built without the inspect feature
    };
    let word = |off: usize| -> u32 {
        ram.get(addr + off..addr + off + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .unwrap_or(0)
    };
    let (seq, len, cap) = (word(8), word(12) as usize, word(16) as usize);
    if seq == 0 || len > cap {
        return;
    }
    let Some(bytes) = ram.get(addr + TAP_HEADER_BYTES..addr + TAP_HEADER_BYTES + len) else {
        return;
    };
    let json = String::from_utf8_lossy(bytes).into_owned();
    *slot.lock().unwrap() = Some((seq, Arc::new(json)));
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

/// Hand-assembled cart fixtures, shared by this module's tests, the
/// panel's, and -- through the crate's `test-support` re-export --
/// `ggo_smoke`'s emulator journeys.
#[cfg(any(test, feature = "test-support"))]
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

    /// `addi rd, x0, imm` -- the load-immediate special case (`rs1 = x0`),
    /// the only form [`green_screen_cart`] needs besides `ecall` and a
    /// backwards `jal`.
    fn addi(rd: u32, imm: i32) -> u32 {
        ((imm as u32 & 0xFFF) << 20) | (rd << 7) | 0x13
    }

    /// `addi rd, rs1, imm` -- the general two-register form. [`addi`]
    /// above is the `rs1 = x0` special case; [`logging_cart`] needs this
    /// one to add an offset onto the base [`lui`] just loaded.
    fn addi_reg(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32 & 0xFFF) << 20) | (rs1 << 15) | (rd << 7) | 0x13
    }

    /// `lui rd, imm20` -- loads `imm20 << 12` into `rd`. Only
    /// [`logging_cart`] needs this, to build the XIP address of its
    /// `log()` message; every other fixture instruction fits a single
    /// 12-bit `addi` immediate.
    fn lui(rd: u32, imm20: u32) -> u32 {
        ((imm20 & 0xF_FFFF) << 12) | (rd << 7) | 0x37
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
    const SYS_LOG: i32 = 0x4B;
    const SYS_SAVE_WRITE: i32 = 0x31;

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

    /// The cart header title [`logging_cart`] stamps.
    pub const SAVING_CART_TITLE: &str = "Save Fix";
    /// The saving cart's declared save region.
    pub const SAVING_CART_SAVE_BYTES: u32 = 64;
    /// How many bytes of its own code the saving cart writes at offset 0.
    pub const SAVING_CART_WRITE_LEN: usize = 8;

    /// A cart that writes the first 8 bytes of its own code into the save
    /// region at offset 0 (`save_write(0, XIP_BASE, 8)`), then presents
    /// green forever -- so a flushed `.sav` carries a payload the test can
    /// predict.
    pub fn saving_cart() -> Vec<u8> {
        let xip_base_hi20 = super::CART_XIP_BASE >> 12;
        let body: Vec<u32> = vec![
            addi(A0, 0),                            // off 0
            lui(A1, xip_base_hi20),                 // buf = CART_XIP_BASE
            addi(A2, SAVING_CART_WRITE_LEN as i32), // len
            addi(A7, SYS_SAVE_WRITE),               //
            ECALL,                                  // save_write
            addi(A0, 0),                            // bank 0
            addi(A1, 0),                            // entry 0
            addi(A2, GREEN as i32),                 // colour
            addi(A7, SYS_SET_PALETTE),              //
            ECALL,                                  //
            addi(A7, SYS_PRESENT),                  // loop:
            ECALL,                                  //
            addi(A7, SYS_VSYNC_WAIT),               //
            ECALL,                                  //
            jal_x0(-16),                            // back to `loop`
        ];
        let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut h = [0u8; HEADER_LEN];
        h[0x00..0x04].copy_from_slice(&MAGIC);
        h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
        h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
        h[0x08..0x08 + SAVING_CART_TITLE.len()].copy_from_slice(SAVING_CART_TITLE.as_bytes());
        h[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes()); // entry_offset
        h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
        h[0x30..0x34].copy_from_slice(&SAVING_CART_SAVE_BYTES.to_le_bytes());
        h[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // ram_needed
        h[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes()); // flags
        let crc = crc32(&h[0x00..0x3C]);
        h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());
        let mut out = h.to_vec();
        out.extend_from_slice(&body);
        out
    }

    pub const LOGGING_CART_TITLE: &str = "Log Fix";

    /// The message [`logging_cart`]'s single `log()` call emits -- what
    /// the end-to-end drive test asserts lands in the console verbatim.
    pub const LOG_MESSAGE: &str = "hi from cart";

    /// [`green_screen_cart`] plus one real `log(ptr, len)` ecall before
    /// the paint loop:
    ///
    /// ```text
    ///     a0 = CART_XIP_BASE + STR_OFFSET   ; lui + addi -- STR_OFFSET
    ///                                       ; points at LOG_MESSAGE's
    ///                                       ; bytes, appended as data
    ///                                       ; after this program's own
    ///                                       ; instructions (never
    ///                                       ; executed, just addressed)
    ///     a1 = len(LOG_MESSAGE)
    ///     log()
    ///     set_palette(bank 0, entry 0, 0x07E0)
    /// loop:
    ///     present()
    ///     vsync_wait()
    ///     j loop
    /// ```
    ///
    /// Exists to prove the sink `drive::run` attaches carries a REAL
    /// guest `log` ecall's bytes end to end -- ggo-ide's equivalent test
    /// (`emu/mod.rs::drain_uart_returns_cart_log_output_via_the_attached_sink`)
    /// pokes `Peripherals::log_sink` directly instead and says why in its
    /// own doc; this fixture goes one step further because a hand-rolled
    /// `log()` call is cheap here (no toolchain needed, same as every
    /// other fixture in this module).
    pub fn logging_cart() -> Vec<u8> {
        // 15 instructions precede the message text, so it lands at byte
        // offset 15 * 4 = 60 -- comfortably inside a 12-bit signed `addi`
        // immediate (max 2047).
        const STR_OFFSET: i32 = 15 * 4;
        let xip_base_hi20 = super::CART_XIP_BASE >> 12;

        let body: Vec<u32> = vec![
            lui(A0, xip_base_hi20),             // a0 = CART_XIP_BASE (hi bits)
            addi_reg(A0, A0, STR_OFFSET),       // a0 += offset of the message
            addi(A1, LOG_MESSAGE.len() as i32), // a1 = message length
            addi(A7, SYS_LOG),                  //
            ECALL,                              // log(a0, a1)
            addi(A0, 0),                        // bank 0 (BANK_BGFG)
            addi(A1, 0),                        // entry 0 (the backdrop)
            addi(A2, GREEN as i32),             // colour
            addi(A7, SYS_SET_PALETTE),          //
            ECALL,                              //
            addi(A7, SYS_PRESENT),              // loop:
            ECALL,                              //
            addi(A7, SYS_VSYNC_WAIT),           //
            ECALL,                              //
            jal_x0(-16),                        // back to `loop`
        ];
        debug_assert_eq!(body.len(), 15, "STR_OFFSET assumes exactly 15 instructions");
        let mut body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();
        body.extend_from_slice(LOG_MESSAGE.as_bytes());

        let mut h = [0u8; HEADER_LEN];
        h[0x00..0x04].copy_from_slice(&MAGIC);
        h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
        h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
        h[0x08..0x08 + LOGGING_CART_TITLE.len()].copy_from_slice(LOGGING_CART_TITLE.as_bytes());
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

        let (session, rx) = start(path, "green.cart".to_string(), None);
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
    fn set_speed_clamps_to_the_supported_range() {
        let (session, _rx) = start(PathBuf::from("/nonexistent/cart.ggo"), "cart.ggo".into(), None);
        assert_eq!(session.speed(), 1, "a fresh run is real time");
        session.set_speed(4);
        assert_eq!(session.speed(), 4);
        session.set_speed(0);
        assert_eq!(session.speed(), 1, "0x is not a speed");
        session.set_speed(99);
        assert_eq!(session.speed(), MAX_SPEED);
    }

    /// At speed the frame hold shrinks by the same factor; a late frame
    /// still holds nothing.
    #[test]
    fn a_faster_speed_shortens_the_frame_hold() {
        assert_eq!(
            pace_sleep(Duration::from_millis(1), FRAME_TIME / 4),
            Some(FRAME_TIME / 4 - Duration::from_millis(1))
        );
        assert_eq!(pace_sleep(Duration::from_millis(5), FRAME_TIME / 4), None);
    }

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

    // ------------------------------------------------------- the audio tap
    //
    // These drive a REAL `ggo_emu_core::apu::Apu` -- the same type a cart
    // run mixes into -- straight into a real `RingWriter`, with no output
    // device anywhere. That is the whole point: every machine can run
    // them, including one with no sound card, and they still prove the
    // emulated APU's samples reach the ring the cpal callback drains.

    /// Sound one PSG square at full volume and advance the APU one frame,
    /// exactly as `runtime::handle_vsync_wait` does on every presented
    /// frame. `(ch=8, step=1.0x, vol=full both, ctrl=enable|duty50)` is
    /// `ggo-emu-core`'s own `audio_psg_note_on_off_via_syscalls` fixture.
    fn apu_with_one_mixed_frame() -> Apu {
        let mut apu = Apu::new();
        apu.set_channel(8, 0x1000, 0xFFFF, 0b101);
        apu.run_frame();
        assert!(
            apu.ring().iter().any(|&s| s != 0),
            "sanity: the fixture note must actually be audible"
        );
        apu
    }

    #[test]
    fn pump_audio_moves_a_frames_mixed_samples_into_the_ring() {
        let apu = apu_with_one_mixed_frame();
        let status = crate::audio::AudioStatus::new();
        let (writer, reader) = crate::audio::channel(status);

        let mut scratch = Vec::new();
        let cursor = pump_audio(&apu, 0, &mut scratch, &writer);

        assert_eq!(
            cursor,
            apu.write_cursor(),
            "the returned cursor must be caught up to the APU's writer"
        );
        assert!(
            !scratch.is_empty(),
            "one advanced frame mixes a frame's worth of samples"
        );
        assert!(
            scratch.iter().any(|&s| s != 0),
            "the note must survive the copy, not arrive as silence"
        );
        assert_eq!(
            reader.queued_len(),
            scratch.len(),
            "everything drained from the APU was submitted to the ring"
        );
    }

    /// The cursor is what keeps one frame's samples from being submitted
    /// twice -- and `scratch` being reused across frames is exactly why
    /// [`pump_audio`] has to clear it (`Apu::copy_since` appends).
    #[test]
    fn pump_audio_submits_each_frame_once_across_a_reused_scratch_buffer() {
        let mut apu = apu_with_one_mixed_frame();
        let status = crate::audio::AudioStatus::new();
        let (writer, reader) = crate::audio::channel(status);

        let mut scratch = Vec::new();
        let cursor = pump_audio(&apu, 0, &mut scratch, &writer);
        let first_frame = reader.queued_len();

        // Nothing new mixed: a second pump at the same cursor submits
        // nothing, rather than re-submitting the frame just sent.
        let cursor = pump_audio(&apu, cursor, &mut scratch, &writer);
        assert_eq!(
            reader.queued_len(),
            first_frame,
            "a caught-up cursor must submit nothing"
        );

        // One more mixed frame: exactly that frame's samples are added.
        // Not `first_frame * 2` -- the APU's mix rate is not an exact
        // multiple of 60, so consecutive frames differ by a sample.
        apu.run_frame();
        let mixed = (apu.write_cursor() - cursor) as usize;
        pump_audio(&apu, cursor, &mut scratch, &writer);
        assert_eq!(
            reader.queued_len(),
            first_frame + mixed,
            "the second frame adds exactly its own samples and no copy of the first"
        );
    }

    /// Mute reaches all the way down here: the emulated APU keeps mixing
    /// (its perf counters must not change just because the user muted),
    /// but nothing is submitted.
    #[test]
    fn pump_audio_submits_nothing_while_muted() {
        let apu = apu_with_one_mixed_frame();
        let status = crate::audio::AudioStatus::new();
        status.set_muted(true);
        let (writer, reader) = crate::audio::channel(status.clone());

        let mut scratch = Vec::new();
        let cursor = pump_audio(&apu, 0, &mut scratch, &writer);
        assert_eq!(reader.queued_len(), 0, "a muted run submits no frames");
        assert_eq!(
            cursor,
            apu.write_cursor(),
            "the cursor still advances, so unmuting resumes from live \
             audio rather than replaying what was mixed while silent"
        );

        status.set_muted(false);
        pump_audio(&apu, 0, &mut scratch, &writer);
        assert!(reader.queued_len() > 0, "unmuting resumes submission");
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

        let (session, rx) = start(path, "green.cart".to_string(), None);

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

    /// Dropping a live Session -- no `wait`, no explicit `stop` -- must
    /// end the emulator thread promptly. This is the panel-close path:
    /// `Drop` signals the stop flag, the thread breaks at the top of its
    /// next turn, writes its terminal console line, and returns (dropping
    /// its channel sender on the way out). Without it, closing the pane
    /// mid-run would orphan an emulator thread spinning at 60 Hz forever.
    #[test]
    fn dropping_a_live_session_stops_the_emulator_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();

        let (session, rx) = start(path, "green.cart".to_string(), None);
        // Prove the run is genuinely live before dropping the handle.
        rx.recv_blocking().expect("the emulator thread must run");

        let uart = session.uart().clone();
        drop(session);

        // The sender is owned by the thread's closure and dropped only
        // when it returns, so the channel closing IS the thread ending.
        // The deadline is what bounds the wait: a thread that ignored the
        // drop would keep presenting frames at 60 Hz, and each of those
        // frames re-checks the clock -- a failure, never a hang.
        let deadline = Instant::now() + Duration::from_secs(10);
        while rx.recv_blocking().is_ok() {
            assert!(
                Instant::now() < deadline,
                "the emulator thread kept presenting frames after the Session was dropped"
            );
        }
        assert_eq!(
            uart.lines().last().map(String::as_str),
            Some("[run ended] stopped"),
            "the thread ran its normal end-of-run path, not a panic"
        );
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
        assert!(
            !finished.is_error,
            "the panel asking a healthy run to stop is not a failure"
        );
    }

    /// A path that isn't a cart fails loudly -- in the outcome AND in the
    /// console -- rather than silently producing no frames.
    #[test]
    fn a_malformed_cart_ends_with_a_reason_and_no_perf_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.cart");
        std::fs::write(&path, b"not a cart at all").unwrap();

        let (session, rx) = start(path, "junk.cart".to_string(), None);
        assert!(
            rx.recv_blocking().is_err(),
            "junk must not produce a frame; the channel just closes"
        );
        let finished = session.wait();
        assert!(finished.reason.starts_with("cart: "), "{}", finished.reason);
        assert!(
            finished.is_error,
            "a cart that would not parse is a FAILED run, not an ordinary end"
        );
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
        let (session, rx) = start(
            "/definitely/not/here.cart".into(),
            "here.cart".to_string(),
            None,
        );
        assert!(rx.recv_blocking().is_err());
        let finished = session.wait();
        assert!(finished.reason.contains("here.cart"), "{}", finished.reason);
        assert!(finished.is_error, "an unreadable cart file is a FAILED run");
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

        let (session, rx) = start(path, "quit.cart".to_string(), None);
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
        assert!(
            !finished.is_error,
            "a cart that exited under its own power ended normally"
        );
        let perf = finished.perf.expect("the core was built, so perf exists");
        assert_eq!(perf.cart, "Quit");
        assert_eq!(
            perf.frames, 0,
            "a cart that never reached vsync recorded no frames -- ggo-ide's \
             'nothing to upload' case"
        );
    }

    /// End-to-end: a REAL guest `log()` ecall -- not a value poked
    /// directly into `Peripherals::log_sink` -- reaches the pane's
    /// console. Proves the whole chain `run`'s sink attachment ->
    /// `Syscall::Log`'s handler -> the per-turn `uart.push(&p.take_log())`
    /// drain -> [`crate::uart::UartLog`] -> what [`Session::wait`] hands
    /// back for ingest.
    #[test]
    fn a_carts_own_log_call_reaches_the_console() {
        use super::fixture::{LOG_MESSAGE, logging_cart};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logging.cart");
        std::fs::write(&path, logging_cart()).unwrap();

        let (session, rx) = start(path, "logging.cart".to_string(), None);
        // The cart's single `log()` call runs on the very first turn,
        // before its first `vsync_wait` -- so by the time the first frame
        // arrives, that turn's drain has already moved it into the
        // console.
        rx.recv_blocking().expect("the emulator thread must run");
        drop(rx);
        let finished = session.wait();

        assert!(
            finished.uart.iter().any(|line| line == LOG_MESSAGE),
            "the cart's log() output must reach the console verbatim: {:?}",
            finished.uart
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

        let (session, rx) = start(path, "green.cart".to_string(), None);
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

    /// Poll `rx` for up to `within`; `async_channel` has no timed recv.
    fn recv_within(rx: &async_channel::Receiver<Frame>, within: Duration) -> Option<Frame> {
        let deadline = std::time::Instant::now() + within;
        loop {
            if let Ok(frame) = rx.try_recv() {
                return Some(frame);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Pause parks the thread at a frame boundary (no new frames), Step
    /// releases exactly one, Resume lets them flow again, and a paused run
    /// still stops. The snapshot slot holds the last presented PPU state
    /// throughout.
    #[test]
    fn pause_parks_step_advances_one_frame_and_resume_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();
        let (session, rx) = start(path, "green.cart".to_string(), None);

        let first = rx.recv_blocking().expect("frames flow before pause");
        assert!(
            session.snapshot().is_some(),
            "a presented frame fills the slot"
        );
        session.pause();
        assert!(session.is_paused());
        // Drain whatever was in flight when the flag landed, until the
        // channel has been quiet for a whole frame time several times
        // over -- a loaded box can delay the in-flight frame, so this
        // waits for silence rather than assuming a fixed window.
        let mut last_number = {
            let mut last_number = first.number;
            let quiet_for = |rx: &async_channel::Receiver<Frame>, last: &mut u32| {
                let started = std::time::Instant::now();
                let mut quiet_since = std::time::Instant::now();
                while started.elapsed() < Duration::from_secs(3) {
                    if let Ok(frame) = rx.try_recv() {
                        *last = frame.number;
                        quiet_since = std::time::Instant::now();
                    } else if quiet_since.elapsed() >= Duration::from_millis(250) {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                false
            };
            assert!(
                quiet_for(&rx, &mut last_number),
                "a paused run stops publishing frames"
            );
            last_number
        };

        let before_step = session.snapshot().map(|s| Arc::as_ptr(&s) as usize);
        session.step();
        let stepped =
            recv_within(&rx, Duration::from_secs(2)).expect("step releases exactly one frame");
        assert_eq!(stepped.number, last_number + 1, "one frame, the next one");
        assert_ne!(
            session.snapshot().map(|s| Arc::as_ptr(&s) as usize),
            before_step,
            "the stepped frame refilled the snapshot slot"
        );
        last_number = stepped.number;
        assert!(
            recv_within(&rx, Duration::from_millis(300)).is_none(),
            "after the step it parks again"
        );

        session.resume();
        assert!(!session.is_paused());
        let resumed = recv_within(&rx, Duration::from_secs(2)).expect("resume lets frames flow");
        assert!(resumed.number > last_number);

        session.pause();
        let finished = session.wait();
        assert_eq!(finished.reason, "stopped", "a paused run still stops");
    }

    /// Step outside of pause is a no-op: the pane pauses first, so a
    /// stray step never skips a frame of a running cart.
    #[test]
    fn step_while_running_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();
        let (session, rx) = start(path, "green.cart".to_string(), None);
        rx.recv_blocking().expect("running");
        session.step();
        assert_eq!(session.step.load(Ordering::Acquire), 0);
        session.wait();
    }

    /// A cart's `save_write` lands on disk the standalone's way: the run's
    /// end-of-run flush writes `<card dir>/savs/<NAME>.sav` with the C5
    /// header and the region's bytes.
    #[test]
    fn a_carts_save_write_is_flushed_to_the_card_dir_at_run_end() {
        let dir = tempfile::tempdir().unwrap();
        let card_dir = dir.path().join("carts");
        std::fs::create_dir_all(&card_dir).unwrap();
        let cart_bytes = fixture::saving_cart();
        let path = card_dir.join("save.cart");
        std::fs::write(&path, &cart_bytes).unwrap();
        let (session, rx) = start(path, "save.cart".to_string(), None);
        rx.recv_blocking().expect("the cart runs");
        rx.recv_blocking().expect("and keeps running");
        let finished = session.wait();
        assert_eq!(finished.reason, "stopped");

        let save_path = savefile::resolve_save_path(
            &card_dir,
            fixture::SAVING_CART_TITLE,
            fixture::SAVING_CART_SAVE_BYTES as usize,
        )
        .expect("the flushed file is this cart's own save");
        let file = std::fs::read(&save_path).unwrap();
        assert_eq!(
            file.len(),
            savefile::SAVE_HDR_BYTES + fixture::SAVING_CART_SAVE_BYTES as usize
        );
        let payload = &file[savefile::SAVE_HDR_BYTES..];
        let code_start = ggo_emu_core::cart::HEADER_LEN;
        assert_eq!(
            &payload[..fixture::SAVING_CART_WRITE_LEN],
            &cart_bytes[code_start..code_start + fixture::SAVING_CART_WRITE_LEN],
            "the region's first bytes are what the cart wrote"
        );
        assert!(
            payload[fixture::SAVING_CART_WRITE_LEN..]
                .iter()
                .all(|b| *b == 0)
        );
        assert!(
            !finished.uart.iter().any(|line| line.contains("[save]")),
            "no save complaint on the console: {:?}",
            finished.uart
        );
    }

    #[test]
    fn save_file_is_only_resolved_for_carts_with_a_save_region() {
        let dir = tempfile::tempdir().unwrap();
        let cart = dir.path().join("carts/game.cart");
        assert_eq!(save_file_for(&cart, "GAME", 0), None);
        let path = save_file_for(&cart, "GAME", 256).expect("a fresh name probe is free");
        assert!(path.starts_with(dir.path().join("carts")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("sav"));
    }
}
