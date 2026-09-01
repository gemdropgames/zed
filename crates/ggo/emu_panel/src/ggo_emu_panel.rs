//! GGO Emulator panel (F3 tasks E1/E2, F4 X4): an embedded `ggo-emu` --
//! Run/Stop, live 320x240 video, keyboard -> pad input, a live stats row,
//! a diagnostic console, and an end-of-run perf ingest into
//! `~/.ggo/ggo_ide.db`. The emulation itself is `ggo-emu-core` verbatim;
//! [`drive`] ports the standalone binary's drive loop
//! (`ggo-emu/src/lib.rs::run_cart` + `src/native.rs`) onto a background
//! thread, and this module is the gpui shell around it.
//!
//! # How a cart gets here
//!
//! The same way every other GGO panel gets its document: from the file
//! explorer. Clicking a `.cart` in the project panel routes here through
//! [`intercept_cart_open`]; the panel has no picker of its own (X4 removed
//! the last one in the fork). A click SELECTS the cart -- it does not run
//! it. Running spawns an emulator thread and takes over the keyboard, so
//! it stays an explicit user action (Run / `ctrl-alt-r`), exactly as
//! opening a document in the world/sprite/tileset panels does not start
//! playback.
//!
//! # End of run
//!
//! Stop, a cart exit, a CPU fault and a Run-over-a-running-cart all funnel
//! into one place ([`EmuPanel::finish_run`]), which hands the session to a
//! background task: [`drive::Session::wait`] joins the emulator thread and
//! collects its perf snapshot plus the run's console lines, and
//! [`ingest`] writes them into the SAME `cart`/`run`/`frame`/`uart` tables
//! `ggo_charts_panel` reads back. That replaces ggo-ide's CLI-chained
//! `ggo-emu --perf` -> `ggo_ide.db` step with a native one; run a cart
//! here, and it is in the charts panel's picker the moment it stops.
//!
//! Structural mirror of `ggo_world_panel`/`ggo_sprite_panel`/
//! `ggo_charts_panel`: `Panel` impl, `ToggleFocus`, `observe_new`
//! registration into every new workspace, a `KeymapEventChannel` observer
//! that re-binds the panel's keys on every keymap reload, and project-root
//! discovery off the workspace's first visible worktree with a
//! `root_override` test hook. Unlike those panels, selecting a cart here is
//! synchronous (routed straight off the project panel's intercept, no
//! background load); this panel's own generation counter instead guards a
//! *run's* off-thread completion against a later run stomping it (see
//! `run_generation` below).
//!
//! Audio (F5.4 R6) is the pane's own: [`drive`] opens a `cpal` output
//! stream on the per-run emulator thread and drops it when the run ends,
//! so no device is ever held across an idle session. The transport carries
//! a mute toggle and the stats row carries the underrun counter -- see
//! [`audio`]'s module doc for why the pane owns a stream at all, and
//! [`drive`]'s for what else is still not ported.
//!
//! # The atlas-release contract (the load-bearing part)
//!
//! gpui NEVER frees a `RenderImage`'s GPU atlas tiles on its own. Every
//! `RenderImage::new` takes a fresh process-global `ImageId`
//! (`gpui/src/assets.rs:59-68`), `img(..)` on an `ImageSource::Render`
//! bypasses every image/asset cache (`gpui/src/elements/img.rs:548`) and
//! uploads straight into the window's sprite atlas keyed by that id
//! (`gpui/src/window.rs:4479`), `RenderImage` has no `Drop` impl, and no
//! atlas backend has an LRU, an eviction pass or a per-frame sweep -- the
//! only thing that ever returns a tile's rect to the allocator is an
//! explicit `PlatformAtlas::remove`, which only
//! [`Window::drop_image`](gpui::Window::drop_image) calls
//! (`gpui/src/window.rs:4577-4589`). So a 60 Hz pane that builds a new
//! `RenderImage` per frame and never calls `drop_image` leaks ~300 KB of
//! atlas per frame, forever: at 320x240 BGRA that is ~18 MB/s of atlas
//! growth, spawning fresh 1024x1024 atlas textures continuously
//! (`gpui_wgpu/src/wgpu_atlas.rs:175`) that can never drop because their
//! live-key count never returns to zero.
//!
//! [`EmuPanel::retire_atlas_frames`] implements the release path. It is
//! the livekit video view's double buffer
//! (`livekit_client/src/remote_video_track_view.rs:88-99`), NOT
//! `svg_preview_view::set_current`'s immediate replace-and-drop: the atlas
//! hands the freed rect straight back to the allocator, so a frame that a
//! just-submitted scene still references must survive one more render.
//! Frame N-2 is dropped, N-1 is retained. [`EmuPanel::release_atlas_all`]
//! covers the two teardown paths (Stop, and panel release) where no
//! further render will come.

pub mod audio;
mod debug;
pub mod agent_remote;
mod drive;
mod emu_item;
mod hardware;
mod hardware_item;
mod ingest;
mod input;
mod menu;
mod stats;
mod uart;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ggo_worldlib::charts::reports::diag_db;
use gpui::{
    AnyWindowHandle, App, AsyncApp, Bounds, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent,
    MouseMoveEvent, Pixels, Render, RenderImage, StatefulInteractiveElement, Styled, Subscription,
    Task, WeakEntity, Window, actions, div, point, px, size,
};
use project::ProjectPath;
use ui::Tooltip;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::Workspace;

use drive::{Frame, Session};
use input::InputState;
use stats::RunStats;
use uart::UartLog;

pub use emu_item::EmulatorItem;

actions!(
    ggo_emu,
    [
        /// Runs the selected cart in the emulator pane.
        Run,
        /// Stops the running cart.
        Stop,
        /// Mutes or unmutes the emulator's audio output.
        ToggleMute,
        /// Pauses the running cart at its next frame, or resumes it.
        TogglePause,
        /// While paused, runs exactly one more frame (pauses first if running).
        StepFrame,
        /// Shows or hides the debug column (tiles, tilemap, OAM, palettes).
        ToggleDebug
    ]
);

/// The panel's key-dispatch context. Everything the pane binds is scoped
/// to it, so the pad keys and the transport bindings are inert unless the
/// pane itself has focus -- typing `z` into an editor must never reach a
/// cart. See [`bind_panel_keys`].
const KEY_CONTEXT: &str = "GgoEmuPanel";

/// How many of the newest console lines the expanded console shows.
/// `ggo-ide`'s `pages/emulator.rs::LIVE_CONSOLE_TAIL_LINES` verbatim, for
/// its reason: this is deliberately far smaller than
/// [`uart::UART_LOG_CAP`] (2000, the full window the ingest receives),
/// because re-laying-out a couple of thousand text elements on every
/// `render` -- i.e. up to sixty times a second while a cart runs -- would
/// be real, needless cost for a debugging aid nobody reads that fast.
/// `UartLog::peek_tail` drains nothing, so this bounds only what is SHOWN.
const LIVE_CONSOLE_TAIL_LINES: usize = 100;

/// Height of the console's scroll region when expanded -- ggo-ide's
/// `CONSOLE_HEIGHT`, itself a port of `EmulatorPage.tsx`'s `max-h-64`.
const CONSOLE_HEIGHT: Pixels = px(200.);

/// The debug column's width: the 512-px sheets scroll inside it.
const DEBUG_COLUMN_PX: f32 = 360.0;
/// Palette grid swatch size.
const DEBUG_SWATCH_PX: f32 = 12.0;

pub fn init(cx: &mut App) {
    // Agent remote-control host (unix socket + on-disk advertisement) --
    // see `agent_remote`'s module doc.
    agent_remote::init(cx);

    // Explorer-driven routing: clicking a `.cart` in the project panel
    // selects it HERE instead of opening a (binary, unreadable) editor tab.
    // This is the panel's only way in -- there is no in-panel cart picker.
    workspace::register_path_open_interceptor(cx, intercept_cart_open);

    // Right-clicking offers the three run actions (S4) -- see `menu`.
    workspace::register_context_menu_contributor(cx, menu::contribute_run_menu);

    // The charts panel's Re-run entry (F5.4 R3) lands here. It cannot call
    // this crate directly -- `ggo_emu_panel` already depends on
    // `ggo_charts_panel` for S4's finished-run hop, so the reverse edge
    // would be a cycle -- so the handoff goes through `ggo_common`'s
    // registry, exactly as the `.cart` explorer routing above goes through
    // `workspace`'s. What it routes to is [`EmuPanel::rerun`]: the SAME
    // entry S4's context menu uses, arming the same hop back, so a Re-run
    // started from the charts panel ends where a Re-run started from the
    // explorer does.
    ggo_common::register_cart_runner(cx, |workspace, rel, window, cx| {
        open_emu_item(workspace, window, cx, move |emu, window, cx| {
            emu.rerun(rel, window, cx);
        })
    });

    // The world panel's Emulate button lands here through the same
    // `ggo_common` registry hop as the charts panel's Re-run above, for
    // the same cycle. It routes to the SAME save-first handler the
    // explorer's "Emulate this world (cart)" entry uses, deferred because
    // that handler saves the world panel's doc and takes the workspace
    // itself -- neither may nest inside the caller's updates.
    ggo_common::register_board_flasher(cx, |workspace, world, rebuild_gateware, window, cx| {
        // Owned before the defer: the caller's world stem is borrowed
        // from ITS document, which outlives neither this call nor the
        // closure below.
        let world = world.map(str::to_string);
        open_emu_item(workspace, window, cx, move |_emu, window, cx| {
            // DEFERRED, like the world emulator below: this handler runs
            // inside `workspace.update`, and `flash_to_board` ->
            // `refresh_root` reads that same leased entity. Doing it here
            // is the double-lease panic, not a style preference.
            cx.defer_in(window, move |emu, window, cx| {
                emu.flash_to_board_with(world.as_deref(), rebuild_gateware, window, cx)
            });
        })
    });

    ggo_common::register_world_emulator(cx, |workspace, rel, window, cx| {
        let handler = menu::emulate_world_handler(workspace.weak_handle(), rel.to_string());
        window.defer(cx, move |window, cx| handler(window, cx));
        true
    });
}

/// Open (or focus) THE center-pane emulator tab and run `f` against its
/// panel -- the emulator is a singleton item, so every entry point
/// (explorer `.cart` click, the run menu, the charts panel's Re-run, the
/// world panel's Emulate) lands in the same tab. The center pane is
/// where a 320x240 frame can actually scale up; the old right-dock home
/// capped it at a sidebar's width.
pub fn open_emu_item(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut EmuPanel, &mut Window, &mut Context<EmuPanel>),
) -> bool {
    // Starting emulation is a HEAVY action: fold every center split into
    // one pane first, so the screen goes to the emulator.
    ggo_common::collapse_center_splits(workspace, window, cx);
    let existing = workspace.items_of_type::<EmulatorItem>(cx).next();
    let item = match existing {
        Some(item) => {
            workspace.activate_item(&item, true, true, window, cx);
            item
        }
        None => {
            let weak = workspace.weak_handle();
            let item = cx.new(|cx| EmulatorItem::new(weak, window, cx));
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            item
        }
    };
    let panel = item.read(cx).panel().clone();
    panel.update(cx, |panel, cx| f(panel, window, cx));
    true
}

/// The cart extension this panel claims from the file explorer. The
/// single-file `.ggo` variant carries the same `GGOC` header plus an asset
/// section and `ggo_emu_core::cart::Cart::parse` handles both, but F3's
/// brief scoped the panel to `.cart` and X4 kept that scope.
const CART_EXT: &str = "cart";

/// Empty-state text. The panel has no picker of its own by design (F4 X4):
/// carts arrive by clicking a `.cart` in the project panel. Worded like
/// the sprite/tileset/world panels' own empty states.
const EMPTY_MESSAGE: &str = "Open a .cart file from the project panel";

/// `workspace::PathOpenInterceptor` for `*.cart`: claim the path, open the
/// panel, and select the cart. Declines (so the normal open path runs) for
/// any other file, for a path outside the primary worktree, and when no
/// panel is docked.
fn intercept_cart_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !path
        .path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(CART_EXT))
    {
        return false;
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    open_emu_item(workspace, window, cx, move |panel, window, cx| {
        panel.open_rel_path(&rel, window, cx)
    })
}

// ------------------------------------------------------------- view state

/// The end-of-run perf ingest's state, shown under the transport.
/// Mirrors `ggo-ide`'s `pages/emulator.rs::IngestStatus`, minus its
/// "View in Reports" navigation button (this fork's charts panel is a
/// separate dock panel with its own picker, not a route to push).
#[derive(Debug, Clone, PartialEq)]
enum IngestStatus {
    /// Nothing has finished yet.
    Idle,
    /// The run never reached a single `vsync_wait`, so there is nothing
    /// worth a `run` row -- ggo-ide's identical "no frames recorded"
    /// guard, which is also what keeps a cart that fails to load from
    /// writing an empty run.
    NoFrames,
    Uploading,
    /// The run row's id, and the original frame count when the ingest
    /// kept only the first `ingest::MAX_FRAMES`.
    Done(i64, Option<usize>),
    Failed(String),
}

impl IngestStatus {
    fn label(&self) -> Option<String> {
        match self {
            IngestStatus::Idle => None,
            IngestStatus::NoFrames => Some("no frames recorded — nothing ingested".into()),
            IngestStatus::Uploading => Some("ingesting perf diagnostics…".into()),
            IngestStatus::Done(run_id, None) => Some(format!(
                "perf run #{run_id} ingested — see the GGO Charts panel"
            )),
            IngestStatus::Done(run_id, Some(frames)) => Some(format!(
                "perf run #{run_id} ingested, truncated to {} of {frames} frames — see the GGO Charts panel",
                ingest::MAX_FRAMES
            )),
            IngestStatus::Failed(e) => Some(format!("perf ingest failed: {e}")),
        }
    }
}

/// The ingest half of [`EmuPanel::finish_run`]'s background task: what a
/// finished run becomes on the panel's ingest row. BLOCKING (it writes a
/// SQLite database), so callers stay off the UI thread. Named rather than
/// inline in the spawn so the failure paths -- malformed perf JSON, an
/// unopenable database -- are testable without a real session to end.
fn ingest_finished_run(
    finished: &drive::FinishedRun,
    db_path_override: Option<PathBuf>,
    label: &str,
) -> IngestStatus {
    match &finished.perf {
        // ggo-ide's "no frames recorded (cart never reached vsync)"
        // guard, and the only case for a cart that failed to load:
        // writing a zero-frame run row would just be noise in the charts
        // picker.
        None => IngestStatus::NoFrames,
        Some(perf) if perf.frames == 0 => IngestStatus::NoFrames,
        Some(perf) => match db_path_override.or_else(ggo_common::default_db_path) {
            None => IngestStatus::Failed("no HOME to resolve ~/.ggo".into()),
            Some(db_path) => {
                match ingest::ingest_run(&db_path, &perf.perf_json, &finished.uart, Some(label)) {
                    Ok(run) => IngestStatus::Done(run.run_id, run.truncated_frames),
                    Err(e) => IngestStatus::Failed(e),
                }
            }
        },
    }
}

pub struct EmuPanel {
    focus_handle: FocusHandle,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery
    /// (`ggo_world_panel::root_override`'s analog).
    root_override: Option<PathBuf>,
    /// Test hook: ingest into this database instead of `~/.ggo/ggo_ide.db`
    /// -- `ggo_charts_panel`'s `db_path_override`, same name, same reason.
    /// Load-bearing here rather than merely convenient: without it, any
    /// test that ran a real cart to completion would write a `run` row
    /// into the developer's actual database.
    db_path_override: Option<PathBuf>,
    /// Test hook: pull a flashed run's rows out of THIS `diag.db` rather
    /// than `~/.ggo/diag.db`. `ggo_charts_panel`'s `diag_db_path_override`,
    /// same name and same reason; separate from `db_path_override` because
    /// the two are files owned by two different tools (see
    /// `ggo_common::default_diag_db_path`). Load-bearing for the same
    /// reason as its neighbour: without it the post-flash hop would read
    /// the developer's real database in a test.
    diag_db_path_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// The cart the file explorer last routed here, as a project-relative
    /// `/`-separated path. `None` until something is clicked.
    selected: Option<String>,

    /// Bumped every time [`Self::run`] starts a new session. [`Self::
    /// finish_run`] captures this at call time and its background
    /// completion closure re-checks it before writing `status`/
    /// `ingest_status`, discarding the result if it no longer matches.
    /// Needed because a run's completion (`Session::wait`, joined
    /// off-thread) can land seconds after a later run has already started
    /// and become the one the pane is showing; without this, run A's late
    /// completion would stomp run B's live status.
    run_generation: u64,
    /// How this panel spawns children -- `emd pack-ggo` for "Emulate this
    /// world", `ggo-diag` for "Run hardware diagnostics". Injectable for
    /// the same reason `ggo_emerald_panel`'s is: it is what lets those
    /// paths be tested end to end without `emd`, a GGO checkout or a
    /// board.
    proc_runner: ggo_common::ProcRunner,
    /// Test hook: pretend the machine has (or hasn't) a board, instead of
    /// reading env vars and `/dev`. Load-bearing rather than convenient --
    /// [`menu::diag_env`]'s inputs are process-global, so without this the
    /// no-hardware path could only be asserted on a machine that happens
    /// to have no hardware.
    diag_env_override: Option<menu::DiagEnv>,
    /// Bumped by every [`Self::emulate_world`]/[`Self::
    /// run_hardware_diagnostics`] click, so a superseded child's result is
    /// dropped instead of starting a run (or overwriting a newer status)
    /// after the user has already asked for something else. Separate from
    /// `run_generation`: a build is not a run, and one build produces at
    /// most one run.
    build_generation: u64,
    _build_task: Option<Task<()>>,
    /// The run generation the pending "hop to the charts panel when this
    /// run's perf ingest lands" is owed to, plus the window to do it in --
    /// the ingest completes on a background task with no `Window` of its
    /// own, and focusing a dock needs one. Armed by every [`Self::run`]:
    /// EVERY stopped run routes to its generated report.
    charts_for_run: Option<(u64, AnyWindowHandle)>,

    /// The running emulator, if any. Dropping it signals the thread to
    /// stop (see [`Session::stop`]).
    session: Option<Session>,
    /// Pumps [`Frame`]s from the emulator thread onto the UI thread. Its
    /// completion (the emulator thread dropping its sender) is also how
    /// the panel learns a run ended on its own.
    _pump_task: Option<Task<()>>,
    /// Last run's exit/error line, shown under the transport.
    status: Option<String>,
    /// Whether [`Self::status`] is a FAILURE rather than a report. A run
    /// ending is normal and reads muted; a build that failed, a `ggo-diag`
    /// that failed, and the no-board message are all things the user has
    /// to act on, and the ingest row beside them already uses
    /// `Color::Error` for exactly that -- a failure whispered in the same
    /// grey as "stopped" is a failure that gets missed. Set through
    /// [`Self::report_failure`]; every ordinary status write clears it.
    status_is_error: bool,
    /// The cart-visible frame number of the last frame received -- the
    /// pane's "is it actually running" readout.
    frame: u32,
    /// Latched pad mask, published into the session on every change.
    input: InputState,

    /// fps / dropped frames / step cost for the current run.
    stats: RunStats,
    /// Mute, device availability and the underrun counter. Owned by the
    /// PANEL rather than by a [`Session`], for two reasons: mute is a user
    /// preference that has to survive across runs (and be settable before
    /// one starts), and the last run's underrun count is exactly the thing
    /// a user wants to still be reading after the run that produced it
    /// ended. A clone goes to [`drive::start`], which is what connects it
    /// to a real device for the life of that run.
    audio: audio::AudioStatus,
    /// When the current FPS window opened. Lives here rather than in
    /// [`RunStats`] so all of that module's math stays pure.
    fps_window_started: Instant,
    /// The current (or most recent) run's diagnostic log. Kept after the
    /// run ends -- in cart mode the interesting lines are written on the
    /// way out, so blanking the console at that moment would hide exactly
    /// what the user wants to read.
    console: Option<UartLog>,
    console_expanded: bool,
    ingest_status: IngestStatus,

    /// The frame to paint. `None` before the first frame of a run.
    latest_frame: Option<Arc<RenderImage>>,
    /// Atlas double buffer -- see the module doc. `current` is the frame
    /// the last render painted; `previous` is the one before it, which is
    /// what actually gets `drop_image`d.
    current_rendered_frame: Option<Arc<RenderImage>>,
    previous_rendered_frame: Option<Arc<RenderImage>>,
    /// Clears the pad mask when the pane loses focus, so a key held while
    /// the user clicks away doesn't stay latched forever.
    _focus_out: Option<Subscription>,
    /// The debug column -- see [`debug`].
    debug: debug::DebugState,
    /// Set by [`Self::auto_pause`] (the tab was hidden); cleared -- and the
    /// run resumed -- by the next render. A user's own pause never sets it,
    /// so it is never auto-resumed.
    auto_paused: bool,
    /// Watch mode: a save anywhere in the project re-packs and restarts
    /// `watched_world` (the world the last "Emulate this world" built).
    watch: bool,
    watched_world: Option<String>,
    watch_rebuilds: u32,
    /// The window the watch restarts run in -- `emulate_world` needs one
    /// and a worktree event has none.
    watch_window: Option<AnyWindowHandle>,
    /// The window this panel lives in, captured at construction for the
    /// agent remote host (`agent_remote::dispatch` needs one to boot/stop).
    remote_window: Option<AnyWindowHandle>,
    _watch_subscription: Option<Subscription>,
    _watch_debounce: Option<Task<()>>,
    /// An `emd pack-ggo` is in flight: a second one would race it on the
    /// same output file, so saves meanwhile queue one rebuild instead.
    building: bool,
    /// The in-flight build was the user's own "Emulate this world" (whose
    /// save produces events); changes seen during it are not queued.
    build_is_explicit: bool,
    pending_rebuild: bool,
    /// The next `emulate_world` is a watch restart: keep the pad, skip the
    /// report hop, count it.
    watch_restart_pending: bool,

    /// How this panel streams a flash / setup run. Injectable for the
    /// same reason `proc_runner` is: a test scripts the transcript.
    proc_streamer: ggo_common::ProcStreamer,
    /// The flash (or setup) run in flight. Dropping it kills the child --
    /// that is the cancel button.
    flash: Option<FlashRun>,
    /// The last run's timeline and total, kept after the run ends so the
    /// page can still show what happened.
    last_flash: Option<(hardware::FlashProgress, Duration)>,
    /// The last report the post-PASS hop resolved, as `(ggo-diag's run
    /// id, our own local `run.id`)` -- remembered because the agent
    /// socket is asked for it AFTER the fact and must not re-run two
    /// blocking database calls on the UI thread to answer.
    ///
    /// The ggo-diag id is stored WITH the value, and never dropped for
    /// being old: the reader ([`Self::remote_flash_status`]) hands the
    /// number out only when it is about to report that same run id, so a
    /// report id can never appear beside another run's timeline. That is
    /// a property of the read, not of who wrote last -- which is the only
    /// thing that survives a hop whose lookup is detached, unbounded, and
    /// free to land after the next board run has taken the page.
    last_flash_perf_run: Option<(String, i64)>,
    /// The window a started flash should open its report in --
    /// [`Self::charts_for_run`]'s analog for the board, and armed for the
    /// same reason: the run ends on a background task with no `Window` of
    /// its own, and focusing a dock needs one.
    ///
    /// Armed only by [`Self::flash_to_board_with`] (the one entry that HAS
    /// a window) and taken by the next [`Self::start_board_run`], so a
    /// setup or `git pull` run never inherits a flash's arming.
    flash_charts_window: Option<AnyWindowHandle>,
    /// The last hardware probe. `None` re-probes on the next ask.
    hardware: Option<hardware::HardwareEnv>,
    /// The world stem (`worlds/arena`) the next flash bakes in as the
    /// cart's boot world.
    ///
    /// Remembered rather than passed per press because the flash surfaces
    /// are not all next to a world: the world panel's menu names one, but
    /// the hardware page's buttons are a tab away from any document and
    /// must re-flash the SAME world -- a page that silently fell back to
    /// the project's `default_world` is exactly the divergence this
    /// feature exists to end. Set by "Emulate this world" (once its build
    /// has validated) and by the world panel's flash path, and cleared
    /// when the project root changes out from under it. Read through
    /// [`Self::flash_world`], never directly: `None` here still falls back
    /// to the world panel's open document before it gives up and leaves
    /// `default_world` alone.
    flash_world: Option<String>,
}

/// A board run in progress: what stage it reached, and the task whose
/// drop cancels it.
struct FlashRun {
    /// The run's timeline, folded from `ggo-diag`'s own output.
    progress: hardware::FlashProgress,
    /// When the run began. Every phase time is relative to this, which
    /// is why [`hardware::FlashProgress`] needs no clock of its own.
    started: Instant,
    /// What is being run, for the status row before any phase lands.
    what: String,
    _task: Task<()>,
}

impl Drop for FlashRun {
    /// The one clear point every ending shares -- finish, failure,
    /// cancel, a closed panel -- so the rescue guard can never stay
    /// armed after its run.
    fn drop(&mut self) {
        hardware::set_board_run_in_flight(false);
    }
}

/// Append one line to the run's on-disk transcript, when it has one.
/// Every miss is tolerated -- a full disk must not take the run down --
/// but not silently: `log_err` puts it in Zed's own log.
fn log_run_line(run_log: &Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>, line: &str) {
    let Some(run_log) = run_log else {
        return;
    };
    // A poisoned lock means a writer panicked mid-line; the transcript is
    // best effort, so skip rather than propagate.
    if let Ok(mut file) = run_log.lock() {
        use std::io::Write as _;
        writeln!(file, "{line}").log_err();
    }
}

/// How long after the last change the re-pack waits, so a save that
/// writes several files (a `.til`/`.pal` pair, a sprite trio) packs once.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Does a changed worktree-relative path warrant a re-pack? Build output
/// (any `target/` -- the pack writes under the emerald project's, which
/// need not be the worktree root) and editor sidecars never do: the
/// former would loop.
fn watch_triggers(rel: &str, change: &project::PathChange) -> bool {
    if matches!(change, project::PathChange::Loaded) {
        return false;
    }
    !rel.split('/')
        .any(|component| component == "target" || component == ".ggo-ide")
}

impl EmuPanel {
    pub fn new(
        workspace: Option<WeakEntity<Workspace>>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let remote_window = window.as_ref().map(|w| w.window_handle());
        let _focus_out = window.map(|window| {
            cx.on_focus_out(&focus_handle, window, |this, _event, _window, cx| {
                this.release_all_buttons(cx);
            })
        });
        // Teardown: the panel's last one or two frames are still in the
        // window atlas when the panel is released, and no further render
        // will come to retire them. `on_release` is the only hook left --
        // the same one livekit's video view uses
        // (`remote_video_track_view.rs:32-44`).
        cx.on_release(|this, cx| {
            this.debug.retire_all();
            for image in [
                this.previous_rendered_frame.take(),
                this.current_rendered_frame.take(),
                this.latest_frame.take(),
            ]
            .into_iter()
            .flatten()
            .chain(this.debug.take_all_retired())
            {
                cx.drop_image(image, None);
            }
        })
        .detach();

        Self {
            focus_handle,
            workspace,
            root_override: None,
            db_path_override: None,
            diag_db_path_override: None,
            project_root: None,
            selected: None,
            run_generation: 0,
            proc_runner: ggo_common::system_proc_runner(),
            diag_env_override: None,
            build_generation: 0,
            _build_task: None,
            charts_for_run: None,
            session: None,
            _pump_task: None,
            status: None,
            status_is_error: false,
            frame: 0,
            input: InputState::default(),
            stats: RunStats::default(),
            audio: audio::AudioStatus::new(),
            fps_window_started: Instant::now(),
            console: None,
            console_expanded: false,
            ingest_status: IngestStatus::Idle,
            latest_frame: None,
            current_rendered_frame: None,
            previous_rendered_frame: None,
            _focus_out,
            debug: debug::DebugState::new(),
            auto_paused: false,
            watch: false,
            watched_world: None,
            watch_rebuilds: 0,
            watch_window: None,
            remote_window,
            _watch_subscription: None,
            _watch_debounce: None,
            building: false,
            build_is_explicit: false,
            pending_rebuild: false,
            watch_restart_pending: false,
            proc_streamer: ggo_common::system_proc_streamer(),
            flash: None,
            last_flash: None,
            last_flash_perf_run: None,
            flash_charts_window: None,
            hardware: None,
            flash_world: None,
        }
    }

    // ------------------------------------------------------------- flash

    /// The cached readiness, refreshing it if this is the first ask.
    ///
    /// `probe` walks `/dev`, scans `PATH` four times and walks the
    /// ancestors for a repo -- fine once, ruinous from `render_transport`,
    /// which runs on every frame of a running cart.
    fn hardware_env_cached(&mut self) -> hardware::HardwareEnv {
        if self.hardware.is_none() {
            self.hardware = Some(self.hardware_env());
        }
        self.hardware.clone().unwrap_or_default()
    }

    /// Drop the cached probe: a setup run just changed the answer, and so
    /// does plugging the board in (which is why activation re-probes).
    fn invalidate_hardware(&mut self) {
        self.hardware = None;
    }

    /// This machine's hardware readiness, probed fresh (a setup run, or
    /// plugging the board in, changes the answer).
    fn hardware_env(&self) -> hardware::HardwareEnv {
        hardware::probe(
            self.project_root.as_deref(),
            std::env::var("PATH").ok().as_deref(),
            &std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_default(),
        )
    }

    fn is_flashing(&self) -> bool {
        self.flash.is_some()
    }

    /// The world the next flash will boot: the one this panel remembers,
    /// else whatever world the docked world panel currently has open.
    ///
    /// The fallback is what stops the two flash buttons from disagreeing.
    /// This panel only learns a world by being TOLD one (an emulate, or
    /// the world panel's own flash button), so without it a user who has
    /// a world open but has pressed neither gets `default_world` from the
    /// emulator's toolbar and the open world from the world panel's
    /// button an inch away -- the exact divergence this feature removes.
    /// Reading another entity here is safe: a child view's `render` is
    /// invoked from element layout, after the workspace's own render has
    /// returned, so nothing above is leased.
    pub(crate) fn flash_world(&self, cx: &App) -> Option<String> {
        if let Some(world) = self.flash_world.clone() {
            return Some(world);
        }
        let workspace = self.workspace.as_ref()?.upgrade()?;
        let world_panel = workspace
            .read(cx)
            .panel::<ggo_world_panel::WorldPanel>(cx)?;
        world_panel.read(cx).open_world_stem()
    }

    /// Remember the world a flash should boot, so the flash surfaces that
    /// are a tab away from any document (the hardware page's buttons)
    /// reach the same one.
    pub(crate) fn remember_flash_world(&mut self, world: &str) {
        self.flash_world = Some(world.to_string());
    }

    /// The flash-and-run button: flash the open project to the board and
    /// boot it, or -- while one is in flight -- cancel.
    pub fn flash_to_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.flash_to_board_with(None, false, window, cx);
    }

    /// [`Self::flash_to_board`], with the world to boot and the choice of
    /// rebuilding the gateware.
    ///
    /// `world` is the stem the PRESS names -- the world panel's button
    /// knows one, this panel's own surfaces do not and pass `None`, which
    /// leaves [`Self::flash_world`] to answer. `rebuild_gateware` drops
    /// `--skip-pnr` so the run place-and-routes the SoC and flashes a
    /// fresh bitstream instead of the cached one -- what a pulled GGO repo
    /// with PPU changes needs.
    pub fn flash_to_board_with(
        &mut self,
        world: Option<&str>,
        rebuild_gateware: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cancel_flash(cx) {
            return;
        }
        // The gap a blocked flash leaves is the page's to explain, not a
        // status line's -- see [`Self::start_flash`], whose error the
        // button therefore drops.
        let config = hardware::FlashConfig {
            world: world.map(str::to_string),
            rebuild_gateware,
            ..Default::default()
        };
        self.start_flash(config, window, cx).ok();
    }

    /// Cancel the flash in flight, if any. Cancelling is not failing: the
    /// timeline stays as it was, with the phase it got to still marked
    /// running. Before any world is remembered, too: a cancel started
    /// nothing, so it has no business changing what the next one boots.
    fn cancel_flash(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(flash) = self.flash.take() else {
            return false;
        };
        let elapsed = flash.started.elapsed();
        self.last_flash = Some((flash.progress.clone(), elapsed));
        self.status = Some("flash cancelled".to_string());
        self.status_is_error = false;
        cx.notify();
        true
    }

    /// [`Self::flash_to_board_with`]'s start half, with the reason a
    /// flash did not start RETURNED rather than left to the page.
    ///
    /// The button has a hardware page to read (a missing prerequisite is
    /// a checklist with buttons there, not a one-line error) and so
    /// ignores this; the agent socket has no page, so `flash_request`'s
    /// "flashing needs a board: …" has to come back as its reply.
    fn start_flash(
        &mut self,
        config: hardware::FlashConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<hardware::FlashConfig, String> {
        if self.is_flashing() {
            return Err("a flash is already running".to_string());
        }
        if let Some(world) = &config.world {
            // Remembered, not merely used: the hardware page this opens
            // has flash buttons of its own, and they have to reach the
            // same world.
            self.remember_flash_world(world);
        }
        self.refresh_root(cx);
        self.invalidate_hardware();
        let env = self.hardware_env_cached();
        // Unconditional, because both outcomes are the same page: a
        // missing prerequisite is not a one-line error but a checklist
        // with buttons, and a started run is a timeline of phases. The
        // status row can carry neither, and "where did my flash go"
        // should have one answer.
        self.open_hardware_page(window, cx);
        let (request, what, progress) = self.flash_plan(&env, &config, cx)?;
        // The world the argv got is the remembered one (see `flash_plan`),
        // and that -- not the caller's `None` -- is what the reply says.
        let effective = hardware::effective_config(
            &env,
            &hardware::FlashConfig { world: self.flash_world(cx), ..config },
        );
        // Arm the "open this flash's report when it passes" for the run
        // about to start. Here rather than in `start_board_run`, which
        // also serves the windowless setup/pull runs -- and only after
        // the plan succeeded, so a flash blocked by a missing
        // prerequisite leaves no arming behind.
        self.flash_charts_window = Some(window.window_handle());
        self.start_board_run(vec![request], what, progress, cx);
        Ok(effective)
    }

    /// The whole shape of the flash `env` would run: what to spawn, what
    /// the status row and timeline call it, and which phases to expect.
    ///
    /// Split out of [`Self::flash_to_board_with`] so the wiring that
    /// actually matters -- that the remembered world reaches the child's
    /// argv AND the line the user reads -- is assertable without a board,
    /// a window or a spawn. The caller is then thin enough to read.
    fn flash_plan(
        &self,
        env: &hardware::HardwareEnv,
        config: &hardware::FlashConfig,
        cx: &App,
    ) -> Result<(ggo_common::ProcRequest, String, hardware::FlashProgress), String> {
        // `start_flash` remembered a named world already; the remembered
        // one is what the argv gets either way, so the two never differ.
        let world = self.flash_world(cx);
        let config = hardware::FlashConfig { world: world.clone(), ..config.clone() };
        let request = hardware::flash_request(env, &config)?;
        let what = match &world {
            // The timeline's own header: which world is on its way to the
            // board is the one thing a flash cannot show.
            Some(world) => format!("flashing {world}"),
            None => "flashing".to_string(),
        };
        let progress = if config.rebuild_gateware {
            hardware::FlashProgress::flash_full()
        } else {
            hardware::FlashProgress::flash()
        };
        Ok((request, what, progress))
    }

    /// Show the hardware setup page.
    pub(crate) fn open_hardware_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        // `window.defer`, NOT `cx.defer_in`: the latter runs its closure
        // inside an `EmuPanel` update, and opening the page reaches back
        // into this very panel (`open_emu_item`) -- which is a double
        // lease on it. Deferring at the App level leaves both free.
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                hardware_item::open_hardware_item(workspace, window, cx);
            });
        });
    }

    /// "Set up hardware tooling": run every install step in order,
    /// stopping at the first failure.
    pub fn setup_hardware(&mut self, cx: &mut Context<Self>) {
        self.refresh_root(cx);
        self.invalidate_hardware();
        let env = self.hardware_env_cached();
        let steps = hardware::setup_steps(&env);
        if steps.is_empty() {
            let blocked: Vec<String> = env
                .missing()
                .into_iter()
                .filter(|m| !m.installable())
                .map(|m| m.label())
                .collect();
            self.report_failure(
                if blocked.is_empty() {
                    "nothing to install".to_string()
                } else {
                    format!("nothing ZedGG can install: {}", blocked.join("; "))
                },
                cx,
            );
            return;
        }
        let what = steps
            .iter()
            .map(|step| step.label.clone())
            .collect::<Vec<_>>()
            .join(", ");
        // An install run announces none of `ggo-diag`'s phases (cargo
        // and git have their own output), so it gets no timeline -- the
        // page falls back to its console for these.
        self.start_board_run(
            steps.into_iter().map(|step| step.request).collect(),
            what,
            hardware::FlashProgress::steps(Vec::new()),
            cx,
        );
    }

    /// "Sync GGO repo": bring the clone this fork manages to the GGO
    /// remote's head -- pulling, or recloning when the pull cannot
    /// fast-forward -- and reinstall `ggo-emu` from the synced source.
    ///
    /// This moves the clone toward its ORIGIN, not toward the emulator:
    /// `emu_commit` was frozen when ZedGG itself was compiled, so a sync
    /// closes the gap only when the clone is the side that is behind.
    ///
    /// Goes through the same runner the installs use, which streams into
    /// the same console and re-probes when it ends -- so the page shows
    /// the synced commit without a Re-check.
    pub fn sync_ggo_repo(&mut self, cx: &mut Context<Self>) {
        self.refresh_root(cx);
        self.invalidate_hardware();
        let env = self.hardware_env_cached();
        let Some(request) = env.sync_request() else {
            self.report_failure(
                "only the GGO clone ZedGG manages can be synced from here".to_string(),
                cx,
            );
            return;
        };
        // Like a setup run, git and `cargo install` announce none of
        // `ggo-diag`'s phases, so it gets the console rather than a
        // timeline.
        self.start_board_run(
            vec![request],
            "syncing the GGO repo".to_string(),
            hardware::FlashProgress::steps(Vec::new()),
            cx,
        );
    }

    /// Run `requests` in order through the streaming runner, feeding every
    /// line to the console and every recognised line to the status row.
    /// Stops at the first failure.
    fn start_board_run(
        &mut self,
        requests: Vec<ggo_common::ProcRequest>,
        what: String,
        mut progress: hardware::FlashProgress,
        cx: &mut Context<Self>,
    ) {
        progress.what = Some(what.clone());
        let console = self.console.get_or_insert_with(uart::UartLog::new).clone();
        progress.console_from = console.lines().len();
        let streamer = self.proc_streamer.clone();
        // Taken, not read: this run spends the arming whatever it is, so a
        // flash that recorded nothing cannot leave one for the next setup
        // run to fire on. A run started without a window (setup, `git
        // pull`, a test) simply has none, which is the hop's off switch.
        let charts_window = self.flash_charts_window.take();
        // Resolved here, on the thread the overrides live on: ggo-diag's
        // own file to clone the flashed run out of, and ours to clone it
        // into. `None` only when no home directory resolves at all, which
        // is the one case with no report to open and nothing to say.
        let report_dbs = self
            .diag_db_path_override
            .clone()
            .or_else(ggo_common::default_diag_db_path)
            .zip(
                self.db_path_override
                    .clone()
                    .or_else(ggo_common::default_db_path),
            );
        self.status = None;
        self.status_is_error = false;
        self.console_expanded = true;
        // The run's transcript on disk, surviving the console it also
        // fills -- a failure found after the pane is gone still has its
        // log. Best effort by design: a run is never blocked by its own
        // paper trail.
        let run_log = hardware::create_run_log(&what).map(|(path, file)| {
            console.push_line(format!("transcript: {}", path.display()));
            progress.transcript = Some(path);
            std::sync::Arc::new(std::sync::Mutex::new(file))
        });
        hardware::set_board_run_in_flight(true);
        let task = cx.spawn({
            let what = what.clone();
            async move |this, cx| {
                for request in requests {
                    let (line_tx, line_rx) = async_channel::unbounded::<String>();
                    // The command itself, so the console says which argv (and
                    // which of several `/dev/ttyUSB*`) this run used.
                    let command_line = format!("$ {}", request.command_line());
                    log_run_line(&run_log, &command_line);
                    console.push_line(command_line);
                    let run = {
                        let console = console.clone();
                        let run_log = run_log.clone();
                        streamer(
                            request,
                            Box::new(move |line: &str| {
                                log_run_line(&run_log, line);
                                console.push_line(line);
                                line_tx.try_send(line.to_string()).ok();
                            }),
                        )
                    };
                    // Lines arrive while the child runs; each recognised one
                    // moves the status row.
                    let pump = {
                        let this = this.clone();
                        cx.spawn(async move |cx| {
                            while let Ok(line) = line_rx.recv().await {
                                // Notify for EVERY line, not only recognised
                                // ones: `cargo install` matches none of the
                                // grammar, and an unrepainted console during a
                                // five-minute install is the opposite of the
                                // streaming this exists to provide.
                                if this
                                    .update(cx, |this, cx| {
                                        if let Some(flash) = &mut this.flash {
                                            let at = flash.started.elapsed();
                                            flash.progress.apply(&line, at);
                                        }
                                        cx.notify();
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        })
                    };
                    let capture = run.await;
                    // AWAITED, not dropped: the sink drops with the run
                    // future, which closes the channel and ends the pump. A
                    // drop here could lose the last line -- and the last line
                    // is the verdict.
                    pump.await;
                    let Ok(verdict) = this.read_with(cx, |this, _| {
                        this.flash.as_ref().and_then(|f| f.progress.verdict())
                    }) else {
                        // The panel is gone; a released tab must not keep
                        // installing things.
                        return;
                    };
                    if !capture.ok || verdict == Some(false) {
                        let reason = hardware::failure_reason(&capture);
                        log_run_line(&run_log, &format!("== {what} failed: {reason}"));
                        this.update(cx, |this, cx| {
                            this.retire_flash(Some(reason.clone()));
                            // A failed run can still have changed the machine
                            // -- a reclone whose `rm -rf` landed before the
                            // clone failed has no repo at all any more, and a
                            // page reading the pre-run probe would still show
                            // one.
                            this.invalidate_hardware();
                            this.report_failure(format!("{what} failed: {reason}"), cx);
                        })
                        .ok();
                        return;
                    }
                }
                this.update(cx, |this, cx| {
                    let passed = this.flash.as_ref().and_then(|f| f.progress.verdict());
                    log_run_line(
                        &run_log,
                        &format!(
                            "== {what}: {}",
                            match passed {
                                Some(true) => "PASS",
                                _ => "done",
                            }
                        ),
                    );
                    this.retire_flash(None);
                    // Installing something changes what this machine can do.
                    this.invalidate_hardware();
                    this.status = Some(match passed {
                        Some(true) => format!("{what}: PASS"),
                        _ => format!("{what}: done"),
                    });
                    this.status_is_error = false;
                    cx.notify();
                    // Only a PASS gets a report: a run that never reached a
                    // verdict is a setup run, and a failed one has no
                    // telemetry worth opening. The id comes off the retired
                    // timeline, which is where `retire_flash` just put it.
                    let Some(diag_run_id) = (match passed {
                        Some(true) => this
                            .last_flash
                            .as_ref()
                            .and_then(|(progress, _)| progress.diag_run_id.clone()),
                        _ => None,
                    }) else {
                        return;
                    };
                    // A SEPARATE, detached task rather than an `await` out
                    // here: `retire_flash` above dropped the `FlashRun`
                    // that owns the handle to the task this closure is
                    // running on, and a dropped `Task` is cancelled at its
                    // next suspension point -- so anything awaited after
                    // this line would silently never resume.
                    cx.spawn(async move |this, cx| {
                        Self::open_flashed_run_report(
                            this,
                            cx,
                            diag_run_id,
                            charts_window,
                            report_dbs,
                        )
                        .await;
                    })
                    .detach();
                })
                .ok();
            }
        });
        self.flash = Some(FlashRun {
            progress,
            started: Instant::now(),
            what,
            _task: task,
        });
        cx.notify();
    }

    /// Move a finished run's timeline off the live slot and onto the
    /// page: which phase a run died in is exactly what someone looks at
    /// after it ends, and dropping it with the task loses that.
    fn retire_flash(&mut self, failure: Option<String>) {
        let Some(mut flash) = self.flash.take() else {
            return;
        };
        let elapsed = flash.started.elapsed();
        if let Some(reason) = failure {
            flash.progress.fail(elapsed);
            flash.progress.failure = Some(reason);
        }
        // Cloned, not moved: `FlashRun` has a `Drop` impl (the rescue
        // guard), and a type with one cannot be moved out of field-wise.
        self.last_flash = Some((flash.progress.clone(), elapsed));
    }

    /// Open the charts item on the report for the run `ggo-diag` just
    /// recorded -- the board's half of the emulator's "every stopped run
    /// routes to its generated report" (see [`Self::charts_for_run`]).
    ///
    /// The rows have to be CLONED first: `~/.ggo/diag.db` is another
    /// tool's file and nothing here may read it as a peer
    /// (`ggo_charts_panel::history`'s module doc has the whole rule), so
    /// `clone_runs` copies this run into our own `ggo_ide.db` and
    /// `device_perf_run_id` then translates ggo-diag's TEXT run id into the
    /// INTEGER `run.id` the charts panel opens by -- two different id
    /// spaces, which is why the translation is a db lookup and not a cast.
    ///
    /// Every way this can come up empty leaves the PASS status exactly as
    /// it is: a flash whose boot captured no telemetry, a `ggo-diag` old
    /// enough to have written none, or a database that would not read are
    /// all "there is no report to open", not "the flash went wrong". The
    /// unexpected ones are logged rather than shown, because the run the
    /// user asked for did succeed.
    ///
    /// A clone that reports a failure is one of those logged cases and
    /// **not** a reason to stop: the failure may belong to some OTHER,
    /// older run in ggo-diag's file (see the call site), while this run's
    /// rows landed fine. `device_perf_run_id` is the question that
    /// actually decides, so it is always asked.
    async fn open_flashed_run_report(
        this: WeakEntity<Self>,
        cx: &mut AsyncApp,
        diag_run_id: String,
        window: Option<AnyWindowHandle>,
        report_dbs: Option<(PathBuf, PathBuf)>,
    ) {
        // No window means this run was not a flash (a setup run, a pull, a
        // test) -- and focusing a dock needs one either way.
        let (Some(window), Some((diag_db_path, ide_db_path))) = (window, report_dbs) else {
            return;
        };
        // Which run this hop is FOR, kept back from the closure that
        // consumes it: by the time the lookup returns, "the last flash"
        // may be somebody else's (see [`Self::remember_flash_perf_run`]).
        let flashed_run = diag_run_id.clone();
        // BLOCKING: both calls spin their own current-thread tokio runtime,
        // the rule `ggo_charts_panel::history::load` carries too -- so they
        // run on the background executor and never on the UI thread.
        let local_id = cx
            .background_spawn(async move {
                // Never created, only read: `clone_runs` opens it through
                // `ggo_db::open_existing`, and a machine whose ggo-diag has
                // recorded nothing has no file at all.
                if !diag_db_path.exists() {
                    return None;
                }
                // Logged, never fatal. `clone_runs` reconciles EVERY run in
                // ggo-diag's file and reports a failure in any of them --
                // including historical runs this build cannot read (a
                // `diag.db` behind on migrations, whose `frame` table has
                // no `cyc`), which fail identically on every call forever.
                // Each run is its own committed transaction, so an `Err`
                // says nothing about whether THIS run made it across. Ask
                // the database that instead of guessing from the error, the
                // same correction `ggo_emu_mcp::tools`' `fetch_ggo_report`
                // makes.
                if let Err(e) = diag_db::clone_runs(&diag_db_path, &ide_db_path) {
                    log::warn!("flashed run {diag_run_id}: could not clone every device run: {e}");
                }
                match diag_db::device_perf_run_id(&ide_db_path, &diag_run_id) {
                    Ok(local_id) => local_id,
                    Err(e) => {
                        log::warn!("flashed run {diag_run_id}: could not resolve its report: {e}");
                        None
                    }
                }
            })
            .await;
        let Some(local_id) = local_id else {
            return;
        };
        // Remembered before it is opened: `flash_status` over the agent
        // socket answers with this rather than repeating the lookup.
        // Stored WITH the run it belongs to and written unconditionally
        // -- a hop that lands after the next run started is harmless,
        // because the reader pairs the ids rather than trusting order.
        this.update(cx, |this, _cx| {
            this.last_flash_perf_run = Some((flashed_run, local_id))
        })
        .ok();
        let Some(workspace) = this
            .read_with(cx, |this, _cx| this.workspace.clone())
            .ok()
            .flatten()
        else {
            return;
        };
        window
            .update(cx, |_root, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        ggo_charts_panel::open_charts_item(
                            workspace,
                            window,
                            cx,
                            |charts, _window, cx| {
                                charts.open_run(local_id, cx);
                            },
                        );
                    })
                    .ok();
            })
            .ok();
    }

    /// The run to draw: the one in flight, else the last one to end.
    /// The `Duration` is the run's total elapsed time, which is what
    /// turns each row's start into a live timer.
    pub(crate) fn flash_progress(&self) -> Option<(&hardware::FlashProgress, Duration)> {
        match &self.flash {
            Some(flash) => Some((&flash.progress, flash.started.elapsed())),
            None => self
                .last_flash
                .as_ref()
                .map(|(progress, elapsed)| (progress, *elapsed)),
        }
    }

    /// The console's lines, for the setup page's output pane.
    pub(crate) fn console_lines(&self) -> Vec<String> {
        self.console
            .as_ref()
            .map(|console| console.lines())
            .unwrap_or_default()
    }

    /// The status row's flash text: the current stage, else what is running.
    fn flash_status(&self) -> Option<String> {
        let flash = self.flash.as_ref()?;
        Some(match flash.progress.running() {
            Some(row) => format!(
                "{}: {}",
                flash.what,
                row.detail.clone().unwrap_or_else(|| row.title.clone())
            ),
            None => format!("{}…", flash.what),
        })
    }

    // --------------------------------------------------------------- watch

    /// A plain cart replaced the world: nothing left to re-pack.
    fn forget_watched_world(&mut self) {
        self.watch = false;
        self.watched_world = None;
        self._watch_debounce = None;
        self.pending_rebuild = false;
    }

    /// Turn watch mode on/off. On requires a world to have been emulated
    /// (a bare cart has nothing to re-pack).
    fn set_watch(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if on && self.watched_world.is_none() {
            return;
        }
        self.watch = on;
        self.watch_window = on.then(|| window.window_handle());
        self.watch_rebuilds = 0;
        self.pending_rebuild = false;
        self.watch_restart_pending = false;
        if !on {
            self._watch_debounce = None;
        }
        if on {
            self.ensure_watch_subscription(cx);
        }
        cx.notify();
    }

    /// Subscribe to the project's worktree changes once. Called from the
    /// watch toggle (outside any `Workspace` update), never from `new`,
    /// which runs while the workspace is leased.
    fn ensure_watch_subscription(&mut self, cx: &mut Context<Self>) {
        if self._watch_subscription.is_some() {
            return;
        }
        let Some(project) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
            .map(|workspace| workspace.read(cx).project().clone())
        else {
            return;
        };
        self._watch_subscription = Some(cx.subscribe(&project, Self::on_project_event));
    }

    fn on_project_event(
        &mut self,
        _project: Entity<project::Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        let project::Event::WorktreeUpdatedEntries(_, changes) = event else {
            return;
        };
        if !self.watch {
            return;
        }
        let relevant = changes
            .iter()
            .any(|(path, _, change)| watch_triggers(path.as_unix_str(), change));
        if !relevant {
            return;
        }
        if self.building {
            // Queue behind the running pack -- unless that pack is the
            // user's own Emulate, whose save is what we are seeing.
            if !self.build_is_explicit {
                self.pending_rebuild = true;
            }
            return;
        }
        self.schedule_watch_rebuild(cx);
    }

    /// (Re)start the debounce; the rebuild runs when it lapses.
    fn schedule_watch_rebuild(&mut self, cx: &mut Context<Self>) {
        let (Some(world), Some(window)) = (self.watched_world.clone(), self.watch_window) else {
            return;
        };
        let workspace = self.workspace.clone();
        self._watch_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(WATCH_DEBOUNCE).await;
            window
                .update(cx, |_, window, cx| {
                    let go = this
                        .update(cx, |this, _| {
                            if !this.watch {
                                return false;
                            }
                            if this.building {
                                this.pending_rebuild = true;
                                return false;
                            }
                            // A restart is not a finished run: no report
                            // hop (it would steal focus and drop the pad).
                            this.charts_for_run = None;
                            this.watch_restart_pending = true;
                            this.watch_rebuilds += 1;
                            true
                        })
                        .unwrap_or(false);
                    if !go {
                        return;
                    }
                    // Through the registered emulator, so a dirty world
                    // panel is saved first -- what an explicit Emulate does.
                    if let Some(workspace) = workspace.as_ref().and_then(|w| w.upgrade()) {
                        workspace.update(cx, |workspace, cx| {
                            ggo_common::emulate_world(workspace, &world, window, cx);
                        });
                    }
                })
                .ok();
        }));
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree) -- the directory `run` joins the selected rel path onto.
    /// MUST NOT run while the workspace itself is mid-update (it reads the
    /// workspace entity); see the deferrals in `set_active` and in
    /// [`Self::open_rel_path`].
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        let previous_root = self.project_root.clone();
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        // A different project is a different `worlds/` tree, so a stem
        // remembered from the old one would flash a world that no longer
        // exists (or, worse, a same-named world in another game). Only a
        // CHANGE clears it: the first discovery is `None -> Some`, which
        // happens on the way into the very flash that just named a world.
        if previous_root.is_some() && previous_root != self.project_root {
            self.flash_world = None;
        }
        if let Some(root) = self.project_root.clone() {
            let panel = cx.weak_entity();
            let window = self.remote_window;
            cx.defer(move |cx| agent_remote::register_panel(root, panel, window, cx));
        }
        cx.notify();
    }

    /// Select the project-relative `.cart` path `rel`. This is the panel's
    /// entry point from the file explorer ([`intercept_cart_open`]); there
    /// is no in-panel picker.
    ///
    /// **A click selects, it does not run.** Starting a cart spawns an
    /// emulator thread and takes over the keyboard, which is far too much
    /// to happen as a side effect of a single click in a file tree; the
    /// user presses Run. This mirrors the other GGO panels, where a click
    /// opens a document without acting on it.
    ///
    /// **Re-clicking the cart that is already selected does nothing at
    /// all** -- in particular it must not touch a run in progress. The
    /// interceptor has already revealed and focused the dock by the time
    /// we get here, so an already-selected click IS the focus/reveal.
    ///
    /// **Clicking a DIFFERENT cart while one is running stops the running
    /// one first**, through the ordinary [`Self::stop`] path rather than
    /// by dropping the session on the floor. That matters because
    /// `stop` -> [`Self::finish_run`] is what collects the run's perf
    /// snapshot and writes it into `~/.ggo/ggo_ide.db`: silently losing a
    /// run's diagnostics because the user clicked the next cart would be a
    /// data loss, small but real. There is no unsaved *document* here, so
    /// no [`ggo_common::prepare_to_close_dirty`] prompt -- a running
    /// process is not user data, and the one thing it produces is
    /// preserved.
    ///
    /// The root re-discovery runs on a spawned task, deliberately: the
    /// interceptor calls this from INSIDE the workspace's own update, and
    /// [`Self::refresh_root`] has to read that same workspace entity.
    /// `stop` reads no entity, so it happens inline while we still have a
    /// `Window`.
    pub fn open_rel_path(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(rel) {
            return;
        }
        // Switching carts: end the current run the normal way, so its perf
        // data still lands in the database.
        self.stop(window, cx);
        self.forget_watched_world();
        self.selected = Some(rel.to_string());
        // The previous cart's counters would otherwise sit under the newly
        // selected one's name.
        self.frame = 0;
        self.stats = RunStats::default();
        cx.notify();

        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| this.refresh_root(cx)).ok();
        })
        .detach();
    }

    /// Start the selected cart. A run already in flight is stopped first,
    /// so Run is idempotent-ish (restart) rather than a way to end up
    /// with two emulator threads fighting over one pane.
    pub(crate) fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(cart)) = (self.project_root.clone(), self.selected.clone()) else {
            self.report_failure("no cart selected".to_string(), cx);
            return;
        };
        self.stop(window, cx);
        self.auto_paused = false;
        // AFTER `stop`, not before: `stop` -> `finish_run` reads
        // `run_generation` to tag the run it is finishing (the OLD one),
        // so it must still see the pre-bump value. This is the new
        // current run from here on -- see `run_generation`'s doc.
        self.run_generation += 1;
        // Arm the "hop to the charts panel when this one's perf lands"
        // for THIS run -- the window handle has to be captured here,
        // because the ingest completes without one.
        self.charts_for_run = Some((self.run_generation, window.window_handle()));

        // Clear the previous run's underrun count and device verdict
        // BEFORE the thread starts, so the pane never shows the last run's
        // "no output device" against this one. Mute is deliberately kept.
        self.audio.reset_for_run();
        let (session, rx) = drive::start(root.join(&cart), cart, Some(self.audio.clone()));
        self.console = Some(session.uart().clone());
        self.session = Some(session);
        // A watch restart keeps the pad as the player holds it.
        self.publish_input();
        self.status = None;
        self.status_is_error = false;
        self.frame = 0;
        self.stats = RunStats::default();
        self.fps_window_started = Instant::now();
        self.ingest_status = IngestStatus::Idle;
        self._pump_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(frame) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.on_frame(frame, cx))
                    .is_err()
                {
                    return;
                }
                // Give the executor a turn after EVERY frame. `recv` only
                // suspends on an empty channel, and the emulator thread
                // refills it every ~16ms -- so whenever one frame's update
                // (a redraw, in a test window synchronously) takes longer
                // than that, the next `recv` is ready immediately and this
                // loop never yields: one poll of this task runs for the
                // whole life of the cart, starving every other foreground
                // task. Observed as a permanent hang in the debug-build
                // tests, where a full workspace draw is slower than a frame.
                smol::future::yield_now().await;
            }
            // The emulator thread dropped its sender: the run is over on
            // its own terms (cart exit, CPU fault, or a stop flag it has
            // now acted on). There is no terminal message on the wire --
            // the close IS the message. See `drive::start`'s doc.
            this.update(cx, |this, cx| this.finish_run(cx)).ok();
        }));
        cx.notify();
    }

    fn on_frame(&mut self, frame: Frame, cx: &mut Context<Self>) {
        self.frame = frame.number;
        if self.stats.on_frame(
            frame.number,
            frame.step_ms,
            self.fps_window_started.elapsed(),
        ) {
            self.fps_window_started = Instant::now();
        }
        // `from_raw` takes the Vec by value: the emulator thread
        // already produced BGRA, so this is a move, not a copy.
        if let Some(buffer) = image::ImageBuffer::from_raw(drive::WIDTH, drive::HEIGHT, frame.bgra)
        {
            self.latest_frame = Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])));
        }
        cx.notify();
    }

    /// The single end-of-run path: Stop, a cart exit, a CPU fault, and a
    /// Run over a still-running cart all arrive here.
    ///
    /// Takes the session (so the transport flips back to Run immediately)
    /// and hands it to a background task, because both halves of the work
    /// block: [`Session::wait`] joins the emulator thread, and
    /// [`ingest::ingest_run`] opens and writes a SQLite database. Neither
    /// may touch the UI thread.
    ///
    /// No idempotence guard is needed here -- unlike ggo-ide, whose Stop
    /// click and terminal frame could both fire for one run and write two
    /// duplicate `run` rows. `self.session.take()` IS the guard: the
    /// second caller for the same run finds `None` and returns.
    fn finish_run(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.take() else {
            return;
        };
        // The run this call is finishing -- captured now, before anything
        // async, so it names THIS run regardless of whichever run
        // `run_generation` points at by the time the background task
        // below actually resolves. See `run_generation`'s doc.
        let generation = self.run_generation;
        // ...and which build/diagnostics click owns the STATUS ROW right
        // now. `emulate_world`/`run_hardware_diagnostics` both begin by
        // stopping whatever is running and then write their own message
        // there, and they bump `build_generation`, never `run_generation`
        // -- so without this second stamp, this completion (which lands
        // later, off-thread) overwrites that message with the stopped
        // run's exit reason. For "Emulate this world" that merely flickers
        // "building …" away; for the no-board diagnostics message, whose
        // whole job is to be READ, it was fatal: the entry looked like a
        // silent no-op. Only the status write is skipped -- the ingest
        // result below is this run's own data and nothing else claims that
        // row, which is also why bumping `run_generation` here would be
        // the wrong fix (it would discard a legitimate ingest).
        let owns_status = self.build_generation;
        if !self.watch_restart_pending {
            self.input.clear();
        }
        self.ingest_status = IngestStatus::Uploading;
        cx.notify();

        // The rel path, as the run's `label` column -- the free-text run
        // identity the charts panel's picker shows next to the cart name.
        // ggo-ide passes `None` here only because neither of its run
        // sources has an identity to attach; this pane always knows which
        // file it ran.
        let label = session.cart.clone();
        let db_path_override = self.db_path_override.clone();
        let finish = cx.background_spawn(async move {
            let finished = session.wait();
            let status = ingest_finished_run(&finished, db_path_override, &label);
            (finished.reason, finished.is_error, status)
        });
        cx.spawn(async move |this, cx| {
            let (reason, is_error, status) = finish.await;
            let hop = this
                .update(cx, |this, cx| {
                    if this.run_generation != generation {
                        // A later run has started (and possibly already
                        // ended) since this one was taken -- this
                        // completion is stale, so don't let it stomp the
                        // live run's status.
                        return None;
                    }
                    if this.build_generation == owns_status {
                        this.status = Some(reason);
                        // The run itself says whether it ended badly: a
                        // cart that never loaded or faulted is a failure,
                        // a stop or a plain exit is not.
                        this.status_is_error = is_error;
                    }
                    this.ingest_status = status;
                    cx.notify();
                    // The Re-run entry's promised hop to the charts panel.
                    // Taken unconditionally: the run is over either way,
                    // so an arming that didn't produce a `run` row (a cart
                    // that never reached vsync, a failed ingest) is spent,
                    // not left to fire after some later run.
                    match (&this.ingest_status, this.charts_for_run.take()) {
                        (IngestStatus::Done(run_id, _), Some((armed, window)))
                            if armed == generation =>
                        {
                            Some((*run_id, window))
                        }
                        _ => None,
                    }
                })
                .ok()
                .flatten();
            if let Some((run_id, window)) = hop {
                let workspace = this
                    .read_with(cx, |this, _cx| this.workspace.clone())
                    .ok()
                    .flatten();
                if let Some(workspace) = workspace {
                    window
                        .update(cx, |_root, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    ggo_charts_panel::open_charts_item(
                                        workspace,
                                        window,
                                        cx,
                                        |charts, _window, cx| {
                                            charts.open_run(run_id, cx);
                                        },
                                    );
                                })
                                .ok();
                        })
                        .ok();
                }
            }
        })
        .detach();
    }

    // ------------------------------------------------- the S4 menu actions

    /// Put a FAILURE on the status row: shown in `Color::Error` rather
    /// than the muted grey a run's exit reason gets (see
    /// [`Self::status_is_error`]).
    fn report_failure(&mut self, message: String, cx: &mut Context<Self>) {
        self.status = Some(message);
        self.status_is_error = true;
        cx.notify();
    }

    /// Report a precondition the MENU checked and this panel could not --
    /// today, exactly one: "Emulate this world" could not write the world
    /// panel's unsaved edits, so there is nothing safe to build.
    ///
    /// Deliberately does NOT `stop`: the user's build never started, and
    /// killing a cart they are already running as a side effect of a
    /// refused one would be its own bug. It DOES bump `build_generation`,
    /// which is what stops a still-in-flight `finish_run` completion from
    /// overwriting this message with a run's exit reason (see
    /// [`Self::finish_run`]).
    pub fn report_blocked(&mut self, message: String, cx: &mut Context<Self>) {
        self.build_generation += 1;
        self.report_failure(message, cx);
    }

    /// **"Emulate this world"**: build a cartridge whose boot world is
    /// `world_rel`, then run it here.
    ///
    /// The saving half happens before this, in the menu handler
    /// ([`menu::contribute_run_menu`]), because it belongs to a different
    /// panel. What is left is ggo-ide's `EmuMsg::BuildAndRunWorld` reduced
    /// to this fork's engine: one `emd pack-ggo --world <stem>` (see
    /// [`menu::world_pack_args`] for exactly how that differs from
    /// ggo-ide's full-system build, and why), then the panel's ORDINARY
    /// [`Self::run`] over the artifact it produced. There is deliberately
    /// no second run path: the cart the build writes is selected exactly
    /// as a clicked one is, so everything downstream -- the pump, the
    /// atlas double buffer, the end-of-run perf ingest -- is the code that
    /// was already there.
    ///
    /// Runs INSIDE the workspace's own update (the entry handler focused
    /// the dock to get here), so every step that reads the workspace back
    /// -- `refresh_root`, which the build needs for the absolute path of
    /// the world -- is deferred onto the spawned task, exactly as
    /// [`Self::open_rel_path`] defers it for the interceptor.
    pub fn emulate_world(&mut self, world_rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        let restart = self.watch_restart_pending;
        // `stop` reads the flag (a restart keeps the pad); clear it after.
        self.stop(window, cx);
        self.watch_restart_pending = false;
        if !restart {
            self.watch_rebuilds = 0;
            self._watch_debounce = None;
        }
        self.building = true;
        self.build_is_explicit = !restart;
        self.watched_world = Some(world_rel.to_string());
        self.build_generation += 1;
        let generation = self.build_generation;
        let world_rel = world_rel.to_string();
        self.status = Some(format!("building {world_rel}…"));
        self.status_is_error = false;
        self.ingest_status = IngestStatus::Idle;
        cx.notify();

        let this = cx.weak_entity();
        self._build_task = Some(window.spawn(cx, async move |cx| {
            this.update(cx, |this, cx| this.refresh_root(cx)).ok();
            let prepared = this
                .update(cx, |this, cx| this.prepare_world_build(&world_rel, cx))
                .ok()
                .flatten();
            let Some((request, runner, cart)) = prepared else {
                this.update(cx, |this, cx| this.build_done(generation, cx))
                    .ok();
                return;
            };
            let capture = cx.background_spawn(async move { runner(request) }).await;
            this.update_in(cx, |this, window, cx| {
                if this.build_generation != generation {
                    return;
                }
                this.build_done(generation, cx);
                if !capture.ok {
                    this.report_failure(
                        format!("build failed: {}", menu::failure_reason(&capture)),
                        cx,
                    );
                    return;
                }
                this.selected = Some(cart);
                this.frame = 0;
                this.stats = RunStats::default();
                this.status = None;
                this.status_is_error = false;
                this.run(window, cx);
            })
            .ok();
        }));
    }

    /// Assemble the `emd pack-ggo` invocation for `world_rel`, or report
    /// on the status row why there isn't one. Runs on the UI thread, where
    /// a problem can still be shown, and hands the background task
    /// everything it needs by value.
    /// The build for `generation` ended (either way): release the
    /// in-flight flag and run any rebuild that queued behind it.
    fn build_done(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.build_generation != generation {
            return;
        }
        self.building = false;
        if std::mem::take(&mut self.pending_rebuild) && self.watch {
            self.schedule_watch_rebuild(cx);
        }
    }

    fn prepare_world_build(
        &mut self,
        world_rel: &str,
        cx: &mut Context<Self>,
    ) -> Option<(ggo_common::ProcRequest, ggo_common::ProcRunner, String)> {
        let mut fail = |this: &mut Self, message: String| {
            this.report_failure(message, cx);
            None
        };
        let Some(root) = self.project_root.clone() else {
            return fail(self, "no project folder is open".to_string());
        };
        // Re-derived rather than passed down from the menu: the same
        // `ggo_world_panel` rule that decided the entry was offered at all.
        let Some(stem) = ggo_world_panel::world_stem(world_rel) else {
            return fail(self, format!("{world_rel} is not a world file"));
        };
        let Some(project_dir) = ggo_common::emerald_project_root(&root.join(world_rel)) else {
            return fail(
                self,
                format!(
                    "no {} above {world_rel} — emd needs an emerald project",
                    ggo_common::EMERALD_MANIFEST
                ),
            );
        };
        let out_dir = project_dir.join(menu::PACK_OUT_DIR);
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            return fail(self, format!("{}: {e}", out_dir.display()));
        }
        let out = out_dir.join(menu::pack_out_name(&stem));
        let cart = menu::cart_selection(&root, &out);
        // Only now, with every check past: a flash pressed after this
        // should put the world being emulated on the board rather than the
        // manifest's `default_world`, but a world that could not even be
        // built is not one to aim a 20-minute place-and-route at.
        self.remember_flash_world(&stem);
        Some((
            ggo_common::ProcRequest::emd(&project_dir, menu::world_pack_args(&out, &stem)),
            self.proc_runner.clone(),
            cart,
        ))
    }

    /// **"Re-run (perf)"**: select `rel` and start it, then hop to the
    /// charts panel once the run's perf ingest lands.
    ///
    /// Nothing about the run or the ingest is reimplemented here -- this
    /// is [`Self::run`], which already ends in [`Self::finish_run`] ->
    /// [`ingest`]. The only thing added is the arming of the hop, which
    /// [`Self::run`] stamps with the run generation so a LATER run's
    /// completion can't inherit it.
    ///
    /// Unlike [`Self::open_rel_path`], re-clicking the selected cart is
    /// the whole point, so there is no already-selected early return.
    pub fn rerun(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Before arming: `stop` finishes any run already in flight, and
        // that run must not be the one that gets the hop.
        self.stop(window, cx);
        if self.selected.as_deref() != Some(rel) {
            self.forget_watched_world();
            self.selected = Some(rel.to_string());
            self.frame = 0;
            self.stats = RunStats::default();
        }
        cx.notify();

        let this = cx.weak_entity();
        self._build_task = Some(window.spawn(cx, async move |cx| {
            // `run` needs the project root, and re-discovering it reads
            // the workspace that is mid-update on the way in here.
            this.update(cx, |this, cx| this.refresh_root(cx)).ok();
            this.update_in(cx, |this, window, cx| this.run(window, cx))
                .ok();
        }));
    }

    /// **"Run hardware diagnostics"**: `ggo-diag --launch`, the built-in
    /// GGO Diagnostic Cart.
    ///
    /// No project is involved, and nothing is emulated -- this is the one
    /// entry that talks to real hardware, and on a machine with no board
    /// attached (the normal case) it CANNOT run. That case is the
    /// interesting one: [`menu::diag_request`] returns a message naming
    /// every missing prerequisite and the env var that supplies it, and
    /// this puts that message on the panel's status row after focusing the
    /// dock, so the entry is never a silent no-op. See that function's doc
    /// for the preconditions.
    ///
    /// A real run's transcript lands in the panel's console, the same
    /// place a cart's UART output goes. One-shot, not streamed: a
    /// streaming console with a cancel button is F5.3's (`ggo-ide`'s
    /// Device page is the model), and this pane has neither yet.
    pub fn run_hardware_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop(window, cx);
        self.build_generation += 1;
        let generation = self.build_generation;

        let env = self
            .diag_env_override
            .clone()
            .unwrap_or_else(|| menu::diag_env(self.project_root.as_deref()));
        let request = match menu::diag_request(&env) {
            Ok(request) => request,
            Err(message) => {
                self.report_failure(message, cx);
                return;
            }
        };
        let console = UartLog::new();
        console.push_line(&format!("[diag] $ {}", request.command_line()));
        self.console = Some(console.clone());
        self.status = Some("running hardware diagnostics…".to_string());
        self.status_is_error = false;
        cx.notify();

        let runner = self.proc_runner.clone();
        let this = cx.weak_entity();
        self._build_task = Some(window.spawn(cx, async move |cx| {
            let capture = cx.background_spawn(async move { runner(request) }).await;
            this.update(cx, |this, cx| {
                if this.build_generation != generation {
                    return;
                }
                for line in &capture.lines {
                    console.push_line(line);
                }
                if capture.ok {
                    this.status = Some("hardware diagnostics finished".to_string());
                    this.status_is_error = false;
                    cx.notify();
                } else {
                    this.report_failure(
                        format!(
                            "hardware diagnostics failed: {}",
                            menu::failure_reason(&capture)
                        ),
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    /// Stop the run and blank the pane: signal the thread (which drops the
    /// core on its way out), release the pad, and hand every atlas tile
    /// the pane still owns back to the window -- no further render will
    /// come to retire them through the double buffer. The run's perf
    /// snapshot and console lines are collected off-thread by
    /// [`Self::finish_run`].
    pub(crate) fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() && self.latest_frame.is_none() {
            return;
        }
        // Drop the pump BEFORE finishing: it owns the frame receiver, and
        // dropping that is what makes the emulator thread's next
        // `try_send` fail closed instead of publishing one more frame
        // into a pane that has just been blanked (and re-populating the
        // atlas slots `release_atlas_all` is about to hand back). Safe to
        // do from here because `stop` is only ever a user action -- the
        // pump's own completion path calls `finish_run` directly, never
        // this, so nothing drops the task from inside itself.
        self._pump_task = None;
        self.finish_run(cx);
        self.release_atlas_all(window);
        cx.notify();
    }

    fn is_running(&self) -> bool {
        self.session.is_some()
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.is_paused())
    }

    /// Hidden-tab pause: the emulator item was deactivated. Undone by the
    /// next render (only a visible item renders). A run already paused by
    /// the user is left exactly as it is.
    pub(crate) fn auto_pause(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if session.is_paused() {
            return;
        }
        session.pause();
        self.auto_paused = true;
        cx.notify();
    }

    fn auto_resume(&mut self) {
        if !self.auto_paused {
            return;
        }
        self.auto_paused = false;
        if let Some(session) = &self.session {
            session.resume();
        }
    }

    /// Pause at the next frame boundary, or resume. The flag lives on the
    /// session, so a new run always starts unpaused. A user's toggle
    /// always ends the hidden-tab auto-pause state, whichever way it goes.
    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        self.auto_paused = false;
        if session.is_paused() {
            session.resume();
        } else {
            session.pause();
        }
        cx.notify();
    }

    /// One more frame while paused; a running cart is paused first (a step
    /// from a running state is a pause, never a skipped frame).
    fn step_frame(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if session.is_paused() {
            session.step();
        } else {
            session.pause();
        }
        cx.notify();
    }

    /// The transport's right-hand readout.
    pub(crate) fn transport_readout(&self) -> Option<String> {
        self.session.as_ref().map(|session| {
            format!(
                "{} · frame {}{}",
                session.cart,
                self.frame,
                if session.is_paused() {
                    " · paused"
                } else {
                    ""
                }
            )
        })
    }

    // ----------------------------------------------------------- audio

    /// Mute/unmute. Takes effect on the live run within one cpal callback
    /// (both ends of the ring read the same flag every buffer/frame), and
    /// persists to the NEXT run: the pane never silently un-mutes itself
    /// because a run ended.
    ///
    /// Available with nothing running -- muting before pressing Run is the
    /// obvious way to start a cart quietly, and it costs nothing to allow.
    /// A run whose device never opened has nothing to toggle, so the
    /// button is disabled there (see [`audio::AudioState::is_toggleable`]).
    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if !self.audio.state().is_toggleable() {
            return;
        }
        self.audio.toggle_mute();
        cx.notify();
    }

    // ----------------------------------------------------------- input

    /// Publish the latched mask into the running session. A no-op when
    /// nothing is running, so key handling stays unconditional.
    fn publish_input(&self) {
        if let Some(session) = &self.session {
            session.set_input(self.input.mask());
        }
    }

    fn on_key(&mut self, key: &str, down: bool) {
        if self.input.key(key, down) {
            self.publish_input();
        }
    }

    fn on_shift(&mut self, held: bool) {
        if self.input.set_select(held) {
            self.publish_input();
        }
    }

    fn release_all_buttons(&mut self, _cx: &mut Context<Self>) {
        if self.input.clear() {
            self.publish_input();
        }
    }

    // ----------------------------------------------------------- atlas

    /// The per-render half of the release contract (see the module doc):
    /// retire frame N-2's atlas tiles, keep N-1's alive one more render.
    /// Copied from `livekit_client`'s `RemoteVideoTrackView::render`,
    /// including the `id` guard -- a run that ends leaves the same image
    /// as both current and latest, and dropping it would blank the pane.
    fn retire_atlas_frames(&mut self, window: &mut Window) {
        let Some(latest) = self.latest_frame.clone() else {
            return;
        };
        if let Some(current) = self.current_rendered_frame.take() {
            if let Some(previous) = self.previous_rendered_frame.take()
                && previous.id != current.id
            {
                window.drop_image(previous).log_err();
            }
            self.previous_rendered_frame = Some(current);
        }
        self.current_rendered_frame = Some(latest);
    }

    /// The teardown half: every tile this pane still owns, at once.
    /// `drop_image` on a key that is already gone is a documented no-op
    /// (`gpui_wgpu/src/wgpu_atlas.rs:133`), so the overlap between the
    /// three slots is harmless.
    fn release_atlas_all(&mut self, window: &mut Window) {
        self.release_debug_images(window);
        for image in [
            self.previous_rendered_frame.take(),
            self.current_rendered_frame.take(),
            self.latest_frame.take(),
        ]
        .into_iter()
        .flatten()
        {
            window.drop_image(image).log_err();
        }
    }

    // ----------------------------------------------------------- debug

    fn toggle_debug(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.debug.open = !self.debug.open;
        if !self.debug.open {
            self.release_debug_images(window);
            self.debug.decoded = None;
            self.debug.hover = None;
        }
        cx.notify();
    }

    /// The per-render half of the viewers' atlas release: drop what was
    /// retired before the previous render (see `DebugState::retired`).
    fn retire_debug_images(&mut self, window: &mut Window) {
        for image in self.debug.take_retired_for_render() {
            window.drop_image(image).log_err();
        }
    }

    /// The teardown half: everything, now.
    fn release_debug_images(&mut self, window: &mut Window) {
        self.debug.retire_all();
        for image in self.debug.take_all_retired() {
            window.drop_image(image).log_err();
        }
    }

    fn set_debug_hover(&mut self, hover: String, cx: &mut Context<Self>) {
        if self.debug.hover.as_deref() != Some(hover.as_str()) {
            self.debug.hover = Some(hover);
            cx.notify();
        }
    }

    fn set_debug_tab(&mut self, tab: debug::DebugTab, cx: &mut Context<Self>) {
        self.debug.tab = tab;
        self.debug.hover = None;
        self.debug.last_decoded_ptr = 0;
        cx.notify();
    }

    fn set_debug_selector(
        &mut self,
        bank: usize,
        palette: usize,
        layer: usize,
        cx: &mut Context<Self>,
    ) {
        self.debug.bank = bank & 1;
        self.debug.palette = palette % ggo_emu_core::ppu::PALETTES;
        self.debug.layer = layer % ggo_emu_core::ppu::LAYER_COUNT;
        self.debug.last_decoded_ptr = 0;
        cx.notify();
    }

    /// Called from `render`: if the column is open and a fresh snapshot is
    /// due, decode the active tab off-thread. The identity check makes a
    /// paused run decode once per step, and the throttle keeps a running
    /// one at ~10 Hz.
    fn debug_tick(&mut self, cx: &mut Context<Self>) {
        if !self.debug.open {
            return;
        }
        // After a run ends the pane keeps the last snapshot, so a tab or
        // selector change still re-decodes the right thing.
        let live = self.session.as_ref().and_then(|session| session.snapshot());
        // With no run the snapshot is static, so there is nothing to
        // throttle: a tab or selector change decodes at once, like a pause.
        let immediate = self.is_paused() || live.is_none();
        let Some(snapshot) =
            live.or_else(|| self.debug.decoded.as_ref().map(|d| d.snapshot.clone()))
        else {
            return;
        };
        if !self.debug.decode_due(&snapshot, immediate, Instant::now()) {
            return;
        }
        self.start_debug_decode(snapshot, cx);
    }

    /// The active tab's pixels for `snapshot`, with the image's size.
    /// `None` for Palettes, which paints from the snapshot directly.
    fn decode_debug_tab(
        tab: debug::DebugTab,
        snapshot: &ggo_emu_core::ppu::PpuSnapshot,
        bank: usize,
        palette: usize,
        layer: usize,
    ) -> Option<Arc<RenderImage>> {
        let (bgra, width, height) = match tab {
            debug::DebugTab::Tiles => (
                debug::tile_sheet_bgra(snapshot, bank, palette),
                debug::SHEET_PX,
                debug::SHEET_PX,
            ),
            debug::DebugTab::Map => (
                debug::map_bgra(snapshot, layer),
                debug::MAP_PX,
                debug::MAP_PX,
            ),
            debug::DebugTab::Oam => (
                debug::oam_composite_bgra(snapshot),
                ggo_emu_core::peripherals::SCREEN_WIDTH,
                ggo_emu_core::peripherals::SCREEN_HEIGHT,
            ),
            debug::DebugTab::Palettes => return None,
        };
        image::ImageBuffer::from_raw(width as u32, height as u32, bgra)
            .map(|buffer| Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
    }

    fn start_debug_decode(
        &mut self,
        snapshot: Arc<ggo_emu_core::ppu::PpuSnapshot>,
        cx: &mut Context<Self>,
    ) {
        self.debug.generation += 1;
        let generation = self.debug.generation;
        self.debug.last_decode_started = Some(Instant::now());
        self.debug.last_decoded_ptr = Arc::as_ptr(&snapshot) as usize;
        let tab = self.debug.tab;
        let (bank, palette, layer) = (self.debug.bank, self.debug.palette, self.debug.layer);
        let decode = cx.background_spawn({
            let snapshot = snapshot.clone();
            async move { Self::decode_debug_tab(tab, &snapshot, bank, palette, layer) }
        });
        self.debug.task = Some(cx.spawn(async move |this, cx| {
            let image = decode.await;
            this.update(cx, |this, cx| {
                if this.debug.generation != generation || !this.debug.open {
                    return;
                }
                this.debug.set_decoded(debug::Decoded {
                    tab,
                    image,
                    snapshot,
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Decode `snapshot` for the active tab synchronously -- the tests'
    /// way to exercise the viewers without a running cart.
    #[cfg(test)]
    pub(crate) fn debug_decode_now(&mut self, snapshot: Arc<ggo_emu_core::ppu::PpuSnapshot>) {
        let tab = self.debug.tab;
        let image = Self::decode_debug_tab(
            tab,
            &snapshot,
            self.debug.bank,
            self.debug.palette,
            self.debug.layer,
        );
        self.debug.last_decoded_ptr = Arc::as_ptr(&snapshot) as usize;
        self.debug.set_decoded(debug::Decoded {
            tab,
            image,
            snapshot,
        });
    }

    fn render_debug_column(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let tabs = h_flex()
            .gap_1()
            .p_1()
            .children(debug::DebugTab::ALL.into_iter().map(|tab| {
                Button::new(
                    SharedString::from(format!("ggo-emu-debug-tab-{}", tab.label())),
                    tab.label(),
                )
                .toggle_state(self.debug.tab == tab)
                .on_click(cx.listener(move |this, _event, _window, cx| this.set_debug_tab(tab, cx)))
            }));
        let decoded = self
            .debug
            .decoded
            .as_ref()
            .filter(|d| d.tab == self.debug.tab);
        let body: gpui::AnyElement = match decoded {
            None => Label::new(if self.session.is_some() {
                "decoding…"
            } else {
                "Run a cart to inspect its PPU"
            })
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
            Some(decoded) => match self.debug.tab {
                debug::DebugTab::Tiles => self.render_debug_tiles(decoded, cx),
                debug::DebugTab::Map => self.render_debug_map(decoded, cx),
                debug::DebugTab::Oam => self.render_debug_oam(decoded, cx),
                debug::DebugTab::Palettes => self.render_debug_palettes(decoded, cx),
            },
        };
        v_flex()
            .w(px(DEBUG_COLUMN_PX))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .child(tabs)
            .child(
                div()
                    .id("ggo-emu-debug-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .child(body),
            )
            .children(self.debug.hover.as_ref().map(|hover| {
                Label::new(hover.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .into_any_element()
    }

    /// A fixed-size image canvas whose hover position is reported back
    /// through `on_hover` as pixel coordinates inside the image.
    fn debug_image_canvas(
        &self,
        id: &'static str,
        image: Arc<RenderImage>,
        width: usize,
        height: usize,
        overlay: impl Fn(Bounds<Pixels>, &mut Window) + 'static,
        on_hover: impl Fn(&mut Self, f32, f32, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bounds_cell: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::new(RefCell::new(None));
        let record = bounds_cell.clone();
        let canvas = gpui::canvas(
            move |bounds, _window, _cx| {
                *record.borrow_mut() = Some(bounds);
            },
            move |bounds, _prepaint, window, _cx| {
                window
                    .paint_image(
                        bounds,
                        bounds,
                        gpui::Corners::default(),
                        image.clone(),
                        0,
                        false,
                        true,
                    )
                    .log_err();
                overlay(bounds, window);
            },
        )
        .w(px(width as f32))
        .h(px(height as f32));
        div()
            .id(id)
            .w(px(width as f32))
            .h(px(height as f32))
            .child(canvas)
            .on_mouse_move(
                cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                    if let Some(bounds) = *bounds_cell.borrow() {
                        let x: f32 = (event.position.x - bounds.origin.x).into();
                        let y: f32 = (event.position.y - bounds.origin.y).into();
                        on_hover(this, x, y, cx);
                    }
                }),
            )
            .into_any_element()
    }

    fn debug_stepper(
        &self,
        id: &'static str,
        label: String,
        on_delta: impl Fn(&mut Self, isize, &mut Context<Self>) + 'static + Clone,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let down = on_delta.clone();
        h_flex()
            .gap_1()
            .items_center()
            .child(
                IconButton::new(
                    SharedString::from(format!("{id}-down")),
                    IconName::ChevronLeft,
                )
                .icon_size(IconSize::XSmall)
                .on_click(cx.listener(move |this, _event, _window, cx| down(this, -1, cx))),
            )
            .child(Label::new(label).size(LabelSize::Small))
            .child(
                IconButton::new(
                    SharedString::from(format!("{id}-up")),
                    IconName::ChevronRight,
                )
                .icon_size(IconSize::XSmall)
                .on_click(cx.listener(move |this, _event, _window, cx| on_delta(this, 1, cx))),
            )
            .into_any_element()
    }

    fn render_debug_tiles(
        &self,
        decoded: &debug::Decoded,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bank_label = format!(
            "bank {}",
            if self.debug.bank == ggo_emu_core::ppu::BANK_SPRITE {
                "sprite"
            } else {
                "bg/fg"
            }
        );
        let palette_label = format!("palette {}", self.debug.palette);
        let selectors = h_flex()
            .gap_3()
            .px_1()
            .child(self.debug_stepper(
                "ggo-emu-debug-bank",
                bank_label,
                |this, _delta, cx| {
                    let (bank, palette, layer) =
                        (this.debug.bank ^ 1, this.debug.palette, this.debug.layer);
                    this.set_debug_selector(bank, palette, layer, cx)
                },
                cx,
            ))
            .child(self.debug_stepper(
                "ggo-emu-debug-palette",
                palette_label,
                |this, delta, cx| {
                    let palettes = ggo_emu_core::ppu::PALETTES as isize;
                    let palette =
                        (this.debug.palette as isize + delta).rem_euclid(palettes) as usize;
                    let (bank, layer) = (this.debug.bank, this.debug.layer);
                    this.set_debug_selector(bank, palette, layer, cx)
                },
                cx,
            ));
        let Some(image) = decoded.image.clone() else {
            return selectors.into_any_element();
        };
        let sheet = self.debug_image_canvas(
            "ggo-emu-debug-tiles",
            image,
            debug::SHEET_PX,
            debug::SHEET_PX,
            |_, _| {},
            |this, x, y, cx| {
                let tile_px = ggo_emu_core::ppu::TILE_PX as f32;
                let span = debug::SHEET_PX as f32;
                if x >= 0.0 && y >= 0.0 && x < span && y < span {
                    let index = (y / tile_px) as usize * debug::SHEET_TILES_PER_ROW
                        + (x / tile_px) as usize;
                    this.set_debug_hover(format!("tile {index}"), cx);
                }
            },
            cx,
        );
        v_flex()
            .gap_1()
            .child(selectors)
            .child(sheet)
            .into_any_element()
    }

    fn render_debug_map(
        &self,
        decoded: &debug::Decoded,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let labels = debug::layer_labels(&decoded.snapshot);
        let selectors = h_flex()
            .gap_1()
            .px_1()
            .children((0..ggo_emu_core::ppu::LAYER_COUNT).map(|layer| {
                let enabled = decoded.snapshot.layer_enable[layer];
                Button::new(
                    SharedString::from(format!("ggo-emu-debug-layer-{layer}")),
                    SharedString::from(labels[layer].clone()),
                )
                .toggle_state(self.debug.layer == layer)
                .color(if enabled {
                    Color::Default
                } else {
                    Color::Muted
                })
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    let (bank, palette) = (this.debug.bank, this.debug.palette);
                    this.set_debug_selector(bank, palette, layer, cx)
                }))
            }));
        let Some(image) = decoded.image.clone() else {
            return selectors.into_any_element();
        };
        let layer = self.debug.layer % ggo_emu_core::ppu::LAYER_COUNT;
        let (scroll_x, scroll_y) = decoded.snapshot.scroll[layer];
        let accent = cx.theme().colors().border_focused;
        let hover_snapshot = decoded.snapshot.clone();
        let map = self.debug_image_canvas(
            "ggo-emu-debug-map",
            image,
            debug::MAP_PX,
            debug::MAP_PX,
            move |bounds, window| {
                // The 320x240 window the screen shows, at the layer's
                // scroll, wrapping at the map edge like the hardware.
                let (w, h) = (
                    ggo_emu_core::peripherals::SCREEN_WIDTH as f32,
                    ggo_emu_core::peripherals::SCREEN_HEIGHT as f32,
                );
                let span = debug::MAP_PX as f32;
                let sx = (scroll_x as f32) % span;
                let sy = (scroll_y as f32) % span;
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    for dx in [0.0, -span] {
                        for dy in [0.0, -span] {
                            let rect = Bounds::new(
                                point(bounds.origin.x + px(sx + dx), bounds.origin.y + px(sy + dy)),
                                size(px(w), px(h)),
                            );
                            window.paint_quad(gpui::outline(
                                rect,
                                accent,
                                gpui::BorderStyle::Solid,
                            ));
                        }
                    }
                });
            },
            move |this, x, y, cx| {
                let tile_px = ggo_emu_core::ppu::TILE_PX as f32;
                let span = debug::MAP_PX as f32;
                if x >= 0.0 && y >= 0.0 && x < span && y < span {
                    let (cell_x, cell_y) = ((x / tile_px) as usize, (y / tile_px) as usize);
                    let cell = hover_snapshot.map_cell(layer, cell_x, cell_y);
                    this.set_debug_hover(
                        format!(
                            "cell ({cell_x},{cell_y})  tile {}  pal {}  {}{}",
                            cell.tile,
                            cell.palette,
                            if cell.hflip { "H" } else { "-" },
                            if cell.vflip { "V" } else { "-" }
                        ),
                        cx,
                    );
                }
            },
            cx,
        );
        v_flex()
            .gap_1()
            .child(selectors)
            .child(map)
            .into_any_element()
    }

    fn render_debug_oam(
        &self,
        decoded: &debug::Decoded,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let rows = debug::oam_rows(&decoded.snapshot);
        let enabled = rows.iter().filter(|(_, entry)| entry.enabled).count();
        let mut column = v_flex().gap_1().child(
            Label::new(format!(
                "{enabled} of {} enabled",
                ggo_emu_core::ppu::OAM_ENTRIES
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        );
        if let Some(image) = decoded.image.clone() {
            column = column.child(self.debug_image_canvas(
                "ggo-emu-debug-oam",
                image,
                ggo_emu_core::peripherals::SCREEN_WIDTH,
                ggo_emu_core::peripherals::SCREEN_HEIGHT,
                |_, _| {},
                |this, x, y, cx| {
                    this.set_debug_hover(format!("({}, {})", x as i32, y as i32), cx);
                },
                cx,
            ));
        }
        column
            .children(rows.into_iter().map(|(index, entry)| {
                Label::new(debug::oam_row_label(index, &entry))
                    .size(LabelSize::XSmall)
                    .color(if entry.enabled {
                        Color::Default
                    } else {
                        Color::Muted
                    })
                    .buffer_font(cx)
            }))
            .into_any_element()
    }

    fn render_debug_palettes(
        &self,
        decoded: &debug::Decoded,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let snapshot = decoded.snapshot.clone();
        let paint_snapshot = snapshot.clone();
        let swatch = DEBUG_SWATCH_PX;
        let width = swatch * ggo_emu_core::ppu::PAL_ENTRIES as f32;
        let height = swatch * 2.0 * ggo_emu_core::ppu::PALETTES as f32;
        let bounds_cell: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::new(RefCell::new(None));
        let record = bounds_cell.clone();
        let grid = gpui::canvas(
            move |bounds, _window, _cx| {
                *record.borrow_mut() = Some(bounds);
            },
            move |bounds, _prepaint, window, _cx| {
                for bank in 0..2 {
                    for palette in 0..ggo_emu_core::ppu::PALETTES {
                        for entry in 0..ggo_emu_core::ppu::PAL_ENTRIES {
                            let rgb = paint_snapshot.palette_rgb565(bank, palette, entry);
                            let argb = ggo_emu_core::peripherals::rgb565_to_argb(rgb);
                            let color = gpui::rgb(argb & 0x00FF_FFFF);
                            let row = bank * ggo_emu_core::ppu::PALETTES + palette;
                            let rect = Bounds::new(
                                point(
                                    bounds.origin.x + px(entry as f32 * swatch),
                                    bounds.origin.y + px(row as f32 * swatch),
                                ),
                                size(px(swatch), px(swatch)),
                            );
                            window.paint_quad(gpui::fill(rect, color));
                        }
                    }
                }
            },
        )
        .w(px(width))
        .h(px(height));
        div()
            .id("ggo-emu-debug-palettes")
            .w(px(width))
            .h(px(height))
            .child(grid)
            .on_mouse_move(
                cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                    if let Some(bounds) = *bounds_cell.borrow() {
                        let x: f32 = (event.position.x - bounds.origin.x).into();
                        let y: f32 = (event.position.y - bounds.origin.y).into();
                        if let Some((bank, palette, entry)) = debug::palette_cell_at(x, y, swatch) {
                            let rgb = snapshot.palette_rgb565(bank, palette, entry);
                            this.set_debug_hover(
                                format!(
                                    "{} pal {palette} slot {entry} = {}",
                                    if bank == ggo_emu_core::ppu::BANK_SPRITE {
                                        "sprite"
                                    } else {
                                        "bg/fg"
                                    },
                                    debug::rgb565_label(rgb)
                                ),
                                cx,
                            );
                        }
                    }
                }),
            )
            .into_any_element()
    }

    // ---------------------------------------------------------- render

    fn render_transport(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let running = self.is_running();
        let paused = self.is_paused();
        let label: SharedString = match &self.selected {
            Some(cart) => cart.clone().into(),
            None => EMPTY_MESSAGE.into(),
        };

        h_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Button::new(
                    "ggo-emu-watch",
                    if self.watch { "Watching" } else { "Watch" },
                )
                .toggle_state(self.watch)
                .disabled(self.watched_world.is_none())
                .tooltip(Tooltip::text(match &self.watched_world {
                    Some(world) => format!("Re-pack and restart {world} on every save"),
                    None => "Emulate a world first".to_string(),
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    let on = !this.watch;
                    this.set_watch(on, window, cx)
                })),
            )
            .when(self.watch && self.watch_rebuilds > 0, |el| {
                el.child(
                    Label::new(format!("rebuilt {}×", self.watch_rebuilds))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(
                IconButton::new("ggo-emu-run", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .disabled(self.selected.is_none())
                    .tooltip(Tooltip::text("Run cart"))
                    .on_click(cx.listener(|this, _event, window, cx| this.run(window, cx))),
            )
            .child(
                IconButton::new("ggo-emu-stop", IconName::Stop)
                    .icon_size(IconSize::Small)
                    .disabled(!running)
                    .tooltip(Tooltip::text("Stop"))
                    .on_click(cx.listener(|this, _event, window, cx| this.stop(window, cx))),
            )
            .child(
                Button::new("ggo-emu-pause", if paused { "Resume" } else { "Pause" })
                    .disabled(!running)
                    .tooltip(Tooltip::text(
                        "Pause / resume at the next frame (ctrl-alt-p)",
                    ))
                    .on_click(cx.listener(|this, _event, _window, cx| this.toggle_pause(cx))),
            )
            .child(
                Button::new("ggo-emu-step", "Step")
                    .disabled(!running)
                    .tooltip(Tooltip::text("Run one frame (ctrl-alt-.)"))
                    .on_click(cx.listener(|this, _event, _window, cx| this.step_frame(cx))),
            )
            .child({
                let flashing = self.is_flashing();
                if flashing {
                    // One click to cancel, no menu in the way.
                    IconButton::new("ggo-emu-flash", IconName::GgoFlashRun)
                        .icon_size(IconSize::Small)
                        .toggle_state(true)
                        .tooltip(Tooltip::text("Cancel the flash"))
                        .on_click(
                            cx.listener(|this, _event, window, cx| this.flash_to_board(window, cx)),
                        )
                        .into_any_element()
                } else {
                    // A menu, not a one-click flash: the plain flash
                    // reuses the cached bitstream, and pressing the wrong
                    // one costs either a stale board or a ~20-minute
                    // place-and-route.
                    let weak = cx.weak_entity();
                    ui::PopoverMenu::new("ggo-emu-flash-menu")
                        .trigger(
                            IconButton::new("ggo-emu-flash", IconName::GgoFlashRun)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(ggo_common::flash_tooltip(
                                    "Flash this project to the board",
                                    self.flash_world(cx).as_deref(),
                                ))),
                        )
                        .menu(move |window, cx| {
                            let weak = weak.clone();
                            Some(ui::ContextMenu::build(
                                window,
                                cx,
                                move |menu, _window, _cx| {
                                    let flash = weak.clone();
                                    let rebuild = weak;
                                    menu.entry(
                                        "Flash now (cached gateware)",
                                        None,
                                        move |window, cx| {
                                            flash
                                                .update(cx, |this, cx| {
                                                    this.flash_to_board_with(
                                                        None, false, window, cx,
                                                    )
                                                })
                                                .ok();
                                        },
                                    )
                                    .entry(
                                        "Flash + rebuild gateware (~20 min)",
                                        None,
                                        move |window, cx| {
                                            rebuild
                                                .update(cx, |this, cx| {
                                                    this.flash_to_board_with(None, true, window, cx)
                                                })
                                                .ok();
                                        },
                                    )
                                },
                            ))
                        })
                        .into_any_element()
                }
            })
            .children(
                self.flash_status()
                    .map(|text| Label::new(text).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .children(
                // Offered whenever this machine cannot flash yet -- not
                // only after a failed press.
                (!self.is_flashing() && !self.hardware_env_cached().ready()).then(|| {
                    Button::new("ggo-emu-hardware-setup", "Set up hardware tooling")
                        .tooltip(Tooltip::text(
                            "What flashing needs, and install the missing parts",
                        ))
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.open_hardware_page(window, cx)
                        }))
                }),
            )
            .child(self.render_mute_button(cx))
            .child(
                Button::new("ggo-emu-debug", "Debug")
                    .toggle_state(self.debug.open)
                    .tooltip(Tooltip::text(
                        "Tiles / map / OAM / palette viewers (ctrl-alt-d)",
                    ))
                    .on_click(
                        cx.listener(|this, _event, window, cx| this.toggle_debug(window, cx)),
                    ),
            )
            // The selected cart, as plain text: the file explorer is the
            // picker now, so there is nothing here to click.
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(if self.selected.is_some() {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .child(div().flex_1())
            // The "is it actually running" readout: the cart's own frame
            // counter, straight off the last `EmuMsg::Frame`.
            .children(self.transport_readout().map(|readout| {
                Label::new(readout)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .into_any_element()
    }

    /// Mute/unmute, sitting with Run and Stop because it is transport, not
    /// a setting. The icon shows the CURRENT state (speaker crossed out
    /// when silent) rather than the action, which is the convention the
    /// rest of Zed's audio controls follow.
    ///
    /// A machine with no output device gets a disabled button whose
    /// tooltip is cpal's own reason -- "muted with a legible reason", which
    /// is the whole degraded contract. The stats row carries the same
    /// reason as text, so it is readable without hovering.
    fn render_mute_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.audio.state();
        let silent = self.audio.is_muted() || !state.is_toggleable();
        let tooltip: SharedString = match &state {
            audio::AudioState::Unavailable(reason) => format!("No audio output: {reason}").into(),
            _ if self.audio.is_muted() => "Unmute".into(),
            _ => "Mute".into(),
        };
        IconButton::new(
            "ggo-emu-mute",
            if silent {
                IconName::AudioOff
            } else {
                IconName::AudioOn
            },
        )
        .icon_size(IconSize::Small)
        .disabled(!state.is_toggleable())
        .tooltip(Tooltip::text(tooltip))
        .on_click(cx.listener(|this, _event, _window, cx| this.toggle_mute(cx)))
        .into_any_element()
    }

    /// The pane itself: the framebuffer integer-scaled to the dock's
    /// size, or a message. Painted through a `canvas` at the bounds
    /// [`scaled_frame_bounds`] picks rather than a stretched
    /// `w_full`/`h_full` img: at an integer multiple on whole-pixel
    /// origins the sampler lands exactly on texel centers, so the
    /// upscale is crisp (the panel's answer to the standalone binary's
    /// `--scale` + `scale_nearest`).
    fn render_screen(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(frame) = &self.latest_frame {
            let frame = frame.clone();
            return div()
                .size_full()
                .bg(cx.theme().colors().panel_background)
                .child(
                    gpui::canvas(
                        |_bounds, _window, _cx| {},
                        move |bounds, _prepaint, window, _cx| {
                            let b = scaled_frame_bounds(bounds);
                            // A zero-area panel has nothing to paint into;
                            // paint_image only fails on that degenerate
                            // clip, so log-and-drop beats poisoning the
                            // frame with an unwrap.
                            window
                                .paint_image(b, b, gpui::Corners::default(), frame, 0, false, true)
                                .log_err();
                        },
                    )
                    .size_full(),
                )
                .into_any_element();
        }
        let message = match (self.is_running(), &self.selected) {
            (true, _) => "Starting…",
            (false, None) => EMPTY_MESSAGE,
            (false, Some(_)) => "Press Run",
        };
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }

    /// The live counters row -- `ggo-ide`'s `State::status_text` fps /
    /// drops / step-time triple, plus the audio segment ggo-ide keeps in
    /// the same string. Each half appears on its own terms: the frame
    /// counters once a run has produced a frame (an all-zero row before
    /// that is noise), the audio segment once a run has opened -- or
    /// failed to open -- a device.
    ///
    /// The underrun count is not decoration. It is the only signal that
    /// distinguishes "audio is working" from "audio is technically
    /// playing, from a ring the emulator cannot fill fast enough", which
    /// is what a dropped frame sounds like; a run whose count climbs is a
    /// run whose pacing is losing.
    fn render_stats(&self) -> Option<gpui::AnyElement> {
        // Once, not once per use: this runs on every render (up to 60 Hz
        // while a cart is going), and `state()` takes a lock and clones a
        // string.
        let audio = self.audio.state();
        let mut parts = Vec::new();
        if self.frame > 0 || self.stats.dropped > 0 {
            parts.push(self.stats.label());
        }
        // The panel is the one authority on whether a run is live -- the
        // status deliberately does not track it (that is run-scoped state,
        // and keeping it there is what broke restarts). Without this an
        // idle pane would keep advertising "audio on" with no device open.
        parts.extend(audio.label(self.is_running()));
        (!parts.is_empty()).then(|| {
            Label::new(parts.join(" · "))
                .size(LabelSize::XSmall)
                // A run that lost its audio is worth noticing but is not a
                // failure -- Warning, not Error, which the status line
                // below reserves for things the user has to act on.
                .color(match audio {
                    audio::AudioState::Unavailable(_) => Color::Warning,
                    _ => Color::Muted,
                })
                .into_any_element()
        })
    }

    /// The run's diagnostic console: a collapsed one-line toggle, and a
    /// scrollable tail when expanded. Ported from `ggo-ide`'s
    /// `State::console_view` (which is itself the native port of
    /// `EmulatorPage.tsx`'s "Console (N lines)" panel), including its
    /// always-available-but-collapsed default and its
    /// [`LIVE_CONSOLE_TAIL_LINES`] cap on what gets laid out.
    ///
    /// See [`uart`]'s module doc for what lands here in cart mode: the
    /// driver's own per-run markers interleaved with the cart's own
    /// `log()` output, via the `Peripherals::log_sink` [`crate::drive`]
    /// attaches and drains every turn -- the same pairing ggo-ide's cart
    /// runner uses.
    fn render_console(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let log = self.console.as_ref()?;
        if log.is_empty() {
            return None;
        }
        // The header counts every line the run has logged (and that the
        // ingest will carry); the body lays out only the newest
        // `LIVE_CONSOLE_TAIL_LINES` of them.
        let total = log.len();
        let lines = log.peek_tail(LIVE_CONSOLE_TAIL_LINES);
        let arrow = if self.console_expanded { "▾" } else { "▸" };
        let toggle = Button::new(
            "ggo-emu-console",
            format!("{arrow} Console ({total} lines)"),
        )
        .label_size(LabelSize::XSmall)
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.console_expanded = !this.console_expanded;
            cx.notify();
        }));
        // Copy the WHOLE held log (not just the rendered tail) so a
        // failure's full context can be pasted anywhere.
        let copy_all = {
            let log = log.clone();
            IconButton::new("ggo-emu-console-copy", IconName::Copy)
                .icon_size(IconSize::XSmall)
                .tooltip(ui::Tooltip::text("Copy console log"))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(log.lines().join("\n")));
                })
        };

        let body = self.console_expanded.then(|| {
            v_flex()
                .id("ggo-emu-console-lines")
                .max_h(CONSOLE_HEIGHT)
                .overflow_y_scroll()
                .px_1()
                .children(lines.into_iter().map(|line| {
                    Label::new(line)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .into_any_element()
                }))
        });

        Some(
            v_flex()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    h_flex()
                        .items_center()
                        .child(toggle)
                        .child(div().flex_1())
                        .child(copy_all),
                )
                .children(body)
                .into_any_element(),
        )
    }
}

impl Render for EmuPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // BEFORE building this frame's elements: the image about to be
        // painted becomes N, so N-2 is now safe to hand back. See the
        // module doc.
        self.retire_atlas_frames(window);
        self.retire_debug_images(window);
        // Rendering means the tab is visible again.
        self.auto_resume();
        // A deactivated window delivers no key-up: alt-tabbing away with a
        // button held would otherwise leave it latched until the same key
        // is pressed again. Every render while inactive releases the pad;
        // frames keep the renders coming.
        if !window.is_window_active() {
            self.release_all_buttons(cx);
        }
        self.debug_tick(cx);

        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &Run, window, cx| this.run(window, cx)))
            .on_action(cx.listener(|this, _: &Stop, window, cx| this.stop(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleMute, _window, cx| this.toggle_mute(cx)))
            .on_action(cx.listener(|this, _: &TogglePause, _window, cx| this.toggle_pause(cx)))
            .on_action(cx.listener(|this, _: &StepFrame, _window, cx| this.step_frame(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleDebug, window, cx| this.toggle_debug(window, cx)),
            )
            // The pad. Scoped by focus, not by keymap: these fire only
            // while the pane's focus handle owns the keyboard, so typing
            // `z` anywhere else never reaches a cart.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, _cx| {
                this.on_key(event.keystroke.key.as_str(), true);
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _window, _cx| {
                this.on_key(event.keystroke.key.as_str(), false);
            }))
            // SELECT: a modifier-only press produces no keystroke, so the
            // shift state has to come from here. See `input`'s module doc
            // for why this is either shift rather than the right one.
            .on_modifiers_changed(cx.listener(
                |this, event: &ModifiersChangedEvent, _window, _cx| {
                    this.on_shift(event.modifiers.shift);
                },
            ))
            .child(self.render_transport(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .size_full()
                            .child(self.render_screen(cx)),
                    )
                    .children(self.debug.open.then(|| self.render_debug_column(cx))),
            )
            .children(self.render_stats())
            .children(self.status.as_ref().map(|status| {
                ggo_common::CopyableText::new("ggo-emu-status-copy", status.clone())
                    .size(LabelSize::Small)
                    .color(if self.status_is_error {
                        Color::Error
                    } else {
                        Color::Muted
                    })
            }))
            .children(self.ingest_status.label().map(|label| {
                ggo_common::CopyableText::new("ggo-emu-ingest-copy", label)
                    .size(LabelSize::XSmall)
                    .color(if matches!(self.ingest_status, IngestStatus::Failed(_)) {
                        Color::Error
                    } else {
                        Color::Muted
                    })
            }))
            .children(self.render_console(cx))
    }
}

impl Focusable for EmuPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EmuPanel {
    /// The selected cart's file stem, for the tab title.
    pub fn selected_cart_stem(&self) -> Option<String> {
        let rel = self.selected.as_ref()?;
        std::path::Path::new(rel)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    }

    /// A workspace-less panel in a real test window -- the shape
    /// `TestAppContext::add_window_view` wants. Tests that don't need a
    /// window call `Self::new(None, None, cx)` directly.
    #[cfg(test)]
    fn test_new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(None, Some(window), cx)
    }

    // ------------------------------------------------- test-support hooks

    /// A cart is selected and its root has resolved, so `Run` has
    /// everything it needs. `test-support` only, for `ggo_smoke`'s
    /// emulator journeys -- `selected` and `project_root` are private.
    #[cfg(feature = "test-support")]
    pub fn test_is_ready(&self) -> bool {
        self.selected.is_some() && self.project_root.is_some()
    }

    /// A run is in flight. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_is_running(&self) -> bool {
        self.is_running()
    }

    /// Status row for `agent_remote`'s `status` command.
    pub(crate) fn remote_status(&self, workspace: String) -> ggo_emu_remote::protocol::WorkspaceStatus {
        ggo_emu_remote::protocol::WorkspaceStatus {
            workspace,
            cart: self.session.as_ref().map(|s| s.cart.clone()).or_else(|| self.selected.clone()),
            running: self.session.is_some(),
            paused: self.is_paused(),
            frame: self.frame,
        }
    }

    /// Select `cart` (project-relative path) and start it — the remote
    /// analog of explorer-select + Run. `root` is the caller's live
    /// project root: a panel opened BY the remote boot hasn't run
    /// `refresh_root` yet (it defers, and cannot read the workspace
    /// mid-update), so the dispatcher supplies the root it resolved.
    pub(crate) fn remote_boot(
        &mut self,
        cart: String,
        root: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.project_root.is_none() {
            self.project_root = Some(root);
        }
        let abs = self.project_root.as_ref().expect("set above").join(&cart);
        if !abs.is_file() {
            return Err(format!(
                "no cart at {} — pack one first (emd pack-ggo [--world <stem>])",
                abs.display()
            ));
        }
        // Same bookkeeping as `open_rel_path`: a live world-watch would
        // otherwise rebuild ITS cart over the agent's run on the next save.
        self.forget_watched_world();
        self.selected = Some(cart);
        self.run(window, cx);
        match (&self.status_is_error, &self.status) {
            (true, Some(status)) => Err(status.clone()),
            _ => Ok(()),
        }
    }

    /// Flash `world` to the board -- `agent_remote`'s `flash_world`, and
    /// the same press the toolbar's "Flash now" is, page and report hop
    /// included. The call RETURNS as soon as the run is spawned: a flash
    /// is minutes (a gateware rebuild, twenty), so the caller polls
    /// [`Self::remote_flash_status`] instead of holding the socket open.
    ///
    /// Unlike the button this never cancels: an agent asking to flash
    /// while one is running has lost track of the board, and killing a
    /// half-written flash on its behalf is not a reasonable reading of
    /// "flash this world".
    /// Returns the configuration the flash actually runs with -- every
    /// default filled -- for the agent's reply.
    pub(crate) fn remote_flash(
        &mut self,
        config: hardware::FlashConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<hardware::FlashConfig, String> {
        self.start_flash(config, window, cx)
    }

    /// What the board run in flight -- else the last one to end -- has
    /// reached, for `agent_remote`'s `flash_status`.
    pub(crate) fn remote_flash_status(&self) -> ggo_emu_remote::protocol::FlashStatusPayload {
        use ggo_emu_remote::protocol::{FlashDiagStep, FlashPhase};
        let Some((progress, elapsed)) = self.flash_progress() else {
            return ggo_emu_remote::protocol::FlashStatusPayload::default();
        };
        let diag_run_id = progress.diag_run_id.clone();
        let phases = progress
            .rows()
            .iter()
            .map(|row| FlashPhase {
                title: row.title.clone(),
                state: match row.state {
                    hardware::PhaseState::Pending => "pending",
                    hardware::PhaseState::Running => "running",
                    hardware::PhaseState::Done => "done",
                    hardware::PhaseState::Failed => "failed",
                }
                .to_string(),
                elapsed_s: row.elapsed(elapsed).as_secs(),
                detail: row.detail.clone(),
            })
            .collect();
        let diag_steps = progress
            .diag_steps()
            .iter()
            .map(|step| FlashDiagStep { index: step.index.clone(), status: step.status.clone() })
            .collect();
        // This run's lines only, and the last 20 of those; the transcript
        // path is there for the rest. ponytail: a console REPLACED by a
        // later emulator boot still shows through here -- track the
        // console's identity too if that ever misleads.
        let lines = self.console_lines();
        let from = progress.console_from.min(lines.len());
        let console_tail = lines[from.max(lines.len().saturating_sub(20))..].to_vec();
        ggo_emu_remote::protocol::FlashStatusPayload {
            active: self.is_flashing(),
            phase: progress.current_phase().map(|row| row.title.clone()),
            verdict: progress.verdict(),
            what: progress.what.clone(),
            elapsed_s: Some(elapsed.as_secs()),
            detail: progress.running().and_then(|row| row.detail.clone()),
            phases,
            diag_steps,
            failure: progress.failure.clone(),
            transcript: progress.transcript.as_ref().map(|path| path.display().to_string()),
            console_tail,
            // Resolved by the post-PASS hop, not here: translating
            // ggo-diag's run id blocks on two database calls, and this
            // runs on the UI thread. Handed out only when the stash names
            // the very run this payload is reporting -- so a report id
            // stashed for an earlier flash cannot ride along beside a
            // later run's timeline, whatever order the hops landed in.
            perf_run_id: self
                .last_flash_perf_run
                .as_ref()
                .filter(|(run, _)| Some(run.as_str()) == diag_run_id.as_deref())
                .map(|(_, local_id)| *local_id),
            diag_run_id,
        }
    }

    /// The board-readiness probe, re-run now: plugging the board in
    /// changes the answer, and the agent asks exactly when it wonders.
    pub(crate) fn remote_env(&mut self) -> ggo_emu_remote::protocol::HwEnvPayload {
        self.invalidate_hardware();
        self.hardware_env_cached().remote_payload()
    }

    /// The button's cancel, for the agent. `false` when nothing was running.
    pub(crate) fn remote_flash_cancel(&mut self, cx: &mut Context<Self>) -> bool {
        self.cancel_flash(cx)
    }

    pub(crate) fn remote_stop(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        self.stop(window, cx);
        Ok(())
    }

    fn remote_session(&self) -> Result<&drive::Session, String> {
        self.session.as_ref().ok_or_else(|| "no run live — boot a cart first".to_string())
    }

    /// Latch the pad mask (level-triggered, exactly like held keys). The
    /// panel's own InputState is updated too, so every later
    /// `publish_input` (watch restart, focus churn) republishes this mask
    /// instead of silently zeroing it mid-probe.
    pub(crate) fn remote_input(&mut self, mask: u32) -> Result<(), String> {
        self.input.set_mask(mask);
        self.remote_session()?.set_input(mask);
        Ok(())
    }

    /// Pause like `toggle_pause` does: clearing `auto_paused` so a later
    /// tab re-activation's `auto_resume` cannot undo an explicit pause.
    pub(crate) fn remote_pause(&mut self) -> Result<(), String> {
        self.auto_paused = false;
        self.remote_session()?.pause();
        Ok(())
    }

    /// While paused, queue exactly `frames` more frames.
    pub(crate) fn remote_step(&self, frames: u32) -> Result<(), String> {
        let session = self.remote_session()?;
        if !session.is_paused() {
            return Err("not paused — pause first, then step".to_string());
        }
        for _ in 0..frames {
            session.step();
        }
        Ok(())
    }

    /// Progress probe for the dispatcher's boot/step waits: the last
    /// delivered frame number, and the failure status if the run died.
    pub(crate) fn remote_progress(&self) -> (u32, bool, Option<String>) {
        (self.frame, self.session.is_some(), self.status_is_error.then(|| {
            self.status.clone().unwrap_or_else(|| "run failed".to_string())
        }))
    }

    /// Arm the cart's world-inspection tap — lock-step (remote) runs
    /// only; ordinary Run leaves the cart serializing nothing.
    pub(crate) fn remote_enable_inspect(&self) -> Result<(), String> {
        self.remote_session()?.set_inspect(true);
        Ok(())
    }

    /// The cart's world-inspection JSON `(tap seq, json)`, once armed.
    pub(crate) fn remote_world_json(&self) -> Option<(u32, std::sync::Arc<String>)> {
        self.session.as_ref().and_then(|s| s.world_json())
    }

    /// The last delivered frame as (width, height, BGRA8 bytes).
    pub(crate) fn remote_screenshot(&self) -> Option<(u32, u32, Vec<u8>)> {
        let bytes = self.latest_frame.as_ref()?.as_bytes(0)?;
        Some((drive::WIDTH, drive::HEIGHT, bytes.to_vec()))
    }

    /// Tail of the run's diagnostic log (whole log when `tail` is None).
    pub(crate) fn remote_uart(&self, tail: Option<usize>) -> Vec<String> {
        let lines = self.console.as_ref().map(|c| c.lines()).unwrap_or_default();
        match tail {
            Some(n) if n < lines.len() => lines[lines.len() - n..].to_vec(),
            _ => lines,
        }
    }

    /// The live run is paused. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_is_paused(&self) -> bool {
        self.is_paused()
    }

    /// The number of the last frame the emulator thread delivered -- the
    /// count the transport readout prints. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_frame(&self) -> u32 {
        self.frame
    }

    /// One pixel of the last delivered frame, BGRA8, in framebuffer
    /// coordinates -- the proof that a cart's own output reached the
    /// pane, through the real PPU compose and the real RGB565 -> BGRA
    /// conversion. `None` before the first frame and after `Stop` (which
    /// hands the frame's atlas tiles back). `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_frame_pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= drive::WIDTH || y >= drive::HEIGHT {
            return None;
        }
        let bytes = self.latest_frame.as_ref()?.as_bytes(0)?;
        let offset = ((y * drive::WIDTH + x) * 4) as usize;
        bytes.get(offset..offset + 4)?.try_into().ok()
    }

    /// The status row's message: a run's exit reason (including a cart
    /// that failed to load) or a refused precondition. `test-support`
    /// only.
    #[cfg(feature = "test-support")]
    pub fn test_status(&self) -> Option<String> {
        self.status.clone()
    }

    /// Whether [`Self::test_status`]'s message is shown as an error
    /// rather than as a run's ordinary exit reason. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_status_is_error(&self) -> bool {
        self.status_is_error
    }

    /// Point the perf ingest at `path` instead of `~/.ggo/ggo_ide.db`.
    ///
    /// The one WRITE hook here, and load-bearing rather than convenient:
    /// a journey that runs a real cart to completion would otherwise
    /// write a `run` row into the developer's actual database -- exactly
    /// what `db_path_override` exists to prevent for this crate's own
    /// tests, which set the same field directly. It redirects a
    /// destination; it changes no emulator state. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_set_db_path(&mut self, path: PathBuf) {
        self.db_path_override = Some(path);
    }
}

/// The hand-assembled cart fixtures `drive`'s and this module's tests
/// run, re-exported for `ggo_smoke`'s emulator journeys: there is no
/// committed `.cart` anywhere in the fork, so a cross-crate test has no
/// other way to get real machine code in front of the panel. `drive`
/// itself stays private.
#[cfg(any(test, feature = "test-support"))]
pub use drive::fixture;

/// Where the framebuffer lands inside `panel`: the largest aspect-true
/// fit of 320x240, centered on floored whole-pixel origins -- the screen
/// fills the center pane it now lives in. Fractional scale by choice:
/// with the emulator as a full center tab, an integer snap could leave a
/// third of the area as letterbox, and filling the pane is worth the
/// slight sampling softness (nearest sampling keeps edges serviceable).
fn scaled_frame_bounds(panel: gpui::Bounds<Pixels>) -> gpui::Bounds<Pixels> {
    let frame_w = drive::WIDTH as f32;
    let frame_h = drive::HEIGHT as f32;
    let panel_w = f32::from(panel.size.width);
    let panel_h = f32::from(panel.size.height);
    let scale = (panel_w / frame_w).min(panel_h / frame_h);
    let w = frame_w * scale;
    let h = frame_h * scale;
    let x = ((panel_w - w) / 2.0).floor();
    let y = ((panel_h - h) / 2.0).floor();
    gpui::Bounds::new(
        gpui::point(panel.origin.x + gpui::px(x), panel.origin.y + gpui::px(y)),
        gpui::size(gpui::px(w), gpui::px(h)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    // ------------------------------------------------- viewport scaling

    /// A panel bigger than the framebuffer gets the largest INTEGER
    /// multiple that fits both axes, centered on whole-pixel origins --
    /// integer scale on aligned bounds is what keeps sprites crisp.
    #[test]
    fn scaled_frame_bounds_fills_the_pane_preserving_aspect() {
        let panel = gpui::Bounds::new(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(1000.), gpui::px(800.)),
        );
        let b = scaled_frame_bounds(panel);
        assert_eq!(
            b.size.width,
            gpui::px(1000.),
            "width-limited: 3.125x of 320"
        );
        assert_eq!(b.size.height, gpui::px(750.), "3.125x of 240 keeps 4:3");
        assert_eq!(b.origin.x, gpui::px(0.));
        assert_eq!(b.origin.y, gpui::px(25.), "(800-750)/2");
    }

    /// The panel's own origin offsets the centered frame.
    #[test]
    fn scaled_frame_bounds_is_relative_to_the_panel_origin() {
        let panel = gpui::Bounds::new(
            gpui::point(gpui::px(10.), gpui::px(30.)),
            gpui::size(gpui::px(640.), gpui::px(480.)),
        );
        let b = scaled_frame_bounds(panel);
        assert_eq!(b.origin.x, gpui::px(10.), "2x fills the width exactly");
        assert_eq!(b.origin.y, gpui::px(30.));
        assert_eq!(b.size.width, gpui::px(640.));
        assert_eq!(b.size.height, gpui::px(480.));
    }

    /// A fractional centering remainder is floored to a whole pixel so
    /// the frame sits on the device grid.
    #[test]
    fn scaled_frame_bounds_floors_the_centering_offset() {
        let panel = gpui::Bounds::new(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(645.), gpui::px(485.)),
        );
        let b = scaled_frame_bounds(panel);
        // Width-limited: scale 645/320, height 483.75, remainder 1.25.
        assert_eq!(b.origin.x, gpui::px(0.));
        assert_eq!(b.origin.y, gpui::px(0.), "floor(1.25/2)");
        assert_eq!(b.size.width, gpui::px(645.));
    }

    /// A panel smaller than one framebuffer falls back to a fractional
    /// aspect-fit shrink -- downscale blur beats clipping the screen.
    #[test]
    fn scaled_frame_bounds_shrinks_to_fit_a_small_panel() {
        let panel = gpui::Bounds::new(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(160.), gpui::px(600.)),
        );
        let b = scaled_frame_bounds(panel);
        assert_eq!(b.size.width, gpui::px(160.), "width-bound shrink");
        assert_eq!(b.size.height, gpui::px(120.), "aspect preserved");
    }
    use project::{FakeFs, Project, WorktreeId};
    use workspace::dock::{DockPosition, Panel as _};
    use workspace::{AppState, MultiWorkspace};

    /// A panel in a real (workspace-less) test window. `AppState::test`
    /// first: `add_window_view` renders immediately, and `render_screen`
    /// reads `cx.theme()`, which panics without the theme global.
    fn windowed_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<EmuPanel>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        cx.add_window_view(EmuPanel::test_new)
    }

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
        ggo_common::bind_default_keymap(cx);
    }

    /// Get (creating on first call) THE center-pane emulator item's
    /// panel -- what the tests below configure `root_override` on before
    /// driving routing, mirroring how production reuses the singleton
    /// tab.
    fn emu_panel_via_item(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<EmuPanel> {
        workspace.update_in(cx, |workspace, window, cx| {
            open_emu_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<EmulatorItem>(cx)
                .next()
                .expect("open_emu_item adds the item")
                .read(cx)
                .panel()
                .clone()
        })
    }

    /// The emulator is a SINGLETON center tab: `open_emu_item` creates it
    /// once and every later call activates the same item instead of
    /// stacking a second emulator.
    #[gpui::test]
    async fn test_open_emu_item_is_a_singleton_center_tab(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let first = emu_panel_via_item(&workspace, cx);
        let second = emu_panel_via_item(&workspace, cx);
        assert_eq!(
            first.entity_id(),
            second.entity_id(),
            "one emulator, re-focused"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<EmulatorItem>(cx).count(),
                1,
                "exactly one emulator tab"
            );
        });

        // HEAVY action: opening the emulator folds every center split
        // into one pane, moving (not closing) the splits' tabs.
        workspace.update_in(cx, |workspace, window, cx| {
            let active = workspace.active_pane().clone();
            let new_pane =
                workspace.split_pane(active, workspace::SplitDirection::Right, window, cx);
            let extra = cx.new(workspace::item::test::TestItem::new);
            new_pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(extra), true, true, None, window, cx);
            });
            assert_eq!(workspace.panes().len(), 2, "split exists before the run");
        });
        emu_panel_via_item(&workspace, cx);
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 1, "the split folded away");
            assert_eq!(
                workspace.active_pane().read(cx).items_len(),
                2,
                "the split's tab moved into the surviving pane"
            );
        });
    }

    /// Run with nothing selected reports rather than panicking, and
    /// starts no thread.
    #[gpui::test]
    async fn test_run_without_a_selection_is_reported(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.run(window, cx);
            assert!(!panel.is_running());
            assert_eq!(panel.status.as_deref(), Some("no cart selected"));
        });
    }

    /// Key events drive the pad mask through the panel's own handlers,
    /// including the shift-as-SELECT path, and losing focus releases
    /// everything.
    #[gpui::test]
    async fn test_key_handling_latches_and_releases_the_pad(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, cx| {
            panel.on_key("z", true);
            panel.on_key("left", true);
            panel.on_shift(true);
            assert_eq!(panel.input.mask(), (1 << 0) | (1 << 6) | input::SELECT_BIT);

            panel.on_key("z", false);
            assert_eq!(panel.input.mask(), (1 << 6) | input::SELECT_BIT);

            // Unmapped keys never touch the mask -- the pane is focused
            // while a user might still hit ctrl-alt-r.
            panel.on_key("r", true);
            assert_eq!(panel.input.mask(), (1 << 6) | input::SELECT_BIT | (1 << 11));
            panel.on_key("escape", true);
            assert_eq!(panel.input.mask(), (1 << 6) | input::SELECT_BIT | (1 << 11));

            panel.release_all_buttons(cx);
            assert_eq!(panel.input.mask(), 0);
        });
    }

    /// A run that dies clears the session (so Stop stops being offered),
    /// surfaces its reason, and reports what happened to the ingest --
    /// here, a cart that never loaded, which has no frames to write.
    #[gpui::test]
    async fn test_an_ended_run_reports_and_clears_the_session(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (panel, cx) = windowed_panel(cx);
        let (session, rx) =
            drive::start("/definitely/not/here.cart".into(), "gone.cart".into(), None);
        drop(rx);
        panel.update(cx, |panel, cx| {
            panel.session = Some(session);
            assert!(panel.is_running());
            panel.finish_run(cx);
            assert!(!panel.is_running(), "an ended run clears the session");
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(
                panel
                    .status
                    .as_deref()
                    .is_some_and(|s| s.contains("here.cart")),
                "{:?}",
                panel.status
            );
            assert_eq!(
                panel.ingest_status,
                IngestStatus::NoFrames,
                "a cart that never loaded has nothing to ingest"
            );
        });
    }

    /// A stale run's late completion must not stomp a live run's status.
    /// `Session::wait` is joined off-thread, so run A's `finish_run` can
    /// still be mid-flight (background `wait` in progress) when the user
    /// starts run B; when A's result finally lands, `run_generation` must
    /// have already moved the pane past it. Mirrors `test_an_ended_run_
    /// reports_and_clears_the_session`'s direct-session-manipulation
    /// style, but interleaves two runs' `finish_run` calls instead of
    /// going through the real thread timing (which can't be raced
    /// deterministically from a test).
    #[gpui::test]
    async fn test_a_stale_runs_late_completion_does_not_stomp_a_live_run(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (panel, cx) = windowed_panel(cx);
        let (session_a, rx_a) = drive::start(
            "/definitely/not/here.cart".into(),
            "run-a.cart".into(),
            None,
        );
        drop(rx_a);

        panel.update(cx, |panel, cx| {
            // Run A, already under way.
            panel.run_generation = 1;
            panel.session = Some(session_a);
            // `finish_run` captures generation 1 and hands session A's
            // `wait()` to a background task -- deliberately not awaited
            // here, so it's still in flight when run B starts below,
            // exactly like a completion landing seconds late.
            panel.finish_run(cx);

            // Run B starts before A's completion has landed: a new
            // generation, a live session, and a status of its own.
            panel.run_generation = 2;
            panel.session =
                Some(drive::start("/also/not/here.cart".into(), "run-b.cart".into(), None).0);
            panel.status = Some("run B is live".to_string());
            panel.ingest_status = IngestStatus::Idle;
        });

        // Let A's background `wait()` resolve and its completion closure
        // run. Without the generation guard this overwrites `status`/
        // `ingest_status` with A's ("here.cart" / NoFrames) even though B
        // is now the live run.
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.status.as_deref(),
                Some("run B is live"),
                "run A's late completion must not overwrite run B's status"
            );
            assert_eq!(
                panel.ingest_status,
                IngestStatus::Idle,
                "nor run B's ingest status"
            );
        });
    }

    /// An ended run leaves its last frame on screen rather than blanking
    /// the pane -- the opposite of Stop.
    #[gpui::test]
    async fn test_an_ended_run_keeps_its_last_frame(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (panel, cx) = windowed_panel(cx);
        push_frame_and_draw(&panel, cx, 1);
        panel.update(cx, |panel, cx| {
            panel.finish_run(cx);
            assert!(
                panel.latest_frame.is_some(),
                "the final frame must stay on screen"
            );
        });
    }

    /// The stats row folds every delivered frame in, and hides itself
    /// before a run has produced one.
    #[gpui::test]
    async fn test_the_stats_row_tracks_frames_and_drops(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.render_stats().is_none(),
                "no stats row before the first frame"
            );
        });
        push_frame_and_draw(&panel, cx, 1);
        // Frame 4 next: two frames were dropped in between.
        push_frame_and_draw(&panel, cx, 4);
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.stats.dropped, 2);
            assert_eq!(panel.frame, 4);
            assert!(panel.render_stats().is_some());
        });
    }

    /// The console is hidden until something is logged, collapsed by
    /// default, and toggles.
    #[gpui::test]
    async fn test_the_console_appears_with_content_and_toggles(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, _window, cx| {
            assert!(
                panel.render_console(cx).is_none(),
                "no console before a run"
            );
            let log = uart::UartLog::new();
            panel.console = Some(log.clone());
            assert!(
                panel.render_console(cx).is_none(),
                "an empty log renders nothing rather than an empty box"
            );
            log.push_line("[run] green.cart");
            assert!(panel.render_console(cx).is_some());
            assert!(!panel.console_expanded, "collapsed by default");
            panel.console_expanded = true;
            assert!(panel.render_console(cx).is_some(), "expanded also renders");
        });
    }

    // ------------------------------------------------------------ audio

    /// Mute toggles, is reflected in the state the transport renders from,
    /// and -- the part that matters -- survives a run ending. The pane must
    /// never quietly un-mute itself between runs.
    #[gpui::test]
    async fn test_mute_toggles_and_survives_the_run_that_set_it(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, cx| {
            assert!(!panel.audio.is_muted(), "runs start unmuted");

            panel.toggle_mute(cx);
            assert!(panel.audio.is_muted());
            assert_eq!(
                panel.audio.state(),
                audio::AudioState::Idle,
                "muting before a run does not invent a device"
            );

            // What `run` does to the status at the top of a new run.
            panel.audio.reset_for_run();
            assert!(
                panel.audio.is_muted(),
                "a new run must not silently un-mute the pane"
            );

            panel.toggle_mute(cx);
            assert!(!panel.audio.is_muted());
        });
    }

    /// The underrun counter is surfaced, not merely counted. It rides the
    /// stats row alongside fps/drops, and it shows even before the first
    /// frame -- a device that opened is worth saying so.
    #[gpui::test]
    async fn test_the_stats_row_surfaces_the_underrun_counter(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.render_stats().is_none(),
                "nothing to say before a run opens a device or delivers a frame"
            );
            panel.audio.mark_unavailable("no default output device");
            assert!(
                panel.render_stats().is_some(),
                "an audio verdict alone is worth a row"
            );
        });

        push_frame_and_draw(&panel, cx, 1);
        panel.update(cx, |panel, _cx| {
            // What the emulator thread does once cpal hands back a stream.
            panel.audio.reset_for_run();
            panel.audio.mark_available();
            for _ in 0..3 {
                panel.audio.record_dropout();
            }
            assert_eq!(
                panel.audio.state().label(true).as_deref(),
                Some("audio on · 3 dropouts")
            );
            assert!(
                panel.render_stats().is_some(),
                "the row carries the frame counters AND the audio segment"
            );

            panel.audio.set_muted(true);
            assert_eq!(
                panel.audio.state().label(true).as_deref(),
                Some("audio muted · 3 dropouts"),
                "muting must not hide (or reset) the diagnostic"
            );

            // Finding 6: with no session live, the pane must not claim a
            // device is open -- the count belongs to the run that ended.
            assert!(!panel.is_running());
            assert_eq!(
                panel.audio.state().label(panel.is_running()).as_deref(),
                Some("audio idle · 3 dropouts last run")
            );
        });
    }

    /// A machine with no audio device: the pane reads muted-with-a-reason,
    /// the reason is the one cpal gave, the mute button has nothing to
    /// toggle, and none of it touches the run.
    #[gpui::test]
    async fn test_an_absent_audio_device_degrades_to_a_legible_reason(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, _window, cx| {
            panel.audio.mark_unavailable("no default output device");

            assert_eq!(
                panel.audio.state(),
                audio::AudioState::Unavailable("no default output device".into())
            );
            assert!(
                !panel.audio.state().is_toggleable(),
                "there is no device for the mute button to act on"
            );

            // The toggle is inert rather than misleading: clicking it must
            // not flip a mute flag that changes nothing audible.
            panel.toggle_mute(cx);
            assert!(
                !panel.audio.is_muted(),
                "toggling with no device must not pretend to have muted something"
            );
            assert!(
                matches!(panel.audio.state(), audio::AudioState::Unavailable(_)),
                "and the reason stands"
            );

            // Both surfaces render rather than panicking.
            assert!(panel.render_stats().is_some());
            let _ = panel.render_mute_button(cx);
        });
    }

    /// The whole robustness claim, at the panel's own boundary: a Run on a
    /// machine that may or may not have an output device must produce a
    /// pane state that renders either way, and must not fail the run.
    /// Nothing here asserts sound -- only that the degraded path is a
    /// state, not an error.
    #[gpui::test]
    async fn test_a_run_reports_an_audio_verdict_without_failing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();

        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.db_path_override = Some(dir.path().join("ggo_ide.db"));
            panel.refresh_root(cx);
            panel.open_rel_path("green.cart", window, cx);
        });
        cx.executor().run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        // The device is opened before the run loop's first frame, so a
        // delivered frame is proof the verdict has landed -- no sleeping
        // on a "has the thread got there yet" guess.
        await_first_frame(&panel, cx);

        panel.update_in(cx, |panel, window, cx| {
            // Either verdict is correct; what must hold is that the pane is
            // in a renderable state and the run is alive.
            match panel.audio.state() {
                audio::AudioState::Live { .. } | audio::AudioState::Unavailable(_) => {}
                audio::AudioState::Idle => {
                    panic!("a started run must reach a verdict about its audio device")
                }
            }
            assert!(panel.is_running(), "the audio verdict must not end the run");
            assert!(panel.render_stats().is_some());
            panel.stop(window, cx);
        });
        cx.executor().run_until_parked();
    }

    /// **BLOCKER 1 at the panel's own boundary.** Restarting a cart is
    /// `stop` -> `finish_run` (which backgrounds the join, so run A's
    /// teardown lands *after* run B is already going) -> `reset_for_run` ->
    /// `drive::start` with the SAME `AudioStatus`. The mechanism that made
    /// this safe -- a run-scoped priming flag -- is unit-tested in
    /// `audio::tests::ending_one_run_does_not_silence_another_started_from_
    /// the_same_status`; what this covers is that the real restart path
    /// still leaves the pane coherent: run B reaches its own verdict, and
    /// the row reads as a LIVE run rather than "idle ... last run".
    #[gpui::test]
    async fn test_restarting_a_cart_leaves_the_new_run_with_its_own_audio_state(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();

        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.db_path_override = Some(dir.path().join("ggo_ide.db"));
            panel.refresh_root(cx);
            panel.open_rel_path("green.cart", window, cx);
        });
        cx.executor().run_until_parked();

        // Run A.
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);
        let first_generation = panel.read_with(cx, |panel, _| panel.run_generation);

        // Run B, over the top of A -- the restart path, with A's join still
        // in flight.
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);
        // Let A's background completion land, which is precisely when the
        // old design clobbered B.
        cx.executor().run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            assert!(
                panel.run_generation > first_generation,
                "sanity: this really was a restart"
            );
            assert!(panel.is_running(), "run B must still be going");
            assert_ne!(
                panel.audio.state(),
                audio::AudioState::Idle,
                "run B must reach its own audio verdict, not inherit A's cleared one"
            );
            let label = panel
                .audio
                .state()
                .label(panel.is_running())
                .expect("a run that reached a verdict has something to say");
            assert!(
                !label.contains("last run"),
                "a live run must not be reported as an ended one: {label}"
            );
            panel.stop(window, cx);
        });
        cx.executor().run_until_parked();
    }

    // ------------------------------------------------- atlas retention

    /// Feed one synthetic frame and paint the panel, returning the
    /// `RenderImage` that frame produced so the test can ask the window
    /// whether its atlas tiles are still there.
    fn push_frame_and_draw(
        panel: &gpui::Entity<EmuPanel>,
        cx: &mut gpui::VisualTestContext,
        n: u32,
    ) -> Arc<RenderImage> {
        let bgra = vec![0x7Fu8; (drive::WIDTH * drive::HEIGHT * 4) as usize];
        let image = panel.update(cx, |panel, cx| {
            panel.on_frame(
                Frame {
                    bgra,
                    number: n,
                    step_ms: 1.0,
                },
                cx,
            );
            panel
                .latest_frame
                .clone()
                .expect("a frame message must produce an image")
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(320.), px(240.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        image
    }

    /// THE prerequisite this task was gated on. A fresh `RenderImage`
    /// every frame means a fresh process-global `ImageId` and therefore a
    /// fresh atlas tile every frame; gpui frees none of them on its own
    /// (see the module doc). This drives twenty frames through the real
    /// render path and asserts the double buffer actually hands the old
    /// tiles back.
    ///
    /// The assertion is *bounded residency*, not "frame N-2 exactly": how
    /// many times `render` runs per delivered frame is the harness's (and
    /// in production, the compositor's) business, not this panel's --
    /// `push_frame_and_draw` provokes two passes per frame, since
    /// `on_emu_msg`'s `cx.notify` schedules one and `cx.draw` forces
    /// another. What the panel guarantees is the invariant that survives
    /// either cadence: at most two distinct frames are ever resident, so
    /// the atlas does not grow with run length, and the frame just
    /// painted is always among them.
    #[gpui::test]
    async fn test_per_frame_images_do_not_accumulate_atlas_tiles(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);

        let mut all: Vec<Arc<RenderImage>> = Vec::new();
        for n in 1..=20 {
            let image = push_frame_and_draw(&panel, cx, n);
            assert!(
                all.iter().all(|i| i.id != image.id),
                "every frame must be a distinct RenderImage/ImageId"
            );
            assert!(
                cx.update(|window, _| window.has_image_atlas_entry(&image)),
                "frame {n}: the frame being painted must be resident"
            );
            all.push(image);

            let resident = cx.update(|window, _| {
                all.iter()
                    .filter(|i| window.has_image_atlas_entry(i))
                    .count()
            });
            assert!(
                resident <= 2,
                "frame {n}: {resident} frames resident -- atlas residency must \
                 stay bounded, or a 60 Hz run leaks ~18 MB/s of atlas forever"
            );
        }

        // And the ones that were dropped really are gone -- not merely
        // uncounted because they were never uploaded in the first place.
        let released = cx.update(|window, _| {
            all.iter()
                .filter(|i| !window.has_image_atlas_entry(i))
                .count()
        });
        assert!(
            released >= all.len() - 2,
            "only {released} of {} frames released -- the rest leaked",
            all.len()
        );
    }

    /// Stop is a teardown path with no further render to retire through
    /// the double buffer, so it must release everything the pane still
    /// holds.
    #[gpui::test]
    async fn test_stop_releases_every_atlas_tile(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        let a = push_frame_and_draw(&panel, cx, 1);
        let b = push_frame_and_draw(&panel, cx, 2);

        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));

        assert!(
            cx.update(
                |window, _| !window.has_image_atlas_entry(&a) && !window.has_image_atlas_entry(&b)
            ),
            "Stop must hand every tile back"
        );
        panel.update(cx, |panel, _cx| {
            assert!(panel.latest_frame.is_none());
            assert!(panel.current_rendered_frame.is_none());
            assert!(panel.previous_rendered_frame.is_none());
        });
    }

    /// **The panel <-> emulator-thread seam, end to end.** E1's report
    /// claimed this could not be written, on the grounds that gpui's test
    /// scheduler panics ("Detected activity on thread ...") as soon as a
    /// foreign thread wakes a task on it. That was wrong:
    /// `cx.executor().allow_parking()` sets `parking_allowed_once`, and
    /// `scheduler::test_scheduler::assert_correct_thread` returns early on
    /// that flag (`test_scheduler.rs:492-495`) -- which is exactly what
    /// every other test in this repo that talks to a real thread relies
    /// on. Parking also makes `block` wait on real time and advance the
    /// test clock by however long it parked (`test_scheduler.rs:445-450`),
    /// so a real 60 Hz emulator can drive a real panel here.
    ///
    /// So this drives the hand-assembled green cart through the WHOLE
    /// production path -- `EmuPanel::run` -> `drive::start`'s OS thread ->
    /// the bounded frame channel -> the `cx.spawn` pump -> `on_frame` ->
    /// `render` -> the window's sprite atlas -- and checks the pixels that
    /// come out the far end are the green the cart actually painted.
    #[gpui::test]
    async fn test_a_real_cart_drives_the_panel_end_to_end(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();

        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, _cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.db_path_override = Some(dir.path().join("ggo_ide.db"));
        });
        // Exactly how the file explorer gets a cart in here.
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.selected.as_deref(), Some("green.cart"));
            assert!(!panel.is_running(), "selecting a cart must not run it");
        });

        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        assert!(panel.read_with(cx, |panel, _| panel.is_running()));

        // Wait for a genuine frame off the emulator thread. The loop is
        // bounded so a broken pump fails the test instead of hanging it
        // (and the scheduler's own 15 s park ceiling backs that up).
        let mut waited = std::time::Duration::ZERO;
        let step = std::time::Duration::from_millis(10);
        while panel.read_with(cx, |panel, _| panel.latest_frame.is_none()) {
            assert!(
                waited < std::time::Duration::from_secs(10),
                "no frame reached the panel from the emulator thread"
            );
            cx.executor().timer(step).await;
            cx.executor().run_until_parked();
            waited += step;
        }

        let image = panel.read_with(cx, |panel, _| panel.latest_frame.clone().unwrap());
        let bytes = image.as_bytes(0).expect("a single-frame RenderImage");
        assert_eq!(
            bytes.len(),
            (drive::WIDTH * drive::HEIGHT * 4) as usize,
            "one BGRA8 pixel per screen pixel"
        );
        assert!(
            bytes
                .chunks_exact(4)
                .all(|px| px == [0x00, 0xFF, 0x00, 0xFF]),
            "every pixel must be the RGB565 0x07E0 backdrop the cart set, \
             expanded to full-range BGRA -- this is the cart's own output, \
             through the real PPU compose and the real conversion"
        );

        // ...and it really reaches the window's atlas when painted.
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(320.), px(240.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.update(|window, _| window.has_image_atlas_entry(&image)),
            "the painted frame must be resident in the sprite atlas"
        );

        // Tear down through the real Stop path, and let the (overridden)
        // ingest land so no thread outlives the test.
        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(!panel.is_running());
            assert_eq!(panel.status.as_deref(), Some("stopped"));
            assert!(
                matches!(panel.ingest_status, IngestStatus::Done(..)),
                "a run with frames must have ingested: {:?}",
                panel.ingest_status
            );
        });

        // The run really is in the database the charts panel reads --
        // through that panel's own query function.
        let db_path = dir.path().join("ggo_ide.db");
        let runs = ggo_charts_panel::loader::list_runs(&db_path).unwrap();
        assert_eq!(runs.len(), 1, "one run row for one run");
        assert_eq!(runs[0].cart_name, drive::fixture::GREEN_CART_TITLE);
        assert_eq!(runs[0].label.as_deref(), Some("green.cart"));
        let samples = ggo_charts_panel::loader::load_run_samples(&db_path, runs[0].id).unwrap();
        assert!(
            !samples.frames.is_empty(),
            "the run's perf frames must be readable by the charts panel"
        );
    }

    /// Stop with nothing running is a no-op that doesn't blank anything
    /// or notify pointlessly.
    #[gpui::test]
    async fn test_stop_without_a_run_is_a_noop(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.stop(window, cx);
            assert!(!panel.is_running());
            assert!(panel.latest_frame.is_none());
        });
    }

    // ------------------------------------------ explorer-driven routing

    /// A fake-fs project with one visible worktree holding cart-shaped
    /// names. The panel never reads a cart's bytes at selection time (only
    /// `run` opens the file), so a fake fs is enough for every routing
    /// assertion here.
    async fn routed_project(cx: &mut TestAppContext, run_init: bool) -> Entity<Project> {
        cx.update(|cx| {
            AppState::test(cx);
            if run_init {
                init(cx);
                ggo_common::bind_default_keymap(cx);
            }
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({
                "carts": { "green.cart": "", "other.cart": "" },
                "notes.txt": "",
            }),
        )
        .await;
        Project::test(fs, ["/proj".as_ref()], cx).await
    }

    fn worktree_id(project: &Entity<Project>, cx: &mut gpui::VisualTestContext) -> WorktreeId {
        project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("one visible worktree")
                .read(cx)
                .id()
        })
    }

    fn project_path(worktree_id: WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    /// The registered `.cart` predicate claims the path (so the project
    /// panel opens NO pane item for it), opens the dock, and selects the
    /// cart -- *without* running it. A non-`.cart` in the same worktree is
    /// declined.
    #[gpui::test]
    async fn test_cart_click_routes_into_the_panel_and_is_claimed(cx: &mut TestAppContext) {
        let project = routed_project(cx, true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        let panel = emu_panel_via_item(&workspace, cx);

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "carts/green.cart"),
                window,
                cx,
            )
        });
        assert!(
            claimed,
            "a .cart must be claimed, suppressing the pane item"
        );
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.selected.as_deref(), Some("carts/green.cart"));
            assert!(
                !panel.is_running(),
                "a click selects a cart; running is the user's call"
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<EmulatorItem>(cx).count(),
                1,
                "routing must open the emulator tab"
            );
        });

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "notes.txt"), window, cx)
        });
        assert!(!claimed, "everything but .cart opens the normal way");
    }

    /// Re-clicking the cart that is ALREADY selected must not disturb a run
    /// in progress -- the interceptor has already done the focus/reveal, so
    /// there is nothing left to do, and stopping/restarting the emulator
    /// under the user would be actively destructive.
    #[gpui::test]
    async fn test_re_clicking_the_selected_cart_does_not_disturb_a_run(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();

        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, _cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.db_path_override = Some(dir.path().join("ggo_ide.db"));
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);
        let generation = panel.read_with(cx, |panel, _| panel.run_generation);

        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert!(
                panel.is_running(),
                "an already-selected click must leave the run alone"
            );
            assert_eq!(
                panel.run_generation, generation,
                "and must not have restarted it"
            );
            assert!(
                panel.latest_frame.is_some(),
                "nor blanked the screen out from under it"
            );
        });

        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.run_until_parked();
    }

    /// Clicking a DIFFERENT cart mid-run stops the running one *through the
    /// ordinary stop path*, so the run's perf snapshot still reaches the
    /// database the charts panel reads. Then it selects the new cart and
    /// waits for the user to press Run.
    #[gpui::test]
    async fn test_switching_carts_mid_run_stops_and_still_ingests(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();
        let db_path = dir.path().join("ggo_ide.db");

        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, _cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.db_path_override = Some(db_path.clone());
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("other.cart", window, cx)
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.selected.as_deref(), Some("other.cart"));
            assert!(!panel.is_running(), "the previous run is stopped");
            assert!(
                matches!(panel.ingest_status, IngestStatus::Done(..)),
                "the stopped run's perf data must still have been ingested, \
                 not silently dropped: {:?}",
                panel.ingest_status
            );
            assert_eq!(panel.frame, 0, "the old run's counters are cleared");
        });

        // ...and it really is in the database, readable by the charts panel.
        let runs = ggo_charts_panel::loader::list_runs(&db_path).unwrap();
        assert_eq!(runs.len(), 1, "one run row for the interrupted run");
        assert_eq!(runs[0].label.as_deref(), Some("green.cart"));
    }

    /// Drive the panel until the emulator thread delivers a frame. Bounded,
    /// so a broken pump fails the test instead of hanging it.
    fn await_first_frame(panel: &Entity<EmuPanel>, cx: &mut gpui::VisualTestContext) {
        // One task per turn (`tick`), NOT `run_until_parked`: while the
        // cart runs, the emulator thread lands a frame every ~16ms, and on
        // a machine where a debug-build redraw takes longer than that the
        // executor never goes idle -- `run_until_parked` livelocks, and a
        // sleep-counting deadline never fires because the pump never
        // returns. Ticking re-checks the exit condition between tasks, and
        // the deadline is wall-clock so it fires no matter where the time
        // actually went.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while panel.read_with(cx, |panel, _| panel.latest_frame.is_none()) {
            assert!(
                std::time::Instant::now() < deadline,
                "no frame reached the panel from the emulator thread"
            );
            if !cx.background_executor.tick() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    /// **The `project_panel.rs` hook itself, end to end.** Every other
    /// routing test enters at `Workspace::intercept_path_open`, which means
    /// none of them would notice if the `if !workspace.intercept_path_open(
    /// &ggo_project_path, …)` guard in `crates/project_panel/src/
    /// project_panel.rs` were lost in an upstream merge -- and that guard is
    /// the fork's single most merge-fragile line (see `docs/ggo/UPSTREAM.md`:
    /// `project_panel.rs` churns considerably more than our other hook
    /// sites).
    ///
    /// So this one builds a REAL `ProjectPanel` (via its public
    /// `ProjectPanel::load`, the same constructor `zed::zed` uses), which
    /// installs the real `Event::OpenedEntry` subscription containing the
    /// guard, then emits that event for a `.cart` entry and checks BOTH
    /// halves of what the guard buys: the cart lands in this panel, and no
    /// pane item is opened for it. `editor::init` is load-bearing for the
    /// second half -- without an editor registered as a project item,
    /// `open_path_preview` would fail to produce a tab even with the guard
    /// deleted, and the test would pass for the wrong reason. The control
    /// case at the end proves the tab really does appear when the path is
    /// NOT claimed.
    ///
    /// The one step not covered is `ProjectPanel::open_internal` ->
    /// `open_entry` -> `cx.emit`, which is private to `project_panel` and
    /// entirely upstream-owned; the fork's edit is in the subscriber, which
    /// is what this exercises.
    #[gpui::test]
    async fn test_project_panel_opened_entry_routes_a_cart_into_the_panel(cx: &mut TestAppContext) {
        let project = routed_project(cx, true).await;
        cx.update(|cx| {
            editor::init(cx);
            project_panel::init(cx);
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        // The real project panel, with the real `OpenedEntry` subscription.
        let (weak_workspace, async_cx) =
            cx.update(|window, cx| (workspace.downgrade(), window.to_async(cx)));
        let project_panel = project_panel::ProjectPanel::load(weak_workspace, async_cx)
            .await
            .expect("the project panel loads");

        let panel = emu_panel_via_item(&workspace, cx);
        let worktree_id = worktree_id(&project, cx);

        fn entry_id(
            project: &Entity<Project>,
            worktree_id: WorktreeId,
            rel: &str,
            cx: &mut gpui::VisualTestContext,
        ) -> project::ProjectEntryId {
            project.read_with(cx, |project, cx| {
                project
                    .entry_for_path(&project_path(worktree_id, rel), cx)
                    .unwrap_or_else(|| panic!("{rel} is in the fake worktree"))
                    .id
            })
        }

        // The click, as the project panel publishes it.
        let cart_entry = entry_id(&project, worktree_id, "carts/green.cart", cx);
        project_panel.update(cx, |_, cx| {
            cx.emit(project_panel::Event::OpenedEntry {
                entry_id: cart_entry,
                focus_opened_item: true,
                allow_preview: true,
            });
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.selected.as_deref(),
                Some("carts/green.cart"),
                "the project panel's own open event must reach the emu panel \
                 -- if this fails, the GGO guard in project_panel.rs is gone"
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).items().count(),
                1,
                "a claimed path opens the emulator tab and nothing else"
            );
            assert_eq!(workspace.items_of_type::<EmulatorItem>(cx).count(), 1);
        });

        // Control: an unclaimed path DOES open a tab, so the assertion
        // above is testing the guard rather than a dead code path.
        let txt_entry = entry_id(&project, worktree_id, "notes.txt", cx);
        project_panel.update(cx, |_, cx| {
            cx.emit(project_panel::Event::OpenedEntry {
                entry_id: txt_entry,
                focus_opened_item: true,
                allow_preview: true,
            });
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).items().count(),
                2,
                "an unclaimed path still opens the normal way, beside the emulator tab"
            );
        });
    }

    /// The `Event::SplitEntry` sibling of the test above. Same real
    /// `ProjectPanel`, same real subscription, but this time the click is a
    /// split-open (e.g. alt-click) rather than a plain open. It exercises
    /// the second guard at `crates/project_panel/src/project_panel.rs`'s
    /// `Event::SplitEntry` arm, which is otherwise untested: nothing else
    /// would notice if that `if !workspace.intercept_path_open(&ggo_project_path,
    /// …)` guard were dropped from a merge.
    #[gpui::test]
    async fn test_project_panel_split_entry_routes_a_cart_into_the_panel(cx: &mut TestAppContext) {
        let project = routed_project(cx, true).await;
        cx.update(|cx| {
            editor::init(cx);
            project_panel::init(cx);
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let (weak_workspace, async_cx) =
            cx.update(|window, cx| (workspace.downgrade(), window.to_async(cx)));
        let project_panel = project_panel::ProjectPanel::load(weak_workspace, async_cx)
            .await
            .expect("the project panel loads");

        let panel = emu_panel_via_item(&workspace, cx);
        let worktree_id = worktree_id(&project, cx);

        fn entry_id(
            project: &Entity<Project>,
            worktree_id: WorktreeId,
            rel: &str,
            cx: &mut gpui::VisualTestContext,
        ) -> project::ProjectEntryId {
            project.read_with(cx, |project, cx| {
                project
                    .entry_for_path(&project_path(worktree_id, rel), cx)
                    .unwrap_or_else(|| panic!("{rel} is in the fake worktree"))
                    .id
            })
        }

        // The alt-click / split-open, as the project panel publishes it.
        let cart_entry = entry_id(&project, worktree_id, "carts/green.cart", cx);
        project_panel.update(cx, |_, cx| {
            cx.emit(project_panel::Event::SplitEntry {
                entry_id: cart_entry,
                allow_preview: true,
                split_direction: Some(workspace::SplitDirection::Right),
            });
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.selected.as_deref(),
                Some("carts/green.cart"),
                "the project panel's split-open event must reach the emu panel \
                 -- if this fails, the GGO guard in project_panel.rs's SplitEntry \
                 arm is gone"
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).items().count(),
                1,
                "a claimed path opens the emulator tab and nothing else, split or not"
            );
            assert_eq!(workspace.items_of_type::<EmulatorItem>(cx).count(), 1);
        });

        // Control: an unclaimed path DOES split-open a tab, so the assertion
        // above is testing the guard rather than a dead code path.
        let txt_entry = entry_id(&project, worktree_id, "notes.txt", cx);
        project_panel.update(cx, |_, cx| {
            cx.emit(project_panel::Event::SplitEntry {
                entry_id: txt_entry,
                allow_preview: true,
                split_direction: Some(workspace::SplitDirection::Right),
            });
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).items().count(),
                1,
                "an unclaimed path still splits open the normal way"
            );
        });
    }

    // ------------------------------ project-panel context-menu contributors

    /// The label the test contributor appends. Deliberately not a real GGO
    /// entry: these tests guard the HOOK, not any particular menu item.
    const GGO_ENTRY: &str = "Re-run (perf)";
    const GGO_ITEM: &str = "MENU_ITEM-Re-run (perf)";
    /// A second contributor's label, for the registration-order test.
    const SECOND_ENTRY: &str = "Emulate this world";
    const SECOND_ITEM: &str = "MENU_ITEM-Emulate this world";
    /// Upstream entries every writable project-panel menu carries. Their
    /// presence proves a menu really deployed, and that the fork APPENDED to
    /// upstream's menu rather than replacing it.
    const UPSTREAM_ITEMS: [&str; 5] = [
        "MENU_ITEM-New File",
        "MENU_ITEM-New Folder",
        "MENU_ITEM-Copy Path",
        "MENU_ITEM-Copy Relative Path",
        "MENU_ITEM-Rename",
    ];

    /// Every `(path, is_dir)` the registered contributor was offered, in
    /// order. A gpui `Global` rather than a `thread_local!`: `#[gpui::test]`
    /// bodies share a thread, and a global dies with its `App`.
    #[derive(Default)]
    struct Offered(Vec<(String, bool)>);

    impl gpui::Global for Offered {}

    /// A `ContextMenuContributor` shaped like the real ones: it records what
    /// it was offered, and contributes one entry for `.cart` paths only.
    fn cart_contributor(
        _workspace: &mut Workspace,
        path: &ProjectPath,
        is_dir: bool,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Vec<ui::ContextMenuItem> {
        let rel = path.path.as_unix_str().to_string();
        cx.default_global::<Offered>().0.push((rel.clone(), is_dir));
        if rel.ends_with(".cart") {
            vec![ui::ContextMenuEntry::new(GGO_ENTRY).into()]
        } else {
            Vec::new()
        }
    }

    /// A second contributor, registered after `cart_contributor`, claiming
    /// the same paths so both land in one menu.
    fn world_contributor(
        _workspace: &mut Workspace,
        path: &ProjectPath,
        _is_dir: bool,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> Vec<ui::ContextMenuItem> {
        if path.path.as_unix_str().ends_with(".cart") {
            vec![ui::ContextMenuEntry::new(SECOND_ENTRY).into()]
        } else {
            Vec::new()
        }
    }

    /// A project whose `is_local()` is false, built on a mock remote
    /// connection. No `HeadlessProject` is set up on the server side and no
    /// worktree is opened: the non-local gate declines before anything is
    /// asked of the connection.
    async fn remote_project(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<()>) {
        let app_state = cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            AppState::test(cx)
        });
        let (opts, server_client, connect_guard) = remote::RemoteClient::fake_server(cx, server_cx);
        // The client handshakes with a `Ping` before it reports connected;
        // answering it is the whole server this test needs. (The returned
        // entity is the handler's owner -- dropping it unregisters it.)
        let ping_handler = server_cx.new(|_| ());
        server_client.add_request_handler::<rpc::proto::Ping, (), _, _>(
            ping_handler.downgrade(),
            |_, _, _| async { Ok(rpc::proto::Ack {}) },
        );
        drop(connect_guard);
        let remote_client = remote::RemoteClient::connect_mock(opts, cx).await;
        let project = cx.update(|cx| {
            Project::remote(
                remote_client,
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                app_state.user_store.clone(),
                app_state.languages.clone(),
                app_state.fs.clone(),
                false,
                cx,
            )
        });
        // `Workspace::test_new` builds its own `WorkspaceStore` on the same
        // client, and the client refuses two handlers for one message: let
        // this one's go first (every other test here drops its `AppState`
        // immediately, which is why none of them trip over this).
        drop(app_state);
        cx.run_until_parked();
        (project, ping_handler)
    }

    /// A project + a REAL `ProjectPanel`, docked and rendered, so a
    /// right-click on a row runs upstream's own `deploy_context_menu`.
    /// Returns the workspace and the window x to right-click at -- the
    /// project panel docks wherever upstream's settings default puts it
    /// (currently the right), and a hardcoded column would silently stop
    /// hitting any row the day that changes.
    async fn context_menu_panel(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Pixels, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            project_panel::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({
                "worlds": {},
                "hero.cart": "",
                "notes.txt": "",
            }),
        )
        .await;
        let project = Project::test(fs, ["/proj".as_ref()], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let (weak_workspace, async_cx) =
            cx.update(|window, cx| (workspace.downgrade(), window.to_async(cx)));
        let project_panel = project_panel::ProjectPanel::load(weak_workspace, async_cx)
            .await
            .expect("the project panel loads");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(project_panel, window, cx);
            workspace.open_panel::<project_panel::ProjectPanel>(window, cx);
        });
        cx.run_until_parked();

        let viewport = cx.update(|window, _| window.viewport_size());
        let column_x = workspace.update_in(cx, |workspace, window, cx| {
            let position = workspace
                .panel::<project_panel::ProjectPanel>(cx)
                .expect("the project panel is docked")
                .read(cx)
                .position(window, cx);
            match position {
                DockPosition::Right => viewport.width - px(40.),
                _ => px(40.),
            }
        });

        (workspace, column_x, cx)
    }

    /// Right-click at `position` in the rendered window, dismissing whatever
    /// menu the previous probe left open first (an open menu is drawn OVER
    /// the rows, and would eat the next click).
    fn right_click(cx: &mut gpui::VisualTestContext, position: gpui::Point<Pixels>) {
        cx.simulate_keystrokes("escape");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
    }

    /// Walk the project panel's rows top-down until the context menu deploys
    /// for the entry named `rel`, and leave that menu open. Scanning rather
    /// than hardcoding a row height is on purpose: `project_panel.rs` is the
    /// fork's highest-churn hook site, and a test that dies of a two-pixel
    /// layout change is a test nobody re-applies the hook for.
    fn right_click_row(cx: &mut gpui::VisualTestContext, column_x: Pixels, rel: &str) {
        let mut y = px(0.);
        while y < px(400.) {
            right_click(cx, gpui::point(column_x, y));
            let hit = cx.update(|_, cx| cx.default_global::<Offered>().0.last().cloned());
            if hit.as_ref().is_some_and(|(path, _)| path == rel) {
                return;
            }
            y += px(4.);
        }
        let seen = cx.update(|_, cx| cx.default_global::<Offered>().0.clone());
        panic!(
            "no row for {rel:?} was right-clickable, or the GGO block in \
             project_panel.rs's deploy_context_menu is gone (nothing was \
             offered to the contributor at all); the scan saw {seen:?}"
        );
    }

    /// **The `deploy_context_menu` hook itself, end to end.** Everything else
    /// here enters at `Workspace::context_menu_contributions`, so nothing but
    /// this would notice the GGO block in
    /// `crates/project_panel/src/project_panel.rs`'s `deploy_context_menu`
    /// disappearing in an upstream merge (see `docs/ggo/UPSTREAM.md`).
    ///
    /// It builds a REAL `ProjectPanel` via its public `ProjectPanel::load`,
    /// docks it so it renders, and right-clicks a real row -- upstream's own
    /// `on_secondary_mouse_down` listener, upstream's own menu. The
    /// contributed entry must show up in the RENDERED menu (`MENU_ITEM-…`
    /// debug bounds), next to upstream's own entries.
    #[gpui::test]
    async fn test_project_panel_context_menu_shows_a_contributed_entry(cx: &mut TestAppContext) {
        let (_workspace, column_x, cx) = context_menu_panel(cx).await;
        cx.update(|_, cx| workspace::register_context_menu_contributor(cx, cart_contributor));

        right_click_row(cx, column_x, "hero.cart");

        let ggo_y = cx
            .debug_bounds(GGO_ITEM)
            .expect(
                "the contributed entry must be in the deployed menu -- if this \
                 fails, the GGO block in project_panel.rs's deploy_context_menu \
                 is gone",
            )
            .origin
            .y;
        // Present AND last: "the fork's entries go last" is load-bearing (see
        // docs/ggo/UPSTREAM.md), and an entry spliced in ahead of upstream's
        // would still be "present".
        for item in UPSTREAM_ITEMS {
            let upstream_y = cx
                .debug_bounds(item)
                .unwrap_or_else(|| {
                    panic!("{item} must still be there: the fork APPENDS to upstream's menu")
                })
                .origin
                .y;
            assert!(
                ggo_y > upstream_y,
                "the contributed entry must render BELOW {item}"
            );
        }
        assert_eq!(
            cx.update(|_, cx| cx.default_global::<Offered>().0.last().cloned()),
            Some(("hero.cart".to_string(), false)),
            "the contributor is offered the clicked path, and told it is a file"
        );

        // A path the contributor declines gets upstream's menu, unchanged.
        right_click_row(cx, column_x, "notes.txt");
        assert!(
            cx.debug_bounds(GGO_ITEM).is_none(),
            "a declined path must not carry the entry"
        );
        for item in UPSTREAM_ITEMS {
            assert!(cx.debug_bounds(item).is_some(), "{item} is still there");
        }

        // Directories are offered too, flagged as such.
        right_click_row(cx, column_x, "worlds");
        assert_eq!(
            cx.update(|_, cx| cx.default_global::<Offered>().0.last().cloned()),
            Some(("worlds".to_string(), true)),
            "a directory is offered with is_dir = true"
        );
    }

    /// Two contributors' entries are concatenated in REGISTRATION order --
    /// the order GGO crates' `init`s run in, which is the only order an
    /// author can reason about when placing their entries.
    #[gpui::test]
    async fn test_project_panel_context_menu_appends_in_registration_order(
        cx: &mut TestAppContext,
    ) {
        let (_workspace, column_x, cx) = context_menu_panel(cx).await;
        cx.update(|_, cx| {
            workspace::register_context_menu_contributor(cx, cart_contributor);
            workspace::register_context_menu_contributor(cx, world_contributor);
        });

        right_click_row(cx, column_x, "hero.cart");

        let first = cx
            .debug_bounds(GGO_ITEM)
            .expect("the first contributor's entry")
            .origin
            .y;
        let second = cx
            .debug_bounds(SECOND_ITEM)
            .expect("the second contributor's entry")
            .origin
            .y;
        assert!(
            first < second,
            "entries follow the order their contributors were registered in"
        );
        for item in UPSTREAM_ITEMS {
            let upstream_y = cx.debug_bounds(item).expect("upstream entry").origin.y;
            assert!(
                first > upstream_y,
                "both contributed entries still come after {item}"
            );
        }
    }

    /// The empty-registry case: upstream's menu, untouched. The separator
    /// the hook emits ahead of the fork's block is conditional on there
    /// being a block, so an unregistered fork adds nothing at all -- not
    /// even a divider.
    #[gpui::test]
    async fn test_project_panel_context_menu_is_untouched_without_contributors(
        cx: &mut TestAppContext,
    ) {
        let (workspace, column_x, cx) = context_menu_panel(cx).await;

        // No contributor registered: nothing to append, so nothing to
        // separate. The call site's `is_empty()` branch returns the menu
        // upstream built, by identity.
        let contributions = workspace.update_in(cx, |workspace, window, cx| {
            let worktree_id = workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .expect("one visible worktree")
                .read(cx)
                .id();
            workspace.context_menu_contributions(
                &project_path(worktree_id, "hero.cart"),
                false,
                window,
                cx,
            )
        });
        assert!(
            contributions.is_empty(),
            "an empty registry contributes nothing"
        );

        // And the deployed menu is upstream's, with no GGO entry in it.
        let mut y = px(0.);
        while y < px(400.) && cx.debug_bounds(UPSTREAM_ITEMS[0]).is_none() {
            right_click(cx, gpui::point(column_x, y));
            y += px(4.);
        }
        for item in UPSTREAM_ITEMS {
            assert!(
                cx.debug_bounds(item).is_some(),
                "{item} must be in the untouched menu"
            );
        }
        assert!(
            cx.debug_bounds(GGO_ITEM).is_none(),
            "nothing was registered, so nothing was contributed"
        );
    }

    /// A non-local project (SSH remote, collab guest) contributes nothing:
    /// GGO panels read their documents with `std::fs` against the worktree's
    /// `abs_path`, which names a directory that does not exist on this
    /// machine. Same rule `ggo_common::rel_in_primary_worktree` applies to
    /// the F4 open interceptors.
    #[gpui::test]
    async fn test_context_menu_contributions_decline_a_non_local_project(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let (project, _ping_handler) = remote_project(cx, server_cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.update(|_, cx| workspace::register_context_menu_contributor(cx, cart_contributor));
        cx.run_until_parked();

        project.read_with(cx, |project, _| {
            assert!(!project.is_local(), "the fixture really is non-local")
        });

        // The gate declines before it ever looks at a worktree, so any id
        // does (the fixture has none: connecting a mock server is enough to
        // make the project non-local).
        let contributions = workspace.update_in(cx, |workspace, window, cx| {
            workspace.context_menu_contributions(
                &project_path(WorktreeId::from_usize(1), "hero.cart"),
                false,
                window,
                cx,
            )
        });
        assert!(
            contributions.is_empty(),
            "a non-local project contributes nothing"
        );
        assert!(
            cx.update(|_, cx| cx.default_global::<Offered>().0.is_empty()),
            "the contributor is not even consulted"
        );
    }
    // ------------------------------------------------- S4: the run actions

    /// A workspace with all three panels S4 wires together (this one, the
    /// world panel it saves through, the charts panel it hands a finished
    /// run to), a FakeFs worktree for the `ProjectPath`s the menu takes,
    /// and this panel pointed at the REAL `root` -- the same split every
    /// other GGO menu test makes: the fake tree exists so a `ProjectPath`
    /// resolves, `root_override` is what the panel actually reads and
    /// writes through.
    pub(crate) async fn run_menu_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        Entity<Workspace>,
        Entity<EmuPanel>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
            ggo_world_panel::init(cx);
            ggo_charts_panel::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({
                "emerald.toml": "",
                "assets": { "worlds": { "main.toml": "" } },
                "green.cart": "",
                "notes.txt": "",
            }),
        )
        .await;
        let project = Project::test(fs, ["/proj".as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        let panel = emu_panel_via_item(&workspace, cx);
        let root = root.to_path_buf();
        panel.update(cx, |panel, _cx| {
            panel.root_override = Some(root.clone());
            // Never the developer's real `~/.ggo/ggo_ide.db`: every path
            // below can reach the end-of-run ingest.
            panel.db_path_override = Some(root.join("ggo_ide.db"));
        });
        (workspace, panel, worktree_id, cx)
    }

    /// A `ProcRunner` that spawns nothing, plus the log of every request it
    /// was handed -- the emu-panel twin of `ggo_emerald_panel`'s
    /// `fake_runner`, and what lets both the `emd` and the `ggo-diag` paths
    /// be asserted without either binary (or a board) present.
    #[allow(clippy::type_complexity)]
    fn fake_proc_runner(
        reply: impl Fn(&ggo_common::ProcRequest) -> ggo_common::ProcCapture + Send + Sync + 'static,
    ) -> (
        ggo_common::ProcRunner,
        Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>>,
    ) {
        let calls: Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let runner: ggo_common::ProcRunner = Arc::new(move |request| {
            let capture = reply(&request);
            recorded.lock().unwrap().push(request);
            capture
        });
        (runner, calls)
    }

    fn ok_capture() -> ggo_common::ProcCapture {
        ggo_common::ProcCapture {
            ok: true,
            lines: vec!["packed".to_string()],
        }
    }

    /// Each entry is offered on exactly its own paths, and nowhere else.
    /// "Run hardware diagnostics" is on everything (it needs no project),
    /// so the counts are 1 + whichever file-specific entry applies.
    /// `ContextMenuEntry::label` is private, so which entry it is has to be
    /// proven by firing the handlers -- which the tests below do.
    #[gpui::test]
    async fn test_the_run_menu_offers_each_entry_only_on_its_own_paths(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };

        assert_eq!(
            contributed("green.cart", false, cx),
            1,
            "a cart gets Re-run"
        );
        assert_eq!(
            contributed("assets/worlds/main.toml", false, cx),
            2,
            "a world gets Emulate -- and `ggo_world_panel`'s own Delete World, \
             which this harness registers too, since contributions from every \
             panel land in one menu"
        );
        assert_eq!(
            contributed("emerald.toml", false, cx),
            0,
            "a .toml outside worlds/ is neither a world nor a cart"
        );
        assert_eq!(
            contributed("notes.txt", false, cx),
            0,
            "an unrelated FILE gets nothing -- upstream's own menu is left alone"
        );
        assert_eq!(
            contributed("assets/worlds", true, cx),
            1,
            "diagnostics hangs off directories: it needs no project, but a \
             permanent extra line on every file's menu is not what \"anywhere\" \
             is worth"
        );
    }

    /// **"Emulate this world", end to end through the entry's own
    /// handler.** The build must run `emd pack-ggo` with THIS world as the
    /// boot world, in the emerald project root, and hand the artifact to
    /// the panel's ordinary run path -- with the dock brought forward, so
    /// the user can see what they started.
    #[gpui::test]
    async fn test_emulate_this_world_packs_that_boot_world_and_selects_the_cart(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\n").unwrap();
        std::fs::write(dir.path().join("assets/worlds/main.toml"), "").unwrap();

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ok_capture());
        panel.update(cx, |panel, _cx| panel.proc_runner = runner);

        let handler = menu::emulate_world_handler(
            workspace.downgrade(),
            "assets/worlds/main.toml".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one build");
        assert_eq!(
            calls[0].cwd,
            dir.path(),
            "emd discovers the project from its cwd, so it must be the project root"
        );
        assert_eq!(
            calls[0].args,
            [
                "pack-ggo",
                "--out",
                dir.path()
                    .join("target/ggo-emulate/worlds-main.ggo")
                    .to_str()
                    .unwrap(),
                "--world",
                "worlds/main",
                "--json",
            ],
            "the clicked world must be baked in as the boot world"
        );

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.selected.as_deref(),
                Some("target/ggo-emulate/worlds-main.ggo"),
                "the built cartridge becomes the selection, through the SAME \
                 path a clicked cart takes"
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<EmulatorItem>(cx).count(),
                1,
                "the emulator tab must come forward"
            );
        });
    }

    /// The world panel's Emulate button arrives through `ggo_common`'s
    /// emulator registry: `init` must register a handler that lands in the
    /// SAME save-first build path the explorer entry uses.
    #[gpui::test]
    async fn test_registered_world_emulator_routes_to_the_same_build(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\n").unwrap();
        std::fs::write(dir.path().join("assets/worlds/main.toml"), "").unwrap();

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ok_capture());
        panel.update(cx, |panel, _cx| panel.proc_runner = runner);

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::emulate_world(workspace, "assets/worlds/main.toml", window, cx)
        });
        assert!(claimed, "init registers a world emulator");
        cx.run_until_parked();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one build");
        assert!(
            calls[0].args.iter().any(|a| a == "worlds/main"),
            "the viewed world must be baked in as the boot world"
        );
        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.selected.as_deref(),
                Some("target/ggo-emulate/worlds-main.ggo"),
                "the built cartridge becomes the selection"
            );
            assert_eq!(
                panel.flash_world(cx).as_deref(),
                Some("worlds/main"),
                "a flash pressed after an emulate puts THAT world on the board"
            );
        });
    }

    /// A build that fails leaves the failure on the status row and starts
    /// nothing -- the entry must never look like it worked.
    #[gpui::test]
    async fn test_a_failed_world_build_reports_and_runs_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\n").unwrap();

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, _calls) = fake_proc_runner(|_| ggo_common::ProcCapture {
            ok: false,
            lines: vec!["error: no world named worlds/main".to_string()],
        });
        panel.update(cx, |panel, _cx| panel.proc_runner = runner);

        let handler = menu::emulate_world_handler(
            workspace.downgrade(),
            "assets/worlds/main.toml".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.status.as_deref(),
                Some("build failed: error: no world named worlds/main")
            );
            assert!(panel.selected.is_none(), "nothing was selected");
            assert!(!panel.is_running());
        });
    }

    /// A world outside any emerald project can't be packed, and says so
    /// rather than spawning `emd` into a directory it will reject.
    #[gpui::test]
    async fn test_emulating_a_world_outside_a_project_reports_the_missing_manifest(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ok_capture());
        panel.update(cx, |panel, _cx| panel.proc_runner = runner);

        let handler = menu::emulate_world_handler(
            workspace.downgrade(),
            "assets/worlds/main.toml".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        assert!(calls.lock().unwrap().is_empty(), "nothing was spawned");
        panel.update(cx, |panel, cx| {
            let status = panel.status.clone().expect("a status was reported");
            assert!(status.contains("emerald.toml"), "{status}");
            assert_eq!(
                panel.flash_world(cx),
                None,
                "a world that could not even be built is not one to aim a \
                 flash at"
            );
        });
    }

    /// **"Re-run (perf)", end to end.** The entry routes the cart into
    /// this panel, runs it for real, and -- once the run's perf ingest has
    /// landed -- hands focus to the CHARTS panel, which is the whole point
    /// of the entry over a plain Run.
    #[gpui::test]
    async fn test_rerun_runs_the_cart_then_focuses_the_charts_panel(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let charts = workspace.update_in(cx, |workspace, window, cx| {
            ggo_charts_panel::open_charts_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<ggo_charts_panel::ChartsItem>(cx)
                .next()
                .expect("open_charts_item adds the reports tab")
                .read(cx)
                .panel()
                .clone()
        });
        // Both panels on ONE temp database, so the run this test ingests is
        // the run the charts panel then reads back.
        let db_path = dir.path().join("ggo_ide.db");
        charts.update(cx, |charts, _cx| {
            charts.set_db_path_override(db_path.clone());
        });

        let handler = menu::rerun_handler(workspace.downgrade(), "green.cart".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.selected.as_deref(), Some("green.cart"));
            assert!(panel.is_running(), "Re-run runs it, it doesn't just select");
        });
        await_first_frame(&panel, cx);

        // End the run the ordinary way; that is what produces the perf row.
        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(panel.ingest_status, IngestStatus::Done(..)),
                "the run must have ingested: {:?}",
                panel.ingest_status
            );
            assert!(
                panel.charts_for_run.is_none(),
                "the arming is spent once the hop has been taken"
            );
        });
        assert!(
            cx.update(|window, cx| charts.read(cx).focus_handle(cx).is_focused(window)),
            "the finished ingest must hand focus to the charts panel"
        );
    }

    /// **The charts panel's Re-run entry lands here** (F5.4 R3). That panel
    /// cannot call this crate -- this crate depends on IT, for the hop the
    /// test above asserts -- so the handoff arrives through
    /// `ggo_common::run_cart`'s registry, which `init` populates. What is
    /// asserted here is this PANEL's state after the hook fires: the cart
    /// selected, the run live, and the return hop armed, exactly as if the
    /// explorer's own "Re-run (perf)" entry had been used. (The charts side
    /// asserts the other half -- that the run's `label` is what reaches the
    /// registry -- in `test_rerun_hands_the_runs_cart_path_to_the_cart_runner`.)
    #[gpui::test]
    async fn test_the_registered_cart_runner_runs_the_cart_in_this_pane(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            assert!(panel.selected.is_none(), "nothing is selected yet");
        });

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::run_cart(workspace, "green.cart", window, cx)
        });
        assert!(claimed, "init() must have registered a cart runner");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.selected.as_deref(), Some("green.cart"));
            assert!(
                panel.is_running(),
                "the hook goes through `rerun`, which RUNS the cart"
            );
            assert!(
                panel.charts_for_run.is_some(),
                "and arms the hop back to the charts panel, like any Re-run"
            );
        });
        assert!(
            cx.update(|window, cx| panel.read(cx).focus_handle(cx).is_focused(window)),
            "the pane the cart went to must have been focused"
        );

        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.run_until_parked();
    }

    /// EVERY run ends at its report: a plain Run's stop ingests the run
    /// and hands focus to the charts panel on that run, same as Re-run.
    #[gpui::test]
    async fn test_a_plain_run_hops_to_the_charts_panel_on_stop(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();
        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let charts = workspace.update_in(cx, |workspace, window, cx| {
            ggo_charts_panel::open_charts_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<ggo_charts_panel::ChartsItem>(cx)
                .next()
                .expect("open_charts_item adds the reports tab")
                .read(cx)
                .panel()
                .clone()
        });
        let db_path = dir.path().join("ggo_ide.db");
        charts.update(cx, |charts, _cx| {
            charts.set_db_path_override(db_path.clone());
        });
        panel.update(cx, |panel, _cx| {
            panel.db_path_override = Some(db_path.clone());
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.charts_for_run.is_some(),
                "a plain Run arms the hop to its report"
            );
        });
        await_first_frame(&panel, cx);

        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(panel.ingest_status, IngestStatus::Done(..)),
                "the run must have ingested: {:?}",
                panel.ingest_status
            );
            assert!(
                panel.charts_for_run.is_none(),
                "the arming is spent once the hop has been taken"
            );
        });
        assert!(
            cx.update(|window, cx| charts.read(cx).focus_handle(cx).is_focused(window)),
            "stopping a plain run must hand focus to the charts panel"
        );
    }

    // ------------------------------------ fix round 1: the two red drills

    /// A world file the world panel can load and edit: one entity with a
    /// Transform, in the canonical shape `ggo_worldlib::write_world`
    /// emits.
    fn write_world_fixture(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("assets/worlds")).unwrap();
        std::fs::write(root.join("emerald.toml"), "[project]\n").unwrap();
        std::fs::write(
            root.join("assets/worlds/main.toml"),
            "[[entity]]\n\
             Transform = { pos = [4.0, 4.0], z = 0.0 }\n\
             Text = { content = \"hi\" }\n",
        )
        .unwrap();
    }

    /// Load `assets/worlds/main.toml` into the workspace's world panel,
    /// off the REAL `root`, and dirty it. Returns the panel.
    fn dirty_world_panel(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        root: &std::path::Path,
    ) -> Entity<ggo_world_panel::WorldPanel> {
        let world_panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<ggo_world_panel::WorldPanel>(cx)
                .expect("ggo_world_panel::init adds its panel")
        });
        let root = root.to_path_buf();
        world_panel.update(cx, |panel, _cx| panel.test_root_override(root));
        world_panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("assets/worlds/main.toml", window, cx)
        });
        cx.run_until_parked();
        world_panel.update(cx, |panel, cx| {
            assert!(
                panel.test_dirty_open_world(cx),
                "the fixture world must have loaded, and the edit must have \
                 dirtied it -- otherwise the save this test is about never runs"
            );
        });
        world_panel
    }

    /// **RED DRILL 1.** The no-board diagnostics message is the entry's
    /// entire output, so anything that can overwrite it turns the entry
    /// back into the silent no-op it was written to avoid.
    ///
    /// `run_hardware_diagnostics` begins by stopping whatever is running,
    /// and `stop` -> `finish_run` writes the stopped run's exit reason
    /// ("stopped") from a background completion that lands AFTER the
    /// message is on the row. Guarded only by `run_generation` -- which
    /// this entry never bumps -- it won. This drives a real cart, fires
    /// the entry over the top of it, and checks the message survives.
    #[gpui::test]
    async fn test_the_no_board_message_survives_stopping_a_running_cart(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();
        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ok_capture());
        panel.update(cx, |panel, _cx| {
            panel.proc_runner = runner;
            panel.diag_env_override = Some(menu::DiagEnv {
                bin: menu::DEFAULT_DIAG_BIN.to_string(),
                repo: None,
                ports: Vec::new(),
            });
        });

        // A cart really running, so `stop` really has a run to finish.
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);

        let handler = menu::diagnostics_handler(workspace.downgrade());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        assert!(
            calls.lock().unwrap().is_empty(),
            "no board, nothing spawned"
        );
        panel.update(cx, |panel, _cx| {
            let status = panel.status.clone().expect("a status");
            assert!(
                status.contains(menu::DIAG_REPO_ENV) && status.contains(menu::DIAG_TTY_ENV),
                "the stopped run's exit reason overwrote the message: {status}"
            );
            assert!(panel.status_is_error, "and it must read as a failure");
            assert!(
                matches!(
                    panel.ingest_status,
                    IngestStatus::NoFrames | IngestStatus::Done(..)
                ),
                "the stopped run's INGEST result is still its own and must \
                 still land: {:?}",
                panel.ingest_status
            );
        });
    }

    /// **RED DRILL 2.** `emd pack-ggo` stages worlds from DISK, so a build
    /// that runs after a failed save boots the previous version of the
    /// world -- silently, with the user looking at their edit on screen.
    /// ggo-ide refuses this explicitly ("Dropped on a failed save: never
    /// boot a stale world"); so does this.
    ///
    /// The save is made to fail by taking write permission off the
    /// directory the world lives in -- worldlib writes atomically (temp
    /// file + rename), so a read-only FILE would not stop it.
    #[gpui::test]
    async fn test_a_failed_save_refuses_to_build_rather_than_booting_a_stale_world(
        cx: &mut TestAppContext,
    ) {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        write_world_fixture(dir.path());
        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ok_capture());
        panel.update(cx, |panel, _cx| panel.proc_runner = runner);
        let _world_panel = dirty_world_panel(&workspace, cx, dir.path());

        let worlds_dir = dir.path().join("assets/worlds");
        let restore = std::fs::metadata(&worlds_dir).unwrap().permissions();
        let mut locked = restore.clone();
        locked.set_mode(0o555);
        std::fs::set_permissions(&worlds_dir, locked).unwrap();
        // Running as root would defeat the setup and the test would pass
        // for the wrong reason -- say so instead.
        assert!(
            std::fs::write(worlds_dir.join("probe"), b"").is_err(),
            "the fixture directory is still writable, so this test cannot \
             provoke a failed save (running as root?)"
        );

        let handler = menu::emulate_world_handler(
            workspace.downgrade(),
            "assets/worlds/main.toml".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        std::fs::set_permissions(&worlds_dir, restore).unwrap();

        assert!(
            calls.lock().unwrap().is_empty(),
            "NOTHING may be built from a world whose edits are not on disk"
        );
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.status.as_deref(), Some(menu::SAVE_FAILED_MESSAGE));
            assert!(panel.status_is_error);
            assert!(panel.selected.is_none(), "and nothing was selected to run");
        });
    }

    /// The happy half of the same wiring: a dirty world is on disk, with
    /// the user's edit in it, BEFORE `emd pack-ggo` is spawned to read it.
    /// Checked by having the fake runner read the file at the moment it is
    /// invoked, which is the only ordering that matters.
    #[gpui::test]
    async fn test_emulate_saves_the_dirty_world_before_the_build_reads_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        write_world_fixture(dir.path());
        let world_path = dir.path().join("assets/worlds/main.toml");
        assert!(
            !std::fs::read_to_string(&world_path).unwrap().contains("50"),
            "the fixture must not already contain the edit"
        );

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let seen: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let recorder = seen.clone();
        let world_path_for_runner = world_path.clone();
        let (runner, calls) = fake_proc_runner(move |_request| {
            *recorder.lock().unwrap() = std::fs::read_to_string(&world_path_for_runner).ok();
            ok_capture()
        });
        panel.update(cx, |panel, _cx| panel.proc_runner = runner);
        let _world_panel = dirty_world_panel(&workspace, cx, dir.path());

        let handler = menu::emulate_world_handler(
            workspace.downgrade(),
            "assets/worlds/main.toml".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        assert_eq!(calls.lock().unwrap().len(), 1, "the build ran");
        let on_disk = seen
            .lock()
            .unwrap()
            .clone()
            .expect("the runner read the world file");
        assert!(
            on_disk.contains("50") && on_disk.contains("60"),
            "the user's edit must be on disk BEFORE pack-ggo reads it, or the \
             build bakes the stale world: {on_disk}"
        );
    }

    /// **The no-hardware case, which is the normal one on a dev machine.**
    /// The entry must focus the panel and put a message naming every
    /// missing prerequisite where the user can read it -- never spawn
    /// anything, and never do nothing.
    #[gpui::test]
    async fn test_hardware_diagnostics_without_a_board_says_what_is_missing(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ok_capture());
        panel.update(cx, |panel, _cx| {
            panel.proc_runner = runner;
            panel.diag_env_override = Some(menu::DiagEnv {
                bin: menu::DEFAULT_DIAG_BIN.to_string(),
                repo: None,
                ports: Vec::new(),
            });
        });

        let handler = menu::diagnostics_handler(workspace.downgrade());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        assert!(
            calls.lock().unwrap().is_empty(),
            "nothing may be spawned without a board"
        );
        panel.update(cx, |panel, _cx| {
            let status = panel.status.clone().expect("the failure must be VISIBLE");
            assert!(status.contains(menu::DIAG_REPO_ENV), "{status}");
            assert!(status.contains(menu::DIAG_TTY_ENV), "{status}");
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<EmulatorItem>(cx).count(),
                1,
                "the tab holding that message must be open"
            );
        });
    }

    /// With both prerequisites satisfied, the entry runs the BUILT-IN
    /// diagnostic cart (`--launch` with no directory) against the board,
    /// out of the repo checkout, and puts the transcript in the console.
    #[gpui::test]
    async fn test_hardware_diagnostics_launches_the_builtin_cart(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ggo_common::ProcCapture {
            ok: true,
            lines: vec!["<<<GEMOS launched>>>".to_string()],
        });
        panel.update(cx, |panel, _cx| {
            panel.proc_runner = runner;
            panel.diag_env_override = Some(menu::DiagEnv {
                bin: "ggo-diag".to_string(),
                repo: Some(dir.path().to_path_buf()),
                ports: vec!["/dev/ttyFAKE".to_string()],
            });
        });

        let handler = menu::diagnostics_handler(workspace.downgrade());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].bin, "ggo-diag");
        assert_eq!(calls[0].cwd, dir.path());
        assert_eq!(calls[0].args, menu::diag_args("/dev/ttyFAKE"));

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.status.as_deref(),
                Some("hardware diagnostics finished")
            );
            let console = panel.console.as_ref().expect("a console was opened");
            assert!(
                console.peek_tail(10).iter().any(|l| l.contains("GEMOS")),
                "the transcript must reach the console: {:?}",
                console.peek_tail(10)
            );
        });
    }

    // ------------------------------- wave 3: input + transport, as the user

    /// [`windowed_panel`] with the panel focused in an ACTIVE window, so
    /// keystrokes and key events dispatch along the panel's focus path
    /// (an inactive window's focus paths are blanked in the draw's focus
    /// phase, same note as `ggo_world_panel`'s windowed tests carry).
    fn focused_panel(cx: &mut TestAppContext) -> (Entity<EmuPanel>, &mut gpui::VisualTestContext) {
        let (panel, cx) = windowed_panel(cx);
        cx.update(|window, _| window.activate_window());
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();
        (panel, cx)
    }

    /// Select the green fixture cart the way the explorer does, off a real
    /// temp `root`, with the ingest pointed at a temp database.
    fn select_green_cart(
        panel: &Entity<EmuPanel>,
        cx: &mut gpui::VisualTestContext,
        root: &std::path::Path,
    ) {
        std::fs::write(root.join("green.cart"), drive::fixture::green_screen_cart()).unwrap();
        panel.update(cx, |panel, _cx| {
            panel.root_override = Some(root.to_path_buf());
            panel.db_path_override = Some(root.join("ggo_ide.db"));
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();
    }

    /// Click the element `debug_selector` names in the rendered window.
    fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} must be rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    /// **The registered transport keybindings, end to end.** `init` binds
    /// `ctrl-alt-r`/`ctrl-alt-s`/`ctrl-alt-m` in the panel's key context;
    /// typing them into the focused pane must start, stop and mute through
    /// the SAME handlers the `run`/`stop`/`toggle_mute` tests cover -- if
    /// this fails, `bind_panel_keys` (or the `on_action` wiring in
    /// `render`) is gone.
    #[gpui::test]
    async fn test_transport_keybindings_run_stop_and_mute_the_pane(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let (panel, cx) = cx.add_window_view(EmuPanel::test_new);
        cx.update(|window, _| window.activate_window());
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();
        select_green_cart(&panel, cx, dir.path());

        // Mute before running -- the pane must be mutable while idle, and
        // during a run the device may (on a headless CI box) be
        // Unavailable, where the toggle is deliberately inert.
        cx.simulate_keystrokes("ctrl-alt-m");
        panel.read_with(cx, |panel, _| {
            assert!(panel.audio.is_muted(), "ctrl-alt-m must mute the pane");
        });
        cx.simulate_keystrokes("ctrl-alt-m");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.audio.is_muted(), "and toggle back");
        });

        cx.simulate_keystrokes("ctrl-alt-r");
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.is_running(),
                "ctrl-alt-r must start the selected cart"
            );
        });
        await_first_frame(&panel, cx);

        cx.simulate_keystrokes("ctrl-alt-s");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_running(), "ctrl-alt-s must stop the run");
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.status.as_deref(), Some("stopped"));
            assert!(
                matches!(panel.ingest_status, IngestStatus::Done(..)),
                "the keybinding-stopped run still ingests: {:?}",
                panel.ingest_status
            );
        });
    }

    /// `ctrl-alt-p` pauses and resumes; `ctrl-alt-.` pauses a running cart
    /// and steps a paused one; the readout says so; Stop clears it all.
    #[gpui::test]
    async fn test_pause_and_step_keybindings_drive_the_session(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let (panel, cx) = cx.add_window_view(EmuPanel::test_new);
        cx.update(|window, _| window.activate_window());
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();
        select_green_cart(&panel, cx, dir.path());

        cx.simulate_keystrokes("ctrl-alt-p");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_paused(), "nothing to pause before a run");
        });
        cx.simulate_keystrokes("ctrl-alt-r");
        await_first_frame(&panel, cx);
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_paused(), "a run starts unpaused");
            assert!(
                panel
                    .transport_readout()
                    .unwrap()
                    .starts_with("green.cart · frame "),
                "{:?}",
                panel.transport_readout()
            );
        });

        cx.simulate_keystrokes("ctrl-alt-.");
        panel.read_with(cx, |panel, _| {
            assert!(panel.is_paused(), "step while running pauses");
            assert!(panel.transport_readout().unwrap().ends_with(" · paused"));
        });
        cx.simulate_keystrokes("ctrl-alt-.");
        panel.read_with(cx, |panel, _| {
            assert!(panel.is_paused(), "step while paused stays paused");
        });
        cx.simulate_keystrokes("ctrl-alt-p");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_paused(), "ctrl-alt-p resumes");
        });
        cx.simulate_keystrokes("ctrl-alt-p");
        panel.read_with(cx, |panel, _| assert!(panel.is_paused()));

        cx.simulate_keystrokes("ctrl-alt-s");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_running() && !panel.is_paused());
            assert_eq!(panel.transport_readout(), None);
        });
        cx.run_until_parked();
    }

    /// `ctrl-alt-d` opens the debug column; a decoded snapshot yields an
    /// image of the tab's size; switching tabs re-decodes; closing the
    /// column queues every image for atlas release and the next render
    /// drains the queue.
    #[gpui::test]
    async fn test_debug_column_decodes_snapshots_per_tab_and_releases_on_close(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let (panel, cx) = cx.add_window_view(EmuPanel::test_new);
        cx.update(|window, _| window.activate_window());
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("ctrl-alt-d");
        panel.read_with(cx, |panel, _| assert!(panel.debug.open, "ctrl-alt-d opens"));

        let mut ppu = ggo_emu_core::ppu::Ppu::new();
        ppu.set_layer(2, true, 1);
        ppu.oam_set(3, 5, 6, 1, 0x10);
        let snapshot = Arc::new(ppu.snapshot());
        panel.update(cx, |panel, _| {
            panel.debug_decode_now(snapshot.clone());
            let decoded = panel.debug.decoded.as_ref().expect("decoded");
            assert_eq!(decoded.tab, debug::DebugTab::Tiles);
            let image = decoded.image.as_ref().expect("the tile sheet is an image");
            assert_eq!(
                image.size(0),
                gpui::size(
                    gpui::DevicePixels(debug::SHEET_PX as i32),
                    gpui::DevicePixels(debug::SHEET_PX as i32)
                )
            );
        });
        panel.update(cx, |panel, cx| {
            panel.set_debug_tab(debug::DebugTab::Oam, cx);
            assert_eq!(
                panel.debug.last_decoded_ptr, 0,
                "a tab switch forces a re-decode"
            );
            panel.debug_decode_now(snapshot.clone());
            let decoded = panel.debug.decoded.as_ref().unwrap();
            assert_eq!(decoded.tab, debug::DebugTab::Oam);
            assert_eq!(
                panel.debug.retired.len(),
                1,
                "the tile sheet is queued for release"
            );
            let image = decoded.image.as_ref().unwrap();
            assert_eq!(
                image.size(0),
                gpui::size(gpui::DevicePixels(320), gpui::DevicePixels(240))
            );
            panel.set_debug_tab(debug::DebugTab::Palettes, cx);
            panel.debug_decode_now(snapshot.clone());
            assert!(
                panel.debug.decoded.as_ref().unwrap().image.is_none(),
                "palettes paint directly"
            );
            assert_eq!(panel.debug.retired.len(), 2);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.debug.retired.is_empty(),
                "a render advanced the retire queue"
            );
            assert_eq!(
                panel.debug.retired_previous.len(),
                2,
                "and holds them one more render before dropping"
            );
        });

        cx.simulate_keystrokes("ctrl-alt-d");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.debug.open, "ctrl-alt-d closes");
            assert!(panel.debug.decoded.is_none());
            assert!(
                panel.debug.retired.is_empty() && panel.debug.retired_previous.is_empty(),
                "closing released everything at once"
            );
        });
    }

    /// The live path: with the column open, a running cart's snapshots
    /// are decoded off-thread into the active tab; a tab switch after the
    /// run ends re-decodes from the kept snapshot.
    #[gpui::test]
    async fn test_debug_column_decodes_a_running_carts_snapshots(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let (panel, cx) = cx.add_window_view(EmuPanel::test_new);
        cx.update(|window, _| window.activate_window());
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();
        select_green_cart(&panel, cx, dir.path());
        cx.simulate_keystrokes("ctrl-alt-d");
        cx.simulate_keystrokes("ctrl-alt-r");
        await_first_frame(&panel, cx);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while panel.read_with(cx, |panel, _| panel.debug.decoded.is_none()) {
            assert!(std::time::Instant::now() < deadline, "no decode landed");
            std::thread::sleep(std::time::Duration::from_millis(10));
            panel.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
        panel.read_with(cx, |panel, _| {
            let decoded = panel.debug.decoded.as_ref().unwrap();
            assert_eq!(decoded.tab, debug::DebugTab::Tiles);
            assert!(decoded.image.is_some());
            // The green cart sets backdrop entry 0; palette 0 entry 0 is green.
            assert_eq!(decoded.snapshot.palette_rgb565(0, 0, 0), 0x07E0);
        });

        cx.simulate_keystrokes("ctrl-alt-s");
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.set_debug_tab(debug::DebugTab::Palettes, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let decoded = panel
                .debug
                .decoded
                .as_ref()
                .expect("re-decoded from the kept snapshot");
            assert_eq!(
                decoded.tab,
                debug::DebugTab::Palettes,
                "the tab switch re-decoded without a session"
            );
        });
    }

    /// Another tab taking the pane pauses the cart (the item is
    /// deactivated and stops rendering); coming back resumes it; a pause
    /// the user made is never auto-resumed.
    #[gpui::test]
    async fn test_hidden_tab_auto_pause_resumes_on_return_but_never_a_user_pause(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();
        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);
        let emu_item = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<EmulatorItem>(cx)
                .next()
                .expect("the emulator tab")
        });

        let hide = |cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                ggo_charts_panel::open_charts_item(workspace, window, cx, |_, _, _| {});
            });
            cx.run_until_parked();
        };
        let show = |cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_item(&emu_item, true, true, window, cx);
            });
            cx.run_until_parked();
        };

        hide(cx);
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.is_paused() && panel.auto_paused,
                "a hidden tab pauses"
            );
        });
        show(cx);
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel.is_paused() && !panel.auto_paused,
                "coming back resumes"
            );
        });

        panel.update(cx, |panel, cx| panel.toggle_pause(cx));
        hide(cx);
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.is_paused() && !panel.auto_paused,
                "a user pause is not adopted"
            );
        });
        show(cx);
        panel.read_with(cx, |panel, _| {
            assert!(panel.is_paused(), "and coming back leaves it alone");
        });
        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.run_until_parked();
    }

    /// A window that is no longer active gets no key-up events, so the
    /// pad is released on the next render instead of staying latched.
    #[gpui::test]
    async fn test_an_inactive_window_releases_the_pad(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let (panel, cx) = cx.add_window_view(EmuPanel::test_new);
        cx.update(|window, _| window.activate_window());
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.on_key("z", true);
            assert_ne!(panel.input.mask(), 0, "a held key is latched");
            cx.notify();
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_ne!(panel.input.mask(), 0, "an active window keeps it held");
        });

        cx.deactivate_window();
        panel.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.input.mask(),
                0,
                "the render while inactive released it"
            );
        });
    }

    /// **The pad listeners on the rendered root, end to end.** The unit
    /// test above (`test_key_handling_latches_and_releases_the_pad`) calls
    /// `on_key` directly; this pins the `on_key_down`/`on_key_up`/
    /// `on_modifiers_changed` wiring in `render` by dispatching real
    /// platform events at a focused pane with a live run: press latches,
    /// release clears, and a modifiers change routes shift-as-SELECT.
    #[gpui::test]
    async fn test_platform_key_events_drive_the_pad_through_the_element_listeners(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = focused_panel(cx);
        select_green_cart(&panel, cx, dir.path());
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);

        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("z").unwrap(),
            is_held: false,
            prefer_character_input: false,
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.input.mask() & (1 << 0),
                1 << 0,
                "a real key-down must latch A through the rendered listener"
            );
        });
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("left").unwrap(),
            is_held: false,
            prefer_character_input: false,
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.input.mask(), (1 << 0) | (1 << 6));
        });

        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("z").unwrap(),
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.input.mask(),
                1 << 6,
                "a real key-up must clear exactly the released button"
            );
        });

        // SELECT rides the modifier state, not a keystroke -- the
        // `on_modifiers_changed` listener is the only path.
        cx.simulate_modifiers_change(gpui::Modifiers {
            shift: true,
            ..Default::default()
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.input.mask(), (1 << 6) | input::SELECT_BIT);
        });
        cx.simulate_modifiers_change(gpui::Modifiers::default());
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.input.mask(),
                1 << 6,
                "releasing shift must clear SELECT and nothing else"
            );
        });

        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.run_until_parked();
    }

    /// **The transport buttons, as clicks on the rendered pane.** The
    /// `IconButton`s register `ICON-<name>` debug bounds; clicking those
    /// bounds must run, stop and mute through the listeners `render_
    /// transport`/`render_mute_button` wire -- the same handlers every
    /// method-level test covers, now reached the way a user reaches them.
    #[gpui::test]
    async fn test_transport_button_clicks_run_stop_and_mute_the_pane(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = focused_panel(cx);
        select_green_cart(&panel, cx, dir.path());

        // Mute first (see the keybinding test for why: while idle the
        // toggle is guaranteed live), and the icon must flip to the
        // crossed-out speaker.
        click(cx, "ICON-AudioOn");
        panel.read_with(cx, |panel, _| {
            assert!(panel.audio.is_muted(), "clicking the speaker must mute");
        });
        assert!(
            cx.debug_bounds("ICON-AudioOff").is_some(),
            "the mute button must now show the muted icon"
        );
        click(cx, "ICON-AudioOff");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.audio.is_muted(), "and clicking again unmutes");
        });

        click(cx, "ICON-PlayFilled");
        panel.read_with(cx, |panel, _| {
            assert!(panel.is_running(), "the Run button must start the cart");
        });
        await_first_frame(&panel, cx);

        click(cx, "ICON-Stop");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_running(), "the Stop button must end the run");
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.status.as_deref(), Some("stopped"));
            assert!(
                matches!(panel.ingest_status, IngestStatus::Done(..)),
                "a click-stopped run still ingests: {:?}",
                panel.ingest_status
            );
        });
    }

    // ------------------------------------------- wave 3: the failed ingest

    /// Malformed perf JSON fails the ingest -- with a reason, and before
    /// the database is touched. Driven through the same named function
    /// `finish_run`'s background task calls, with a hand-built
    /// `FinishedRun` (the real emulator can only emit well-formed perf, so
    /// this seam is the only way to reach the parse failure).
    #[test]
    fn a_malformed_perf_json_fails_the_ingest_before_touching_the_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let finished = drive::FinishedRun {
            reason: "cart exited".to_string(),
            is_error: false,
            perf: Some(drive::PerfSnapshot {
                cart: "Green Fix".to_string(),
                perf_json: "this is not perf json".to_string(),
                frames: 3,
            }),
            uart: vec!["[run] green.cart".to_string()],
        };

        let status = ingest_finished_run(&finished, Some(db_path.clone()), "green.cart");

        let IngestStatus::Failed(reason) = &status else {
            panic!("malformed perf output must fail the ingest: {status:?}");
        };
        assert!(!reason.is_empty(), "the failure must carry emd's reason");
        let label = status.label().expect("a failed ingest has a row to show");
        assert!(
            label.starts_with("perf ingest failed:"),
            "the row must read as a failure: {label}"
        );
        assert!(
            !db_path.exists(),
            "a run that cannot be parsed must not create (or dirty) the db"
        );
    }

    /// A run whose ingest fails must SAY so on the pane: the whole
    /// end-of-run path, with the database made unopenable (a directory
    /// where the file must go), ends in `IngestStatus::Failed` and the
    /// error row `render` shows for it.
    #[gpui::test]
    async fn test_a_failed_ingest_surfaces_on_the_panel(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("green.cart"),
            drive::fixture::green_screen_cart(),
        )
        .unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        std::fs::create_dir(&db_path).unwrap();

        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, _cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.db_path_override = Some(db_path);
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("green.cart", window, cx)
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.run(window, cx));
        await_first_frame(&panel, cx);
        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            let IngestStatus::Failed(reason) = &panel.ingest_status else {
                panic!(
                    "an unopenable db must surface a failed ingest: {:?}",
                    panel.ingest_status
                );
            };
            assert!(!reason.is_empty());
            let label = panel
                .ingest_status
                .label()
                .expect("the failed ingest renders a row");
            assert!(label.starts_with("perf ingest failed:"), "{label}");
            assert_eq!(
                panel.status.as_deref(),
                Some("stopped"),
                "the run itself ended normally -- only the ingest failed"
            );
        });
    }

    // ------------------------------------------------------ watch (task 8)

    #[test]
    fn watch_triggers_skips_pack_output_sidecars_and_removals() {
        use project::PathChange;
        assert!(watch_triggers("assets/tiles/a.til", &PathChange::Updated));
        assert!(watch_triggers(
            "assets/worlds/main.toml",
            &PathChange::AddedOrUpdated
        ));
        assert!(!watch_triggers(
            "target/ggo-emulate/worlds-main.ggo",
            &PathChange::Added
        ));
        assert!(
            !watch_triggers("game/target/ggo-emulate/x.ggo", &PathChange::Added),
            "nested project"
        );
        assert!(
            !watch_triggers("target", &PathChange::Added),
            "the output dir itself"
        );
        assert!(!watch_triggers(
            ".ggo-ide/assets/tiles/a.til.editor.json",
            &PathChange::Updated
        ));
        assert!(
            watch_triggers("assets/tiles/a.til", &PathChange::Removed),
            "a deleted asset re-packs"
        );
        assert!(
            !watch_triggers("assets/tiles/a.til", &PathChange::Loaded),
            "the initial scan"
        );
    }

    #[gpui::test]
    async fn test_watch_mode_repacks_after_a_save_and_ignores_its_own_output(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\n").unwrap();
        std::fs::write(dir.path().join("assets/worlds/main.toml"), "").unwrap();

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_proc_runner(|_| ok_capture());
        panel.update(cx, |panel, _cx| panel.proc_runner = runner);
        let fs = workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).fs().clone()
        });
        let fake_fs = fs.as_fake();

        panel.update_in(cx, |panel, window, cx| {
            panel.set_watch(true, window, cx);
            assert!(!panel.watch, "nothing to watch before a world was emulated");
        });
        workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::emulate_world(workspace, "assets/worlds/main.toml", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1);
        panel.update_in(cx, |panel, window, cx| panel.set_watch(true, window, cx));
        assert!(panel.read_with(cx, |panel, _| panel.watch));

        fake_fs
            .insert_tree(
                "/proj",
                serde_json::json!({ "assets": { "tiles": {} }, "target": { "ggo-emulate": {} } }),
            )
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        let after_dirs = calls.lock().unwrap().len();

        // Two quick saves collapse into one re-pack.
        fake_fs
            .insert_file("/proj/assets/tiles/a.til", b"til".to_vec())
            .await;
        fake_fs
            .insert_file("/proj/assets/tiles/a.pal", b"pal".to_vec())
            .await;
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), after_dirs, "still debouncing");
        cx.executor().advance_clock(WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        let after_save = calls.lock().unwrap().len();
        assert_eq!(after_save, after_dirs + 1, "one re-pack after the debounce");

        // The pack's own output landing in the worktree must not loop.
        fake_fs
            .insert_file("/proj/target/ggo-emulate/worlds-main.ggo", b"cart".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            after_save,
            "no rebuild for the output"
        );

        // The debounce restarts on every change: two saves 200 ms apart
        // rebuild once, after the second.
        fake_fs
            .insert_file("/proj/assets/tiles/c.til", b"til".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(WATCH_DEBOUNCE * 2 / 3);
        cx.run_until_parked();
        fake_fs
            .insert_file("/proj/assets/tiles/d.til", b"til".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(WATCH_DEBOUNCE * 2 / 3);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            after_save,
            "the second save re-armed the debounce"
        );
        cx.executor().advance_clock(WATCH_DEBOUNCE);
        cx.run_until_parked();
        let after_pair = calls.lock().unwrap().len();
        assert_eq!(after_pair, after_save + 1, "one rebuild for the pair");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.watch_restart_pending, "the restart flag is consumed");
            assert!(!panel.building);
        });

        panel.update_in(cx, |panel, window, cx| panel.set_watch(false, window, cx));
        fake_fs
            .insert_file("/proj/assets/tiles/b.til", b"til".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), after_pair, "off means off");
    }

    // ---------------------------------------------- flash to hardware

    /// A streamer that replays `lines` and then reports `ok`, recording
    /// every request it was handed.
    fn fake_streamer(
        lines: Vec<&'static str>,
        ok: bool,
    ) -> (
        ggo_common::ProcStreamer,
        Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>>,
    ) {
        let calls: Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let streamer: ggo_common::ProcStreamer = Arc::new(move |request, mut on_line| {
            recorded.lock().unwrap().push(request);
            let lines = lines.clone();
            Box::pin(async move {
                for line in &lines {
                    on_line(line);
                }
                ggo_common::ProcCapture {
                    ok,
                    lines: lines.iter().map(|l| l.to_string()).collect(),
                }
            })
        });
        (streamer, calls)
    }

    /// A panel whose machine looks ready to flash, with a scripted
    /// streamer standing in for `ggo-diag`.
    fn flashable_panel<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
        streamer: ggo_common::ProcStreamer,
    ) -> (Entity<EmuPanel>, &'a mut gpui::VisualTestContext) {
        let root = root.to_path_buf();
        // A windowed view renders, and rendering needs the theme.
        cx.update(|cx| {
            AppState::test(cx);
        });
        cx.add_window_view(|window, cx| {
            let mut panel = EmuPanel::new(None, Some(window), cx);
            panel.root_override = Some(root);
            panel.proc_streamer = streamer;
            panel
        })
    }

    /// A ready env pointing at real fixture dirs, so `flash_request`
    /// builds a command without needing a board attached.
    fn ready_hardware(root: &std::path::Path) -> hardware::HardwareEnv {
        hardware::HardwareEnv {
            diag_bin: Some("ggo-diag".into()),
            emd_bin: Some("emd".into()),
            repo: Some(root.join("repo")),
            emerald: None,
            ports: vec!["/dev/ttyUSB0".into()],
            stuck_board: false,
            project: Some(root.join("game")),
            cargo: true,
            git: true,
            clone_dest: root.join(".ggo/ggo"),
            home: root.to_path_buf(),
            repo_commit: None,
            emu_commit: None,
            emu_commit_in_repo: None,
        }
    }

    /// The happy path: every recognised line moves the status row, and a
    /// `RESULT: PASS` lands as a passing verdict.
    #[gpui::test]
    async fn test_a_flash_run_streams_stages_and_reports_pass(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, calls) = fake_streamer(
            vec![
                "==> Provision SD card",
                "  [boot] banner — GemOS",
                "diag step 1: PASS",
                "RESULT: PASS",
            ],
            true,
        );
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
            assert!(panel.is_flashing(), "the run is in flight");
        });
        cx.executor().run_until_parked();

        assert_eq!(calls.lock().unwrap().len(), 1, "one ggo-diag invocation");
        let args = &calls.lock().unwrap()[0].args;
        assert!(
            args.contains(&"--project".to_string()) && args.contains(&"--skip-pnr".to_string())
        );
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_flashing(), "the run finished");
            assert_eq!(panel.status.as_deref(), Some("flashing: PASS"));
            assert!(!panel.status_is_error);
            let console = panel.console.as_ref().expect("the console took the lines");
            assert!(
                console.lines().iter().any(|l| l.contains("[boot] banner")),
                "raw lines reach the console: {:?}",
                console.lines()
            );
        });
    }

    /// The run id the scripted transcript announces, and the one the
    /// seeded `diag.db` records that flash under.
    const FLASHED_RUN: &str = "20260831T120000Z-abc123def0";

    /// A `diag.db` holding one finished device run WITH telemetry: the
    /// `runs` row `ggo-diag` writes, plus the `cart`/`run`/`frame` rows its
    /// `perf_run_id` points at -- exactly what `diag_db::clone_runs` pulls
    /// across into our own database.
    fn seed_flashed_run(diag_db: &std::path::Path) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(diag_db).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO runs (id, started_at, branch, commit_hash, git_describe, \
                 hostname, state, verdict) VALUES (?1, '2026-08-31T12:00:00Z', 'main', \
                 'abc123', 'v1.2.3', 'test-host', 'done', 'PASS')",
                [FLASHED_RUN],
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO cart(name) VALUES ('device:slop_battle')",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO run (cart_id, started_at, frames, frame_budget_cycles) \
                 SELECT id, '2026-08-31T12:00:02Z', 2, 555549 FROM cart \
                 WHERE name = 'device:slop_battle'",
                (),
            )
            .await
            .unwrap();
            let perf_id = conn.last_insert_rowid();
            for (n, cyc) in [(0i64, 111_000i64), (1, 999_999)] {
                conn.execute(
                    "INSERT INTO frame (run_id, n, cyc, wire_total, over_budget) \
                     VALUES (?1, ?2, ?3, 0, ?4)",
                    (perf_id, n, cyc, i64::from(cyc > 555_549)),
                )
                .await
                .unwrap();
            }
            conn.execute(
                "UPDATE runs SET perf_run_id = ?2 WHERE id = ?1",
                (FLASHED_RUN, perf_id),
            )
            .await
            .unwrap();
        });
    }

    /// The board's half of "every finished run routes to its report": a
    /// flash that passes AND said which run it recorded opens the reports
    /// page on that run's telemetry, cloned out of ggo-diag's own file.
    #[gpui::test]
    async fn test_a_passing_flash_opens_the_report_for_the_run_it_recorded(
        cx: &mut TestAppContext,
    ) {
        // The clone and the lookup each block on their own tokio runtime.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let diag_db = dir.path().join("diag.db");
        let ide_db = dir.path().join("ggo_ide.db");
        seed_flashed_run(&diag_db);

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let charts = workspace.update_in(cx, |workspace, window, cx| {
            ggo_charts_panel::open_charts_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<ggo_charts_panel::ChartsItem>(cx)
                .next()
                .expect("open_charts_item adds the reports tab")
                .read(cx)
                .panel()
                .clone()
        });
        // Both panels on the SAME temp database, so the run this flash
        // clones is the run the charts panel then reads back.
        charts.update(cx, |charts, _cx| {
            charts.set_db_path_override(ide_db.clone());
        });

        let (streamer, _calls) = fake_streamer(
            vec![
                "==> Report",
                "[db] run 20260831T120000Z-abc123def0: 2 uart lines, 2 frames -> diag.db",
                "RESULT: PASS",
            ],
            true,
        );
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, window, cx| {
            panel.proc_streamer = streamer;
            panel.db_path_override = Some(ide_db.clone());
            panel.diag_db_path_override = Some(diag_db.clone());
            // What `flash_to_board_with` arms. `start_board_run` is entered
            // directly because that entry re-probes this machine for a
            // board first, and a test has none.
            panel.flash_charts_window = Some(window.window_handle());
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.status.as_deref(), Some("flashing: PASS"));
            assert_eq!(
                panel
                    .last_flash
                    .as_ref()
                    .and_then(|(progress, _)| progress.diag_run_id.as_deref()),
                Some(FLASHED_RUN),
                "the retired timeline remembers which run this flash was"
            );
            assert!(
                panel.flash_charts_window.is_none(),
                "the arming is spent by the run that consumed it"
            );
        });
        assert!(
            diag_db::device_perf_run_id(&ide_db, FLASHED_RUN)
                .expect("our own database reads")
                .is_some(),
            "the flash cloned its own telemetry into the db the page reads"
        );
        assert!(
            cx.update(|window, cx| charts.read(cx).focus_handle(cx).is_focused(window)),
            "a passing flash must hand focus to the report it just produced"
        );
    }

    /// Add an OLDER device run that this build can never clone, the way a
    /// real `~/.ggo/diag.db` a migration behind cannot have its device runs
    /// cloned (its `frame` table has no `cyc`). The cause here is a cart
    /// with no name -- a per-run failure like that one, and one that can be
    /// induced for a single run without disturbing the flashed run's rows.
    /// Its id sorts FIRST, so it is attempted before the flashed run.
    fn seed_unclonable_legacy_run(diag_db: &std::path::Path) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(diag_db).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO runs (id, started_at, branch, commit_hash, git_describe, \
                 hostname, state, verdict) VALUES ('20260101T000000Z-0000000000', \
                 '2026-01-01T00:00:00Z', 'main', 'old123', 'v1.0.0', 'test-host', \
                 'done', 'PASS')",
                (),
            )
            .await
            .unwrap();
            conn.execute("INSERT INTO cart(name) VALUES (NULL)", ())
                .await
                .unwrap();
            let cart_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO run (cart_id, started_at, frames, frame_budget_cycles) \
                 VALUES (?1, '2026-01-01T00:00:02Z', 1, 555549)",
                [cart_id],
            )
            .await
            .unwrap();
            let perf_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE runs SET perf_run_id = ?1 WHERE id = '20260101T000000Z-0000000000'",
                [perf_id],
            )
            .await
            .unwrap();
        });
    }

    /// **The hop may not be held hostage by somebody else's run.**
    /// `clone_runs` reconciles every run in ggo-diag's file, so one
    /// permanently-unclonable historical run makes it return `Err` on
    /// EVERY call -- and on a real machine with a `diag.db` a migration
    /// behind, that is every call forever. The just-flashed run still
    /// cloned and committed (each run is its own transaction), so its
    /// report must still open.
    #[gpui::test]
    async fn test_a_passing_flash_opens_its_report_despite_an_unclonable_older_run(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let diag_db = dir.path().join("diag.db");
        let ide_db = dir.path().join("ggo_ide.db");
        seed_unclonable_legacy_run(&diag_db);
        seed_flashed_run(&diag_db);
        // The premise, asserted rather than assumed: the clone really does
        // fail, for the older run and not for this one.
        let err = diag_db::clone_runs(&diag_db, &ide_db).expect_err("the older run cannot clone");
        assert!(err.contains("20260101T000000Z-0000000000"), "{err}");
        assert!(!err.contains(FLASHED_RUN), "{err}");

        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let charts = workspace.update_in(cx, |workspace, window, cx| {
            ggo_charts_panel::open_charts_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<ggo_charts_panel::ChartsItem>(cx)
                .next()
                .expect("open_charts_item adds the reports tab")
                .read(cx)
                .panel()
                .clone()
        });
        charts.update(cx, |charts, _cx| {
            charts.set_db_path_override(ide_db.clone());
        });

        let (streamer, _calls) = fake_streamer(
            vec![
                "==> Report",
                "[db] run 20260831T120000Z-abc123def0: 2 uart lines, 2 frames -> diag.db",
                "RESULT: PASS",
            ],
            true,
        );
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, window, cx| {
            panel.proc_streamer = streamer;
            panel.db_path_override = Some(ide_db.clone());
            panel.diag_db_path_override = Some(diag_db.clone());
            panel.flash_charts_window = Some(window.window_handle());
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            diag_db::device_perf_run_id(&ide_db, FLASHED_RUN)
                .expect("our own database reads")
                .is_some(),
            "the flashed run's own telemetry cloned regardless of the older run"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.last_flash_perf_run.as_ref().map(|(id, _)| id.as_str()),
                Some(FLASHED_RUN),
                "the hop resolved the report id rather than bailing on the Err"
            );
        });
        assert!(
            cx.update(|window, cx| charts.read(cx).focus_handle(cx).is_focused(window)),
            "and the report still opens"
        );
    }

    /// A run that never named a run id -- a `ggo-diag` too old to print the
    /// line, and every setup or `git pull` run -- passes without hopping.
    /// Nothing is cloned and nothing is opened; the PASS stands alone.
    #[gpui::test]
    async fn test_a_run_that_recorded_nothing_opens_no_report(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let diag_db = dir.path().join("diag.db");
        let ide_db = dir.path().join("ggo_ide.db");
        // Telemetry IS sitting there to be found: what is missing is the
        // transcript line saying which run this flash was.
        seed_flashed_run(&diag_db);

        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board", "RESULT: PASS"], true);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, window, cx| {
            panel.proc_streamer = streamer;
            panel.db_path_override = Some(ide_db.clone());
            panel.diag_db_path_override = Some(diag_db.clone());
            panel.flash_charts_window = Some(window.window_handle());
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.status.as_deref(), Some("flashing: PASS"));
            assert!(!panel.status_is_error);
            assert!(
                panel
                    .last_flash
                    .as_ref()
                    .and_then(|(progress, _)| progress.diag_run_id.as_deref())
                    .is_none(),
                "the transcript named no run"
            );
        });
        assert!(
            !ide_db.exists(),
            "with no run to look up, nothing is cloned and no report opens"
        );
    }

    /// The run's shape, not just its last line: every announced phase
    /// lands on the timeline, and the finished run stays on the page
    /// instead of vanishing with the task.
    #[gpui::test]
    async fn test_a_flash_run_builds_a_phase_timeline(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(
            vec![
                "==> Compile firmware",
                "--> component cpu",
                "==> Flash board",
                "diag step 1: PASS",
                "RESULT: PASS",
            ],
            true,
        );
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            )
        });
        cx.executor().run_until_parked();

        panel.read_with(cx, |panel, _| {
            let (progress, _elapsed) = panel
                .flash_progress()
                .expect("the finished run stays on the page");
            assert_eq!(progress.verdict(), Some(true));
            let rows: Vec<(&str, hardware::PhaseState)> = progress
                .rows()
                .iter()
                .map(|row| (row.title.as_str(), row.state))
                .collect();
            assert_eq!(
                rows,
                vec![
                    ("Compile firmware", hardware::PhaseState::Done),
                    ("Flash board", hardware::PhaseState::Done),
                ],
                "the announced phases, in order, all closed",
            );
            assert_eq!(
                progress.diag_steps().len(),
                1,
                "the diagnostic step is on the page too",
            );
        });
    }

    /// A failed run keeps the timeline: which phase died is the whole
    /// point of looking at the page afterwards.
    #[gpui::test]
    async fn test_a_failed_flash_leaves_the_dead_phase_on_the_page(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board", "fujprog: no board"], false);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            )
        });
        cx.executor().run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_flashing());
            let (progress, _elapsed) = panel.flash_progress().expect("the dead run stays");
            assert_eq!(progress.verdict(), Some(false));
            let dead = progress
                .rows()
                .iter()
                .find(|row| row.state == hardware::PhaseState::Failed)
                .expect("the phase that died");
            assert_eq!(dead.title, "Flash board");
        });
    }

    /// Starting a second run clears the last one's timeline, so a stale
    /// PASS can never sit above a running flash.
    #[gpui::test]
    async fn test_a_new_run_replaces_the_previous_timeline(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board", "RESULT: PASS"], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request.clone()],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            )
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
            let (progress, _) = panel.flash_progress().expect("the run in flight");
            assert_eq!(progress.verdict(), None, "the old verdict is gone");
            assert!(
                progress
                    .rows()
                    .iter()
                    .all(|row| row.state == hardware::PhaseState::Pending),
                "a fresh pipeline",
            );
        });
    }

    /// A `RESULT: FAIL` is a failed run even when the process exits 0 --
    /// the verdict is the CLI's, not the shell's.
    #[gpui::test]
    async fn test_a_fail_verdict_is_an_error_even_on_a_zero_exit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board", "RESULT: FAIL"], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            )
        });
        cx.executor().run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.status_is_error, "a FAIL verdict is an error");
            assert!(
                panel
                    .status
                    .as_deref()
                    .unwrap_or("")
                    .contains("flashing failed"),
                "{:?}",
                panel.status
            );
            assert!(!panel.is_flashing());
        });
    }

    /// A non-zero exit with no verdict line still fails, and the reason
    /// is the last thing the child said.
    #[gpui::test]
    async fn test_a_nonzero_exit_without_a_verdict_still_fails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board", "fujprog: no board"], false);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            )
        });
        cx.executor().run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.status_is_error);
            assert!(
                panel
                    .status
                    .as_deref()
                    .unwrap_or("")
                    .contains("fujprog: no board"),
                "the child's own words: {:?}",
                panel.status
            );
        });
    }

    /// Pressing the button during a run cancels it: the task is dropped
    /// (which kills the child) and nothing is left in flight.
    #[gpui::test]
    async fn test_pressing_flash_again_cancels_the_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board"], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, window, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
            assert!(panel.is_flashing());
            panel.flash_to_board_with(Some("worlds/arena"), false, window, cx);
            assert!(!panel.is_flashing(), "the second press cancelled");
            assert_eq!(panel.status.as_deref(), Some("flash cancelled"));
            assert_eq!(
                panel.flash_world(cx),
                None,
                "a press that only cancelled started nothing, so it has no \
                 business changing what the next flash boots"
            );
        });
    }

    /// Cancelling actually drops the run future -- the thing whose drop
    /// kills the child. The flags saying "cancelled" while the future
    /// lives on would be a ggo-diag that keeps flashing a board nobody
    /// can stop.
    #[gpui::test]
    async fn test_cancel_drops_the_run_future(cx: &mut TestAppContext) {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = dropped.clone();
        // A run that never finishes on its own, like a 20-minute PnR.
        let streamer: ggo_common::ProcStreamer = Arc::new(move |_request, _on_line| {
            let guard = DropFlag(flag.clone());
            Box::pin(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                unreachable!("the run never completes; cancel must drop it")
            })
        });
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, _window, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
        });
        cx.run_until_parked();
        assert!(
            !dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the run is in flight"
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.flash_to_board_with(None, false, window, cx);
            assert!(!panel.is_flashing());
        });
        cx.run_until_parked();
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "cancel dropped the run future, which is what kills the child"
        );
    }

    /// A machine with nothing set up spawns NOTHING and names every gap.
    #[gpui::test]
    async fn test_flashing_without_the_prerequisites_spawns_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, calls) = fake_streamer(vec![], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        panel.update_in(cx, |panel, window, cx| {
            // No project, no repo, no board.
            panel.root_override = None;
            panel.flash_to_board(window, cx);
            assert!(!panel.is_flashing(), "nothing to flash yet");
        });
        // The page needs a workspace to open into; this panel has none,
        // so the only thing being asserted here is that nothing ran.
        cx.run_until_parked();
        assert!(calls.lock().unwrap().is_empty(), "nothing was spawned");
    }

    /// The agent socket's flash gets the gap as an ERROR, not as a page:
    /// there is nobody at the other end to read a checklist. Until then
    /// its status is the honest "nothing has ever run here".
    #[gpui::test]
    async fn test_remote_flash_without_a_board_answers_with_the_reason(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, calls) = fake_streamer(vec![], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        panel.update_in(cx, |panel, window, cx| {
            let idle = panel.remote_flash_status();
            assert!(!idle.active);
            assert_eq!(
                (idle.phase, idle.verdict, idle.diag_run_id, idle.perf_run_id),
                (None, None, None, None),
                "nothing has flashed in this panel yet"
            );
            // No project, no repo, no board -- the same gap the button
            // draws as a checklist.
            panel.root_override = None;
            let err = panel
                .remote_flash(
                    hardware::FlashConfig { world: Some("worlds/arena".to_string()), ..Default::default() },
                    window,
                    cx,
                )
                .expect_err("a machine with no board cannot flash");
            assert!(err.starts_with("flashing needs a board"), "{err}");
            assert!(!panel.is_flashing());
        });
        cx.run_until_parked();
        assert!(calls.lock().unwrap().is_empty(), "nothing was spawned");
    }

    /// A remote flash while one is in flight is REFUSED, not treated as
    /// the button's cancel: an agent that lost track of the board must
    /// not kill a half-written flash by asking for another.
    #[gpui::test]
    async fn test_remote_flash_refuses_while_one_is_running(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board"], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, window, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
            let live = panel.remote_flash_status();
            assert!(live.active && live.verdict.is_none(), "the run is in flight");
            let err = panel
                .remote_flash(hardware::FlashConfig::default(), window, cx)
                .expect_err("one board, one flash");
            assert_eq!(err, "a flash is already running");
            assert!(panel.is_flashing(), "the running flash was left alone");
        });
    }

    /// The agent's cancel is the button's cancel: the timeline is kept
    /// with its phase still running, and a second cancel finds nothing.
    #[gpui::test]
    async fn test_remote_flash_cancel_retires_the_run_without_failing_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board"], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, _window, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
            assert!(panel.remote_flash_status().active);
            assert!(panel.remote_flash_cancel(cx), "a live flash was cancelled");
            let status = panel.remote_flash_status();
            assert!(!status.active);
            assert_eq!(status.verdict, None, "cancelled is not failed");
            assert_eq!(panel.status.as_deref(), Some("flash cancelled"));
            assert!(!panel.remote_flash_cancel(cx), "nothing left to cancel");
        });
    }

    #[gpui::test]
    async fn test_remote_env_reports_the_probe(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            let env = panel.remote_env();
            // Asserted against the payload's own internal consistency,
            // never against this host: the probe reads the real PATH,
            // HOME and /dev, so a developer machine with the board
            // toolchain installed is as valid an answer as a bare one.
            assert_eq!(env.ready, env.missing.is_empty(), "{env:?}");
            for missing in &env.missing {
                assert!(
                    matches!(
                        missing.code.as_str(),
                        "project" | "repo" | "diag" | "emd" | "port" | "port_stuck"
                    ),
                    "{env:?}"
                );
                assert!(!missing.label.is_empty(), "{env:?}");
            }
            let blames_the_port = env
                .missing
                .iter()
                .any(|missing| missing.code == "port" || missing.code == "port_stuck");
            assert_eq!(env.ports.is_empty(), blames_the_port, "{env:?}");
        });
    }

    /// A report id is only ever reported beside the run it belongs to.
    ///
    /// The lookup that resolves it is detached and unbounded, so it can
    /// land whenever -- including after the next board run has taken the
    /// page. Rather than argue about that ordering, the id is stashed
    /// WITH its run and surfaced only when the payload is reporting that
    /// same run: every other arrangement reports `None`.
    #[gpui::test]
    async fn test_a_report_id_never_surfaces_beside_another_runs_timeline(
        cx: &mut TestAppContext,
    ) {
        /// A second flash, with a run id of its own.
        const LATER_RUN: &str = "20260901T090000Z-9999999999";
        let dir = tempfile::tempdir().unwrap();
        let (first, _calls) = fake_streamer(
            vec![
                "==> Report",
                "[db] run 20260831T120000Z-abc123def0: 2 uart lines, 2 frames -> diag.db",
                "RESULT: PASS",
            ],
            true,
        );
        let (panel, cx) = flashable_panel(cx, dir.path(), first);
        let request = || {
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready")
        };
        // No charts window: no run's own hop looks anything up, so the
        // stash is the test's to play the landing hops itself.
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request()],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            )
        });
        cx.run_until_parked();

        let (second, _calls) = fake_streamer(
            vec![
                "==> Report",
                "[db] run 20260901T090000Z-9999999999: 1 uart lines, 1 frames -> diag.db",
                "RESULT: PASS",
            ],
            true,
        );
        panel.update(cx, |panel, cx| {
            // Run A's hop, landing.
            panel.last_flash_perf_run = Some((FLASHED_RUN.to_string(), 9));
            assert_eq!(
                panel.remote_flash_status().perf_run_id,
                Some(9),
                "run A's report id, beside run A's timeline"
            );
            // A hop from a flash older still, landing late.
            panel.last_flash_perf_run = Some(("20260101T000000Z-0000000000".to_string(), 7));
            assert_eq!(
                panel.remote_flash_status().perf_run_id,
                None,
                "a report id for a run this page is not showing stays hidden"
            );

            // Run B takes the page while A's report id is still stashed:
            // the live payload names no run at all, so the number cannot
            // ride along with it.
            panel.last_flash_perf_run = Some((FLASHED_RUN.to_string(), 9));
            panel.proc_streamer = second;
            panel.start_board_run(
                vec![request()],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
            let live = panel.remote_flash_status();
            assert!(live.active && live.diag_run_id.is_none(), "run B is in flight");
            assert_eq!(
                live.perf_run_id, None,
                "a live run has no report yet, least of all the previous run's"
            );
        });
        // And once B retires with a run id of its own, A's stashed id is
        // still not B's -- the mismatch does not become permanent.
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let status = panel.remote_flash_status();
            assert_eq!(status.diag_run_id.as_deref(), Some(LATER_RUN));
            assert_eq!(
                status.perf_run_id, None,
                "run B's timeline never inherits run A's report"
            );
        });
    }

    /// What the agent socket polls: a finished flash's phase, verdict,
    /// ggo-diag run id, and the local report id its clone resolved to --
    /// the last of which is remembered by the hop, never re-derived on
    /// the UI thread.
    #[gpui::test]
    async fn test_remote_flash_status_reports_the_finished_run(cx: &mut TestAppContext) {
        // The clone and the lookup each block on their own tokio runtime.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let diag_db = dir.path().join("diag.db");
        let ide_db = dir.path().join("ggo_ide.db");
        seed_flashed_run(&diag_db);

        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (streamer, _calls) = fake_streamer(
            vec![
                "==> Boot verify (UART)",
                "==> Report",
                "[db] run 20260831T120000Z-abc123def0: 2 uart lines, 2 frames -> diag.db",
                "RESULT: PASS",
            ],
            true,
        );
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, window, cx| {
            panel.proc_streamer = streamer;
            panel.db_path_override = Some(ide_db.clone());
            panel.diag_db_path_override = Some(diag_db.clone());
            panel.flash_charts_window = Some(window.window_handle());
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
        });
        cx.run_until_parked();

        let local_id = diag_db::device_perf_run_id(&ide_db, FLASHED_RUN)
            .expect("our own database reads")
            .expect("the flash cloned its telemetry across");
        panel.read_with(cx, |panel, _| {
            let status = panel.remote_flash_status();
            assert!(!status.active, "the run ended");
            assert_eq!(
                status.phase.as_deref(),
                Some("Report"),
                "the phase it got to, still named after the run ends"
            );
            assert_eq!(status.verdict, Some(true));
            assert_eq!(status.diag_run_id.as_deref(), Some(FLASHED_RUN));
            assert_eq!(
                status.perf_run_id,
                Some(local_id),
                "the same report id the page opened"
            );
        });
    }

    /// The running context an agent polls for: what is being flashed,
    /// every phase with its state, the boot stage and budget the running
    /// phase is on, the console tail -- and, once it dies, why.
    #[gpui::test]
    async fn test_remote_flash_status_carries_the_runs_context(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        let (streamer, _calls) = fake_streamer(
            vec![
                "==> Flash board",
                "==> Boot verify (UART)",
                "  [boot] boot-rom alive — next: SD ready (10s budget)",
                "diag step 1: running",
                "boot stalled at boot-rom alive",
                "RESULT: FAIL",
            ],
            false,
        );
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.proc_streamer = streamer;
            panel.start_board_run(
                vec![request],
                "flashing worlds/arena".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let status = panel.remote_flash_status();
            assert!(!status.active);
            assert_eq!(status.what.as_deref(), Some("flashing worlds/arena"));
            assert_eq!(status.verdict, Some(false));
            assert_eq!(status.failure.as_deref(), Some("boot stalled at boot-rom alive"));
            assert!(status.elapsed_s.is_some());
            let states: Vec<(&str, &str)> = status
                .phases
                .iter()
                .map(|phase| (phase.title.as_str(), phase.state.as_str()))
                .collect();
            assert_eq!(
                states,
                [
                    ("Flash board", "done"),
                    ("Boot verify (UART)", "failed"),
                    ("Report", "pending"),
                ],
                "the skipped phases dropped out, the pending one stayed: {states:?}"
            );
            assert_eq!(
                status.phases[1].detail.as_deref(),
                Some("boot: boot-rom alive — next: SD ready (10s budget)"),
                "the boot stage and its budget ride on the phase that ran it"
            );
            assert_eq!(
                (status.diag_steps[0].index.as_str(), status.diag_steps[0].status.as_str()),
                ("1", "running")
            );
            assert!(
                status.console_tail.iter().any(|line| line == "RESULT: FAIL"),
                "{:?}",
                status.console_tail
            );
        });
    }

    /// The setup flow runs its steps in order and stops at the first
    /// failure.
    #[gpui::test]
    async fn test_setup_runs_its_steps_in_order(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, calls) = fake_streamer(vec!["installing"], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let env = hardware::HardwareEnv {
            cargo: true,
            git: true,
            clone_dest: dir.path().join(".ggo/ggo"),
            home: dir.path().to_path_buf(),
            ..Default::default()
        };
        let steps = hardware::setup_steps(&env);
        assert_eq!(steps.len(), 3, "clone + two installs");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                steps.into_iter().map(|s| s.request).collect(),
                "setting up".to_string(),
                hardware::FlashProgress::steps(Vec::new()),
                cx,
            )
        });
        cx.executor().run_until_parked();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "every step ran");
        assert_eq!(calls[0].bin, "git");
        assert_eq!(calls[1].bin, "cargo");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.status.as_deref(), Some("setting up: done"));
        });
    }

    /// A failing step stops the ones after it.
    #[gpui::test]
    async fn test_a_failed_setup_step_stops_the_rest(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, calls) = fake_streamer(vec!["error: no network"], false);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let env = hardware::HardwareEnv {
            cargo: true,
            git: true,
            clone_dest: dir.path().join(".ggo/ggo"),
            home: dir.path().to_path_buf(),
            ..Default::default()
        };
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                hardware::setup_steps(&env)
                    .into_iter()
                    .map(|s| s.request)
                    .collect(),
                "setting up".to_string(),
                hardware::FlashProgress::steps(Vec::new()),
                cx,
            )
        });
        cx.executor().run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "stopped at the first failure"
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.status_is_error);
            assert!(panel.status.as_deref().unwrap_or("").contains("no network"));
        });
    }

    /// The REGISTERED flasher -- the path the world panel's button takes.
    ///
    /// It runs inside `workspace.update`, and `flash_to_board` ->
    /// `refresh_root` reads that same entity, so anything but a deferred
    /// call is `cannot read Workspace while it is already being updated`.
    /// The routing test in `ggo_world_panel` installs a FAKE flasher and
    /// therefore never touches this; only driving the real registration
    /// does.
    #[gpui::test]
    async fn test_the_registered_flasher_does_not_double_lease_the_workspace(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::flash_to_board(workspace, None, false, window, cx)
        });
        assert!(claimed, "init registers a board flasher");
        // The deferred `flash_to_board` runs here; before the fix this
        // panicked instead.
        cx.run_until_parked();

        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<EmulatorItem>(cx)
                .next()
                .expect("the flasher opened the emulator tab")
                .read(cx)
                .panel()
                .clone()
        });
        panel.read_with(cx, |panel, _| {
            // No board on a CI machine, so nothing runs -- the point is
            // that it got far enough to say so.
            assert!(!panel.is_flashing());
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<hardware_item::HardwareSetupItem>(cx)
                    .count(),
                1,
                "a missing prerequisite opens the setup page, not a status line"
            );
        });
    }

    /// The world the world panel flashed is REMEMBERED, because the
    /// hardware page the flash opens has flash buttons of its own and
    /// they must reach the same world -- a page that fell back to the
    /// project's `default_world` is the divergence this feature ends.
    #[gpui::test]
    async fn test_the_registered_flasher_remembers_the_world_for_the_hardware_page(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;

        workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::flash_to_board(workspace, Some("worlds/arena"), false, window, cx)
        });
        cx.run_until_parked();

        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<EmulatorItem>(cx)
                .next()
                .expect("the flasher opened the emulator tab")
                .read(cx)
                .panel()
                .clone()
        });
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.flash_world(cx).as_deref(), Some("worlds/arena"));
            // The plan the hardware page's own buttons take -- they name
            // no world of their own, so this is the whole of what makes
            // them re-flash the same one.
            let (request, what, _progress) = panel
                .flash_plan(&ready_hardware(dir.path()), &hardware::FlashConfig::default(), cx)
                .expect("a ready machine flashes");
            assert!(
                request
                    .args
                    .windows(2)
                    .any(|pair| pair == ["--world".to_string(), "worlds/arena".to_string()]),
                "the page re-flashes the same world: {:?}",
                request.args
            );
            assert_eq!(
                what, "flashing worlds/arena",
                "and the timeline says which world is on its way"
            );
        });

        // A different project is a different `worlds/` tree: the stem must
        // not survive into it.
        let other = tempfile::tempdir().unwrap();
        panel.update(cx, |panel, cx| {
            panel.root_override = Some(other.path().to_path_buf());
            panel.refresh_root(cx);
            assert_eq!(
                panel.flash_world(cx),
                None,
                "a root change drops the world remembered from the old tree"
            );
            let (request, what, _progress) = panel
                .flash_plan(&ready_hardware(dir.path()), &hardware::FlashConfig::default(), cx)
                .expect("a ready machine flashes");
            assert!(
                !request.args.contains(&"--world".to_string()),
                "and the run falls back to the project's default_world: {:?}",
                request.args
            );
            assert_eq!(what, "flashing");
        });
    }

    /// A world open in the world panel is the world the user is working
    /// on, whether or not they pressed anything: the emulator's own flash
    /// surfaces fall back to it rather than silently booting
    /// `default_world` while the world panel's button an inch away names
    /// the open world.
    #[gpui::test]
    async fn test_the_flash_falls_back_to_the_world_panels_open_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_world_fixture(dir.path());
        let (workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            assert_eq!(panel.flash_world(cx), None, "nothing open, nothing to boot");
        });

        dirty_world_panel(&workspace, cx, dir.path());
        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.flash_world(cx).as_deref(),
                Some("worlds/main"),
                "the open document answers when this panel has been told nothing"
            );
            let (request, what, _progress) = panel
                .flash_plan(&ready_hardware(dir.path()), &hardware::FlashConfig::default(), cx)
                .expect("a ready machine flashes");
            assert!(
                request
                    .args
                    .windows(2)
                    .any(|pair| pair == ["--world".to_string(), "worlds/main".to_string()]),
                "{:?}",
                request.args
            );
            assert_eq!(what, "flashing worlds/main");

            // A world this panel WAS told about wins: it is the one the
            // user last aimed at hardware.
            panel.remember_flash_world("worlds/arena");
            assert_eq!(panel.flash_world(cx).as_deref(), Some("worlds/arena"));
        });
    }

    /// Cancelling drops the run future, which is what `kill_on_drop`
    /// hangs off: a streamer whose future is merely *abandoned* would
    /// leave `ggo-diag` holding the board's serial port.
    #[gpui::test]
    async fn test_cancelling_drops_the_run_future(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct DropGuard(Arc<AtomicUsize>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        let streamer: ggo_common::ProcStreamer = {
            let dropped = dropped.clone();
            Arc::new(move |_request, _on_line| {
                let guard = DropGuard(dropped.clone());
                Box::pin(async move {
                    // A real flash runs for minutes; this one never
                    // finishes, so the only way out is cancellation.
                    let _guard = guard;
                    std::future::pending::<()>().await;
                    unreachable!()
                })
            })
        };
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update(cx, |panel, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "the run is still in flight"
        );

        panel.update_in(cx, |panel, window, cx| panel.flash_to_board(window, cx));
        cx.run_until_parked();
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "cancelling dropped the run future -- that is what kills the child"
        );
    }
}
