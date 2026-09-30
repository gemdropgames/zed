//! The core drive loop, ported from `ggo-emu/src/lib.rs::run_cart` +
//! `ggo-emu/src/native.rs`. Nothing here emulates anything -- the cart runs
//! inside the wasm emulator module (`ggo_emu_wasm::WasmEmu`), and every
//! step is a call into it, in the same order the standalone binary uses.
//!
//! ## Why an OS thread and not `cx.background_spawn`
//!
//! A `WasmEmu` owns a wasmtime store, and a turn can run for up to
//! 5 million instructions (the module's per-turn budget), so the drive loop is blocking work
//! with a per-frame pacing sleep. A plain `std::thread::spawn` keeps it off
//! the executors entirely; the emulator instance is constructed inside the
//! thread and never crosses a boundary.
//!
//! ## How a run ends, and where its perf data comes from
//!
//! The module's perf sim is always on (as `ggo-ide`'s `CartStepper::new`
//! enables it), so every frame the cart presents is recorded. On the way
//! out -- cart exit, CPU fault, or the panel's stop flag -- the thread
//! asks the module for the whole run's perf JSON (`perf_json`, the same
//! document `CartStepper::perf_json` produces) and stores it in a
//! [`PerfSnapshot`] in the shared [`Session`] slot, then returns. [`Session::wait`] joins the thread and hands the panel the
//! snapshot plus the run's diagnostic lines, which is what
//! [`crate::ingest`] writes to the database.
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
//! - **Save-file persistence beyond the standalone's rule.** A run loads its
//!   save file at start, flushes on a dirty frame at most once a second and
//!   once more on the way out, exactly as `run_cart` does -- see
//!   [`save_file_for`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ggo_common::{LinkEndpoint, ViewerState};
use ggo_emu_abi::{MIX_RATE, PpuSnapshot, SCREEN_HEIGHT, SCREEN_WIDTH};
use ggo_emu_wasm::{LoadedEmulator, TurnEvent, WasmEmu};

use crate::audio::{AudioStatus, RingWriter};
use crate::uart::UartLog;

/// The reason a run ends when the world panel asked it to
/// ([`ggo_common::LinkEndpoint::request_stop`]). Shared with the pane,
/// which writes the same words when it acts on the request at a frame
/// boundary instead -- the world view must not read two different
/// accounts of the one thing it asked for.
pub const WORLD_PANEL_STOP: &str = "stopped by the world panel";

/// One 60 Hz vsync period -- `ggo_emu::FRAME_TIME`, redeclared because it
/// lives in the `ggo-emu` binary crate (which drags in winit and cpal)
/// rather than in `ggo-emu-core`.
pub const FRAME_TIME: Duration = Duration::from_micros(16_667);

/// The fastest the pane will drive a cart: ten frames per real frame
/// period. Past this the UI thread cannot keep up with presenting, and
/// the point -- reaching a late-game fault sooner -- is long since made.
pub const MAX_SPEED: u32 = 10;

/// Framebuffer geometry, re-exported so the panel doesn't have to depend
/// on `ggo_emu_abi` directly.
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
    /// The emulator module's perf JSON for the whole run.
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
    /// Cleared by [`Self::release_link`]; read by the emulator thread.
    link_owned: Arc<AtomicBool>,
    /// `None` only after [`Self::wait`] has taken it.
    join: Option<JoinHandle<()>>,
}

impl Session {
    /// Signal the thread to stop. Deliberately does NOT join: a turn can
    /// take up to 5 million instructions (the module's per-turn budget), and blocking the UI
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
        self.step_by(1);
    }

    /// [`Self::step`] for `frames` frames: ONE add, so a long lock-step
    /// step costs the foreground thread a single atomic rather than a
    /// loop of them.
    pub fn step_by(&self, frames: u32) {
        if self.is_paused() {
            self.step.fetch_add(frames, Ordering::AcqRel);
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
    /// `1..=MAX_SPEED`). Each frame is a whole frame of cart execution
    /// with a whole vsync period on the cart's clock -- the same frames
    /// the board would run, arriving `speed` times sooner -- never one
    /// frame with a stretched delta. Audio is silenced above 1x rather
    /// than played at chipmunk pitch.
    pub fn set_speed(&self, speed: u32) {
        self.speed
            .store(speed.clamp(1, MAX_SPEED), Ordering::Release);
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

    /// Hand this run's viewer link over: from here it neither pumps the
    /// endpoint nor reports its own ending through it.
    ///
    /// A world view keeps ONE endpoint across rebuilds, so the run being
    /// stopped for a rebuild and the run being built for it share it.
    /// Without this the old thread's terminal `Stopped` -- written
    /// whenever it gets round to noticing the stop flag, which is after
    /// the rebuild has already said `Building` -- would leave the world
    /// view reading the previous run's stop reason for the length of the
    /// build, and its dying pump would eat outbound messages meant for
    /// the new run.
    pub fn release_link(&self) {
        self.link_owned.store(false, Ordering::Release);
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

/// The emulator a run should start on. Tests and `ggo_smoke` never install
/// an `EmuRuntime` (loading one compiles the module off-thread, per test),
/// so when no runtime exists at all they get the shared bundled module; an
/// installed runtime is always consulted, so its failures stay testable.
pub fn current_emulator(cx: &gpui::App) -> anyhow::Result<Arc<LoadedEmulator>> {
    #[cfg(any(test, feature = "test-support"))]
    if ggo_emu_wasm::EmuRuntime::global(cx).is_none() {
        return Ok(tests_support::test_emulator());
    }
    ggo_emu_wasm::current_emulator(cx)
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
    emulator: Arc<LoadedEmulator>,
    cart_path: PathBuf,
    cart: String,
    audio: Option<AudioStatus>,
    link: Option<Arc<LinkEndpoint>>,
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
    let link_owned = Arc::new(AtomicBool::new(true));

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
            link,
            link_owned: link_owned.clone(),
        };
        let (uart, outcome) = (uart.clone(), outcome.clone());
        std::thread::Builder::new()
            .name("ggo-emu-panel".into())
            .spawn(move || {
                let result = run(&emulator, &cart_path, &tx, &controls, &uart, audio.as_ref());
                uart.push_line(format!("[run ended] {}", result.reason));
                // Here rather than inside `run`, so it covers every way
                // out: the loop's own breaks AND the setup failures that
                // return before the loop (unreadable file, unparsable
                // cart). A viewer whose cart never started must see
                // `Stopped`, not a `Building` that never resolves.
                if let Some(link) = &controls.link
                    && controls.link_owned.load(Ordering::Acquire)
                {
                    link.set_state(ViewerState::Stopped(result.reason.clone()));
                }
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
        link_owned,
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
    /// The viewer link, when this run is a world view's viewer cart.
    link: Option<Arc<LinkEndpoint>>,
    /// Is [`Self::link`] still THIS run's to speak through? Cleared by
    /// [`Session::release_link`] when the panel hands the same endpoint
    /// to a new run; the thread then neither pumps it nor reports its own
    /// ending through it. See that method for why.
    link_owned: Arc<AtomicBool>,
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
    ggo_savefile::resolve_save_path(card_dir, title, save_bytes)
}

fn run(
    emulator: &LoadedEmulator,
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
        link,
        link_owned,
    } = controls;
    // Holds a frame that arrived split across two frame boundaries, so it
    // must outlive the loop -- see `crate::link`.
    let mut link_reader = ggo_comm::MessageReader::default();
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

    // Same wall-clock RNG seed `run_cart` uses, so successive runs of the
    // same cart differ but a single run stays deterministic once started.
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // The card dir is the cart's own directory, so `asset_load` resolves
    // against it as it does for `run_cart`. Only the cart's own TOC sizes
    // the asset pools: neighbouring files in the folder must not change
    // the arena this cart is granted.
    let mut emu = match emulator.start_cart(&bytes, seed, cart_path.parent().map(Path::to_path_buf))
    {
        Ok(emu) => emu,
        Err(error) => {
            // Mirrors `CartStepper::drain_uart`'s one synthetic line: a
            // cart that failed to load must say why in the console, not
            // just vanish behind a status word.
            uart.push_line(format!("[cart load failed] {error}"));
            // The bytes are not a cart this emulator can load: a failure.
            return RunOutcome {
                reason: format!("cart: {error}"),
                is_error: true,
                perf: None,
            };
        }
    };
    let info = emu.info().clone();
    // The arena is per-cart, so its size is no longer assumable from the
    // outside -- report it the way `run_cart`'s banner does.
    uart.push_line(format!(
        "[ram] arena {} KiB, vram pool {} KiB, ram pool {} KiB",
        info.arena_len / 1024,
        info.vram_pool / 1024,
        info.ram_pool / 1024,
    ));
    if info.body_truncated {
        uart.push_line("[cart] body is larger than the code window; truncated");
    }

    // Save-file backing, the standalone's way (`run_cart`): load any
    // existing save now, flush on a dirty frame at most once a second and
    // once more on the way out.
    let save_file = save_file_for(cart_path, &info.title, info.save_bytes);
    if let Some(path) = &save_file {
        let mut save = vec![0u8; info.save_bytes];
        ggo_savefile::load_save(path, &info.title, &mut save);
        if let Err(error) = emu.write_save(&save) {
            uart.push_line(format!("[save] {error}"));
        }
    }
    let mut last_save_flush: Option<u32> = None;
    let mut frames_presented: u32 = 0;
    let mut paused_total = Duration::ZERO;
    if save_file.is_none() && info.save_bytes > 0 {
        uart.push_line(
            "[save] no save file could be resolved (every name probe is held by another cart's save); saves disabled",
        );
    }
    let mut input_mask: u32 = 0;
    let mut ticks_ms: u32 = 0;

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
            let out = crate::audio::start_output(status, reader, MIX_RATE);
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
    // The cart's clock. At 1x it follows real time, as the standalone
    // emulator's does. Above 1x every frame is one vsync period of cart
    // time regardless of how long the host took to run it -- the run is
    // N frames per period, not one frame with a stretched delta, so the
    // cart plays out exactly the frames it would on the board, sooner.
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
        // HERE as well as at the frame boundary the pane checks: a cart
        // that never reaches `vsync_wait` publishes no frame for the pane
        // to notice the request on, and would otherwise run for ever.
        // Not an error either -- the world view asked.
        if world_panel_stop_requested(link.as_ref(), link_owned) {
            break (WORLD_PANEL_STOP.to_string(), false);
        }
        let turn_started = Instant::now();
        let event = emu.run_turn(input_mask, ticks_ms);
        // Drain every turn, regardless of what the turn ended with (Vsync,
        // Budget, Exit or Fault) -- not just on a completed frame. This is
        // the cadence `ggo-ide`'s `thread::run_loop` uses too
        // (`uart.push(&s.drain_uart())` once per `step`), and it is what
        // keeps the module's log sink bounded: a cart that logs a lot
        // between vsync waits (or never reaches one) can't grow the sink
        // past one turn's worth of bytes.
        match emu.take_log() {
            Ok(log) => uart.push(&log),
            Err(error) => break emulator_failure(error),
        }
        match event {
            // Frame boundary: the cart drew a complete frame and called
            // vsync_wait. Publish it, pace, then latch input --
            // `run_cart`'s Vsync arm, minus the present and the stall.
            TurnEvent::Vsync(number) => {
                let bgra = match emu.framebuffer_bgra() {
                    Ok(bgra) => bgra,
                    Err(error) => break emulator_failure(error),
                };
                let step_ms = turn_started.elapsed().as_secs_f32() * MILLIS_PER_SEC;
                // BEFORE the frame goes out: the snapshot describes this
                // frame, and the pane reads it on the frame's arrival.
                match emu.ppu_snapshot() {
                    Ok(ppu) => *snapshot.lock().unwrap() = Some(Arc::new(ppu)),
                    Err(error) => break emulator_failure(error),
                }
                let arm_tap = inspect.load(Ordering::Acquire);
                if let Err(error) = emu.with_arena(|arena| {
                    if arm_tap {
                        arm_world_tap(arena, &mut tap_addr, true);
                    }
                    publish_world_tap(arena, &mut tap_addr, world_json);
                }) {
                    break emulator_failure(error);
                }
                // A link this run has handed over (see
                // `Session::release_link`) is not this run's to touch:
                // its outbound queue now belongs to the next run.
                if let Some(link) = link
                    && link_owned.load(Ordering::Acquire)
                    && let Err(error) = crate::link::pump_link(&mut emu, link, &mut link_reader)
                {
                    break emulator_failure(error);
                }
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
                // the period as it can be. The module advanced the APU
                // exactly one frame on the way to this event, so there is
                // precisely one frame of samples waiting.
                let speed = speed.load(Ordering::Acquire).clamp(1, MAX_SPEED);
                if let Some(writer) = &audio_writer {
                    let copied = if speed == 1 {
                        pump_audio(
                            |cursor, out| emu.audio_copy_since(cursor, out),
                            audio_cursor,
                            &mut audio_scratch,
                            writer,
                        )
                    } else {
                        // Silence at speed: the ring would drop most of
                        // it anyway, and what got through would be noise.
                        // The cursor still advances so 1x resumes clean.
                        audio_scratch.clear();
                        emu.audio_copy_since(audio_cursor, &mut audio_scratch)
                    };
                    match copied {
                        Ok(cursor) => audio_cursor = cursor,
                        Err(error) => break emulator_failure(error),
                    }
                }
                frames_presented = frames_presented.wrapping_add(1);
                match emu.save_dirty() {
                    Ok(true)
                        if last_save_flush.is_none_or(|f| {
                            frames_presented.wrapping_sub(f) >= ggo_savefile::FLUSH_INTERVAL_FRAMES
                        }) =>
                    {
                        flush_save(&save_file, &info.title, &mut emu, uart);
                        last_save_flush = Some(frames_presented);
                    }
                    Ok(_) => {}
                    Err(error) => break emulator_failure(error),
                }
                if let Some(hold) = pace_sleep(last_present.elapsed(), FRAME_TIME / speed) {
                    std::thread::sleep(hold);
                }
                // The debugger's park: hold here, frame complete and
                // published, until resumed, stepped, or stopped. The pause
                // time is kept out of the cart's clock below so it doesn't
                // see a giant tick.
                let (parked, parked_stop) = park_while_paused(
                    pause,
                    step,
                    stop,
                    link.as_ref(),
                    link_owned,
                    audio_writer.as_ref(),
                );
                paused_total += parked;
                if let Some(reason) = parked_stop {
                    // Stopped out of the debugger's park: not an error,
                    // whichever of the two asked for it.
                    break (reason.to_string(), false);
                }
                last_present = Instant::now();
                // AFTER the hold, not before it. `native::refresh_input`
                // runs after `Display::present` has already slept out the
                // rest of the period, so the cart sees the pad as it was
                // at the START of the frame it is about to run, not as it
                // was ~16 ms earlier. Latching before the sleep costs a
                // whole frame of input latency.
                input_mask = input.load(Ordering::Acquire);
                // AFTER the hold and the input latch -- `ggo-emu/src/
                // lib.rs`'s Vsync arm sets the ticks last too
                // (present -> refresh_input -> set_ticks_ms), so the
                // clock the cart reads next turn accounts for the pacing
                // sleep it just went through.
                let real = start.elapsed().saturating_sub(paused_total);
                emulated += if speed == 1 {
                    real.saturating_sub(last_real)
                } else {
                    FRAME_TIME
                };
                last_real = real;
                ticks_ms = emulated.as_millis().min(u32::MAX as u128) as u32;
            }
            // Budget exhausted mid-frame: the framebuffer is half-drawn,
            // so do NOT publish it (`run_cart` likewise refuses to
            // present a partial buffer). Latch input and go round again;
            // the `stop` check at the top of the loop is what keeps this
            // interruptible for a cart that never reaches vsync_wait.
            TurnEvent::Budget => {
                // A cart that never reaches vsync_wait still honours pause
                // (no frame to publish or snapshot, but it parks).
                let (parked, parked_stop) = park_while_paused(
                    pause,
                    step,
                    stop,
                    link.as_ref(),
                    link_owned,
                    audio_writer.as_ref(),
                );
                paused_total += parked;
                if let Some(reason) = parked_stop {
                    // Stopped out of the debugger's park: not an error,
                    // whichever of the two asked for it.
                    break (reason.to_string(), false);
                }
                input_mask = input.load(Ordering::Acquire);
            }
            // The cart called exit: an ordinary end however it scores
            // itself, so NOT an error even for a non-zero code -- the code
            // is the cart's verdict on its own work, and the pane already
            // shows it in the reason.
            TurnEvent::Exit(code) => break (format!("cart exited with {code}"), false),
            // The module words both the arena overrun ("out of memory:
            // ...", the commonest way a cart dies) and any other CPU trap;
            // either way the run died, which is an error however the cart
            // got there.
            TurnEvent::Fault(reason) => break (reason, true),
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
    match emu.save_dirty() {
        Ok(true) => flush_save(&save_file, &info.title, &mut emu, uart),
        Ok(false) => {}
        Err(error) => uart.push_line(format!("[save] {error}")),
    }

    // `idump`/`ddump` are absent for the reason `ggo-ide` gives:
    // function-level I$/D$ attribution needs the cart's companion ELF and
    // tooling that lives above the emulator. The perf JSON simply omits the
    // optional `"profile"`/`"dprofile"` sections, which
    // `ingest::parse_output` treats as "no rows", not an error.
    let perf = match (emu.perf_json(), emu.perf_frames()) {
        (Ok(perf_json), Ok(frames)) => Some(PerfSnapshot {
            cart: info.title,
            perf_json,
            frames,
        }),
        (Err(error), _) | (_, Err(error)) => {
            uart.push_line(format!("[perf] {error}"));
            None
        }
    };
    RunOutcome {
        reason,
        is_error,
        perf,
    }
}

/// How an emulator-module call that errors (a trap inside the module, a
/// malformed hand-off) ends the run: always a failure.
fn emulator_failure(error: anyhow::Error) -> (String, bool) {
    (format!("emulator: {error}"), true)
}

/// Hold while `pause` is set: returns `(time parked, stop requested)`.
/// Checks `stop` first on every turn so a paused run still stops within
/// one frame time; one queued `step` releases exactly one turn. The audio
/// ring is idled on entry (silence, no dropouts counted) and re-primes on
/// the first push after resuming.
fn park_while_paused(
    pause: &AtomicBool,
    step: &AtomicU32,
    stop: &AtomicBool,
    link: Option<&Arc<LinkEndpoint>>,
    link_owned: &AtomicBool,
    audio_writer: Option<&RingWriter>,
) -> (Duration, Option<&'static str>) {
    if !pause.load(Ordering::Acquire) {
        return (Duration::ZERO, None);
    }
    let parked_at = Instant::now();
    if let Some(writer) = audio_writer {
        writer.idle();
    }
    loop {
        if stop.load(Ordering::Acquire) {
            return (parked_at.elapsed(), Some("stopped"));
        }
        // A PAUSED viewer still hears the world panel. The park is where
        // a manually paused run spends all of its time -- it has already
        // published the frame it was on -- so a request noticed only on
        // the frame path would never be acted on at all.
        if world_panel_stop_requested(link, link_owned) {
            return (parked_at.elapsed(), Some(WORLD_PANEL_STOP));
        }
        if !pause.load(Ordering::Acquire) {
            return (parked_at.elapsed(), None);
        }
        if step.load(Ordering::Acquire) > 0 {
            step.fetch_sub(1, Ordering::AcqRel);
            return (parked_at.elapsed(), None);
        }
        std::thread::sleep(FRAME_TIME);
    }
}

/// Has the world panel asked this run to end
/// ([`ggo_common::LinkEndpoint::request_stop`])?
///
/// Only while the link is still THIS run's to answer for: a handed-over
/// endpoint (see [`Session::release_link`]) belongs to whoever owns it
/// next, and its request is that run's to act on, not ours.
fn world_panel_stop_requested(link: Option<&Arc<LinkEndpoint>>, link_owned: &AtomicBool) -> bool {
    link.is_some_and(|link| link_owned.load(Ordering::Acquire) && link.stop_requested())
}

/// Write the save region to its file, clearing the module's dirty flag on
/// success. A failure is a console line, not a run failure -- the
/// standalone prints and carries on the same way.
fn flush_save(save_file: &Option<PathBuf>, title: &str, emu: &mut WasmEmu, uart: &UartLog) {
    let Some(path) = save_file else {
        return;
    };
    let save = match emu.save_bytes() {
        Ok(save) => save,
        Err(error) => {
            uart.push_line(format!("[save] {error}"));
            return;
        }
    };
    match ggo_savefile::flush_save(path, title, &save) {
        Ok(()) => {
            if let Err(error) = emu.clear_save_dirty() {
                uart.push_line(format!("[save] {error}"));
            }
        }
        Err(e) => uart.push_line(format!("[save] flush {} failed: {e}", path.display())),
    }
}

/// Move every APU sample mixed since `cursor` into `writer`, returning the
/// new cursor -- the whole audio contribution of one presented frame.
///
/// Factored out of [`run`] rather than inlined so it can be driven against
/// a real emulated APU with no output device anywhere in sight (see this
/// module's tests): everything about whether the emulated APU's samples
/// actually reach the ring is here, and nothing about it needs cpal.
///
/// `copy_since` is `WasmEmu::audio_copy_since` in a run. `scratch` is reused
/// across frames to avoid a per-frame allocation, and is cleared here
/// rather than by it -- that method
/// *appends*, so a caller that forgets would re-push every previous
/// frame's samples on top of the new ones.
fn pump_audio(
    copy_since: impl FnOnce(u64, &mut Vec<i16>) -> anyhow::Result<u64>,
    cursor: u64,
    scratch: &mut Vec<i16>,
    writer: &RingWriter,
) -> anyhow::Result<u64> {
    scratch.clear();
    let next = copy_since(cursor, scratch)?;
    writer.push(scratch);
    Ok(next)
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
            let found = ram
                .chunks_exact(4)
                .position(|c| c == TAP_MAGIC)
                .map(|i| i * 4)?;
            *tap_addr = Some(found);
            Some(found)
        }
    }
}

/// Arm (or disarm) the cart's tap by writing its `enabled` word — the
/// host-side switch that makes serialization cost nothing in ordinary
/// runs. No-op for carts without a tap. `arena` is the cart's writable
/// RAM arena, which the SDK linker anchors `.data`/`.bss` at the base of,
/// so it is the window the tap static can be in.
fn arm_world_tap(arena: &mut [u8], tap_addr: &mut Option<usize>, on: bool) {
    let Some(addr) = find_tap(arena, tap_addr) else {
        return;
    };
    let off = addr + TAP_ENABLED_OFFSET;
    if let Some(word) = arena.get_mut(off..off + 4) {
        word.copy_from_slice(&u32::from(on).to_le_bytes());
    }
}

/// Copy the cart's world-inspection JSON (if armed and written) out of
/// guest RAM into the session slot.
fn publish_world_tap(
    ram: &[u8],
    tap_addr: &mut Option<usize>,
    slot: &Mutex<Option<(u32, Arc<String>)>>,
) {
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

#[cfg(any(test, feature = "test-support"))]
pub use ggo_emu_wasm::fixture;

/// Helpers other modules' tests drive a real run through.
#[cfg(any(test, feature = "test-support"))]
pub mod tests_support {
    use super::*;

    /// The bundled emulator module, compiled once per test process --
    /// compiling is the slow part and every run shares the result.
    pub fn test_emulator() -> Arc<LoadedEmulator> {
        static EMULATOR: std::sync::OnceLock<Arc<LoadedEmulator>> = std::sync::OnceLock::new();
        EMULATOR
            .get_or_init(|| {
                Arc::new(
                    LoadedEmulator::compile(ggo_emu_wasm::BUNDLED_WASM, "bundled")
                        .expect("the bundled emulator compiles"),
                )
            })
            .clone()
    }

    /// Run the green fixture cart until `frames` frames have arrived,
    /// then stop it and return the finished run -- perf snapshot and all.
    /// Used by `crate::ingest`'s tests to ingest genuinely-emitted perf
    /// JSON rather than a hand-written imitation of it.
    #[cfg(test)]
    pub fn run_green_cart_briefly(frames: usize) -> FinishedRun {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, fixture::green_screen_cart()).unwrap();

        let (session, rx) = start(test_emulator(), path, "green.cart".to_string(), None, None);
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
        let (session, _rx) = start(
            tests_support::test_emulator(),
            PathBuf::from("/nonexistent/cart.ggo"),
            "cart.ggo".into(),
            None,
            None,
        );
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

    // ------------------------------------------------------- the sandbox

    /// The cart PMP is what bounds the arena now -- the flat `psram`
    /// backing does not -- so a cart that stores past the arena it was
    /// granted must halt, and halt saying so. Without the module's sandbox the
    /// store lands silently and the cart paints green forever, which is
    /// exactly what the `is_err` assertion catches (a frame arriving at
    /// all means no fault fired).
    #[test]
    fn a_store_past_the_arena_halts_the_run_out_of_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overrun.ggo");
        std::fs::write(&path, fixture::overrun_cart()).unwrap();

        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "overrun.ggo".into(),
            None,
            None,
        );
        assert!(
            rx.recv_blocking().is_err(),
            "the run must die on its first store, never reaching a frame"
        );
        let finished = session.wait();

        assert!(finished.is_error, "an arena overrun is a failed run");
        assert!(
            finished.reason.contains("out of memory")
                && finished
                    .reason
                    .contains(&format!("{:#010x}", fixture::OVERRUN_ADDR)),
            "the halt must name the overrun and the address: {}",
            finished.reason
        );
    }

    // ------------------------------------------------------- the audio tap
    //
    // These drive a REAL emulated APU -- the module's standalone `WasmApu`,
    // the same mixer a cart run uses -- straight into a real `RingWriter`,
    // with no output device anywhere. That is the whole point: every
    // machine can run them, including one with no sound card, and they
    // still prove the emulated APU's samples reach the ring the cpal
    // callback drains.

    /// Play one full-volume clip and advance the APU one frame, as the
    /// module does on every presented frame.
    fn apu_with_one_mixed_frame() -> ggo_emu_wasm::WasmApu {
        let mut apu = tests_support::test_emulator()
            .new_apu()
            .expect("the apu instantiates");
        apu.queue_samples(0, &[0x11u8; 64])
            .expect("the clip queues");
        apu.play_sample(
            0,
            0,
            64,
            ggo_emu_abi::ONE_SHOT,
            0x1000 | (0xFF << 16) | (0xFF << 24),
            0,
        )
        .expect("the clip plays");
        apu.run_frame().expect("the frame mixes");
        let mut mixed = Vec::new();
        apu.copy_since(0, &mut mixed).expect("the ring reads");
        assert!(
            mixed.iter().any(|&s| s != 0),
            "sanity: the fixture clip must actually be audible"
        );
        apu
    }

    #[test]
    fn pump_audio_moves_a_frames_mixed_samples_into_the_ring() {
        let mut apu = apu_with_one_mixed_frame();
        let status = crate::audio::AudioStatus::new();
        let (writer, reader) = crate::audio::channel(status);

        let mut scratch = Vec::new();
        let cursor = pump_audio(|c, out| apu.copy_since(c, out), 0, &mut scratch, &writer)
            .expect("the pump succeeds");

        let mut everything = Vec::new();
        assert_eq!(
            cursor,
            apu.copy_since(0, &mut everything).unwrap(),
            "the returned cursor must be caught up to the APU's writer"
        );
        assert!(
            !scratch.is_empty(),
            "one advanced frame mixes a frame's worth of samples"
        );
        assert!(
            scratch.iter().any(|&s| s != 0),
            "the clip must survive the copy, not arrive as silence"
        );
        assert_eq!(
            reader.queued_len(),
            scratch.len(),
            "everything drained from the APU was submitted to the ring"
        );
    }

    /// The cursor is what keeps one frame's samples from being submitted
    /// twice -- and `scratch` being reused across frames is exactly why
    /// [`pump_audio`] has to clear it (`copy_since` appends).
    #[test]
    fn pump_audio_submits_each_frame_once_across_a_reused_scratch_buffer() {
        let mut apu = apu_with_one_mixed_frame();
        let status = crate::audio::AudioStatus::new();
        let (writer, reader) = crate::audio::channel(status);

        let mut scratch = Vec::new();
        let cursor = pump_audio(|c, out| apu.copy_since(c, out), 0, &mut scratch, &writer)
            .expect("the pump succeeds");
        let first_frame = reader.queued_len();

        // Nothing new mixed: a second pump at the same cursor submits
        // nothing, rather than re-submitting the frame just sent.
        let cursor = pump_audio(
            |c, out| apu.copy_since(c, out),
            cursor,
            &mut scratch,
            &writer,
        )
        .expect("the pump succeeds");
        assert_eq!(
            reader.queued_len(),
            first_frame,
            "a caught-up cursor must submit nothing"
        );

        // One more mixed frame: exactly that frame's samples are added.
        // Not `first_frame * 2` -- the APU's mix rate is not an exact
        // multiple of 60, so consecutive frames differ by a sample.
        apu.run_frame().expect("the frame mixes");
        let mut mixed = Vec::new();
        apu.copy_since(cursor, &mut mixed).expect("the ring reads");
        pump_audio(
            |c, out| apu.copy_since(c, out),
            cursor,
            &mut scratch,
            &writer,
        )
        .expect("the pump succeeds");
        assert_eq!(
            reader.queued_len(),
            first_frame + mixed.len(),
            "the second frame adds exactly its own samples and no copy of the first"
        );
    }

    /// Mute reaches all the way down here: the emulated APU keeps mixing
    /// (its perf counters must not change just because the user muted),
    /// but nothing is submitted.
    #[test]
    fn pump_audio_submits_nothing_while_muted() {
        let mut apu = apu_with_one_mixed_frame();
        let status = crate::audio::AudioStatus::new();
        status.set_muted(true);
        let (writer, reader) = crate::audio::channel(status.clone());

        let mut scratch = Vec::new();
        let cursor = pump_audio(|c, out| apu.copy_since(c, out), 0, &mut scratch, &writer)
            .expect("the pump succeeds");
        assert_eq!(reader.queued_len(), 0, "a muted run submits no frames");
        let mut everything = Vec::new();
        assert_eq!(
            cursor,
            apu.copy_since(0, &mut everything).unwrap(),
            "the cursor still advances, so unmuting resumes from live \
             audio rather than replaying what was mixed while silent"
        );

        status.set_muted(false);
        pump_audio(|c, out| apu.copy_since(c, out), 0, &mut scratch, &writer)
            .expect("the pump succeeds");
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

        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "green.cart".to_string(),
            None,
            None,
        );

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

        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "green.cart".to_string(),
            None,
            None,
        );
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

        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "junk.cart".to_string(),
            None,
            None,
        );
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
            tests_support::test_emulator(),
            "/definitely/not/here.cart".into(),
            "here.cart".to_string(),
            None,
            None,
        );
        assert!(rx.recv_blocking().is_err());
        let finished = session.wait();
        assert!(finished.reason.contains("here.cart"), "{}", finished.reason);
        assert!(finished.is_error, "an unreadable cart file is a FAILED run");
        assert!(finished.perf.is_none());
    }

    /// A viewer whose run ends must not be left waiting on a `Building`
    /// that never resolves -- including when the run died in setup, before
    /// the frame loop the link is pumped from ever turned over.
    #[test]
    fn a_run_started_for_a_link_leaves_it_stopped_with_the_runs_reason() {
        let endpoint = LinkEndpoint::new();
        let (session, rx) = start(
            tests_support::test_emulator(),
            "/definitely/not/here.cart".into(),
            "here.cart".to_string(),
            None,
            Some(endpoint.clone()),
        );
        assert!(rx.recv_blocking().is_err());
        let finished = session.wait();
        assert_eq!(endpoint.state(), ViewerState::Stopped(finished.reason));
    }

    /// **The `Vsync` pump's call site, end to end.** A wire frame the
    /// host queues on the endpoint reaches a REAL cart's `comm_recv`, and
    /// what that cart sends back comes out of the endpoint as a decoded
    /// `CHANNEL_APP` payload. `crate::link`'s own tests drive the two
    /// halves directly; only this one runs them at the frame boundary the
    /// driver actually pumps at.
    #[test]
    fn the_frame_boundary_pump_carries_the_link_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("echo.cart");
        std::fs::write(&path, fixture::comm_echo_cart()).unwrap();

        let endpoint = LinkEndpoint::new();
        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "echo.cart".to_string(),
            None,
            Some(endpoint.clone()),
        );
        endpoint
            .send_app(b"ping")
            .expect("a four-byte payload fits");

        // The host's frame is injected at one boundary, read by the cart
        // on the next, and its reply decoded at the one after -- so a
        // handful of frames, not one. The `recv` blocks, so this cannot
        // spin.
        let mut inbound = Vec::new();
        for _ in 0..30 {
            rx.recv_blocking().expect("the emulator thread must run");
            inbound.extend(endpoint.try_recv_inbound());
            if !inbound.is_empty() {
                break;
            }
        }
        assert_eq!(
            inbound,
            vec![b"ping".to_vec()],
            "the cart echoed the host's datagram back over the link"
        );

        // Hand the link over: from here this run neither drains what the
        // host queues nor publishes what the cart sends, and its ending
        // is no longer its to report.
        session.release_link();
        endpoint
            .send_app(b"pong")
            .expect("a four-byte payload fits");
        for _ in 0..20 {
            rx.recv_blocking().expect("the emulator thread must run");
        }
        assert!(
            endpoint.try_recv_inbound().is_empty(),
            "a released link is not pumped"
        );
        assert_eq!(
            endpoint.take_outbound().len(),
            1,
            "and what the host queued is left for whoever owns the link next"
        );

        drop(rx);
        let finished = session.wait();
        assert_eq!(finished.reason, "stopped");
        assert_eq!(
            endpoint.state(),
            ViewerState::Building,
            "a released run does not report its own ending through the endpoint"
        );
    }

    /// **A PAUSED viewer run still hears the world panel.** The pane
    /// notices [`ggo_common::LinkEndpoint::request_stop`] on the frame
    /// path, and a paused run publishes no frames -- it sits in
    /// [`park_while_paused`] indefinitely. So the thread checks too, and
    /// ends with the same reason the pane would have written.
    ///
    /// No `Session::wait` before the assertion: `wait` sets the stop flag
    /// itself, which would end the run whether or not the request was
    /// heard. The thread dropping its frame sender IS the observation.
    #[test]
    fn a_paused_run_ends_on_the_world_panels_stop_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();

        let endpoint = LinkEndpoint::new();
        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "green.cart".to_string(),
            None,
            Some(endpoint.clone()),
        );
        rx.recv_blocking().expect("the emulator thread must run");
        session.pause();
        // Long enough for the thread to reach the park and settle there:
        // the pause is read at a frame boundary, so one frame period is
        // the bound and four is slack.
        std::thread::sleep(FRAME_TIME * 4);
        assert!(!rx.is_closed(), "a paused run is still a live run");

        endpoint.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !rx.is_closed() {
            assert!(
                Instant::now() < deadline,
                "a paused run ignored the world panel's stop request"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            endpoint.state(),
            ViewerState::Stopped(WORLD_PANEL_STOP.to_string()),
            "and says who ended it"
        );
        assert_eq!(session.wait().reason, WORLD_PANEL_STOP);
    }

    /// The same request on a run whose link has been handed over is not
    /// this run's to act on: the endpoint belongs to whoever owns it next,
    /// and their run is the one that must end.
    #[test]
    fn a_released_run_ignores_the_world_panels_stop_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("green.cart");
        std::fs::write(&path, green_screen_cart()).unwrap();

        let endpoint = LinkEndpoint::new();
        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "green.cart".to_string(),
            None,
            Some(endpoint.clone()),
        );
        rx.recv_blocking().expect("the emulator thread must run");
        session.release_link();

        endpoint.request_stop();
        for _ in 0..5 {
            rx.recv_blocking().expect("the run carries on");
        }
        assert_eq!(
            endpoint.state(),
            ViewerState::Building,
            "and it wrote nothing to an endpoint that is no longer its own"
        );
        drop(rx);
        assert_eq!(session.wait().reason, "stopped");
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

        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "quit.cart".to_string(),
            None,
            None,
        );
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

    /// End-to-end: a REAL guest `log()` ecall reaches the pane's console.
    /// Proves the whole chain the module's log sink -> the per-turn
    /// `uart.push(&emu.take_log())` drain -> [`crate::uart::UartLog`] -> what [`Session::wait`] hands
    /// back for ingest.
    #[test]
    fn a_carts_own_log_call_reaches_the_console() {
        use super::fixture::{LOG_MESSAGE, logging_cart};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logging.cart");
        std::fs::write(&path, logging_cart()).unwrap();

        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "logging.cart".to_string(),
            None,
            None,
        );
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

        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "green.cart".to_string(),
            None,
            None,
        );
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
        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "green.cart".to_string(),
            None,
            None,
        );

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

        // The Arc itself is HELD, never just its address: dropping it
        // frees the allocation, and the allocator is free to hand the very
        // same address back to the next snapshot -- which reads as "the
        // slot was never refilled" even when it was.
        let before_step = session.snapshot();
        session.step();
        let stepped =
            recv_within(&rx, Duration::from_secs(2)).expect("step releases exactly one frame");
        assert_eq!(stepped.number, last_number + 1, "one frame, the next one");
        let after_step = session.snapshot().expect("the stepped frame presented");
        assert!(
            !before_step
                .as_ref()
                .is_some_and(|before| Arc::ptr_eq(before, &after_step)),
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
        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "green.cart".to_string(),
            None,
            None,
        );
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
        let (session, rx) = start(
            tests_support::test_emulator(),
            path,
            "save.cart".to_string(),
            None,
            None,
        );
        rx.recv_blocking().expect("the cart runs");
        rx.recv_blocking().expect("and keeps running");
        let finished = session.wait();
        assert_eq!(finished.reason, "stopped");

        let save_path = ggo_savefile::resolve_save_path(
            &card_dir,
            fixture::SAVING_CART_TITLE,
            fixture::SAVING_CART_SAVE_BYTES as usize,
        )
        .expect("the flushed file is this cart's own save");
        let file = std::fs::read(&save_path).unwrap();
        assert_eq!(
            file.len(),
            ggo_savefile::SAVE_HDR_BYTES + fixture::SAVING_CART_SAVE_BYTES as usize
        );
        let payload = &file[ggo_savefile::SAVE_HDR_BYTES..];
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
