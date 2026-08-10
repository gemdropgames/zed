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
//! Audio is explicitly out of scope for F3 (constraints.md) -- see
//! [`drive`]'s module doc for exactly what else is not ported.
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

mod drive;
mod ingest;
mod input;
mod stats;
mod uart;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyBinding, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, Pixels, Render, RenderImage,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, actions, div, img,
    px,
};
use project::ProjectPath;
use ui::Tooltip;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use drive::{Frame, Session};
use input::InputState;
use stats::RunStats;
use uart::UartLog;

actions!(
    ggo_emu,
    [
        /// Toggles focus on the GGO emulator panel.
        ToggleFocus,
        /// Runs the selected cart in the emulator pane.
        Run,
        /// Stops the running cart.
        Stop
    ]
);

const GGO_EMU_PANEL_KEY: &str = "GGOEmuPanel";

/// The panel's key-dispatch context. Everything the pane binds is scoped
/// to it, so the pad keys and the transport bindings are inert unless the
/// pane itself has focus -- typing `z` into an editor must never reach a
/// cart. See [`bind_panel_keys`].
const KEY_CONTEXT: &str = "GgoEmuPanel";

/// Fixed default width until the panel grows real settings persistence
/// (the same call the other three GGO panels made). Wide enough for the
/// 320px screen plus the dock's padding.
const DEFAULT_WIDTH: Pixels = px(360.);

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

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as the other GGO panels' `init`: `zed::reload_keymaps`
    // clears and rebuilds ALL key bindings on every keymap/settings
    // change (including once at startup), and keymap assets are upstream
    // files this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` is what keeps Run/Stop alive across reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    // Explorer-driven routing: clicking a `.cart` in the project panel
    // selects it HERE instead of opening a (binary, unreadable) editor tab.
    // This is the panel's only way in -- there is no in-panel cart picker.
    workspace::register_path_open_interceptor(cx, intercept_cart_open);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| EmuPanel::new(Some(weak_workspace), Some(window), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<EmuPanel>(window, cx);
        });
    })
    .detach();
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
    ggo_common::open_in_panel(
        workspace,
        window,
        cx,
        move |panel: &mut EmuPanel, window, cx| panel.open_rel_path(&rel, window, cx),
    )
}

/// Transport bindings only. The 18 pad keys are deliberately NOT
/// `KeyBinding`s: an action fires on press and has no release event, and
/// the pad mask is level-triggered (the cart asks "is A held right now"),
/// so they go through `on_key_down`/`on_key_up`/`on_modifiers_changed`
/// listeners on the focus-tracked root instead -- see [`EmuPanel::render`]
/// and [`input`].
///
/// `ctrl-alt-` rather than anything shorter, for two reasons. Every bare
/// letter is a pad key while the pane is focused, and a binding would
/// swallow the keystroke before the pad listener saw it; and shift is
/// SELECT, so any shift chord would latch a button as a side effect of
/// running the cart. `ToggleFocus` stays unbound, dispatched via
/// `Panel::toggle_action` / the command palette, exactly as the other
/// three GGO panels leave it.
fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-alt-r", Run, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-alt-s", Stop, Some(KEY_CONTEXT)),
    ]);
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
    Done(i64),
    Failed(String),
}

impl IngestStatus {
    fn label(&self) -> Option<String> {
        match self {
            IngestStatus::Idle => None,
            IngestStatus::NoFrames => Some("no frames recorded — nothing ingested".into()),
            IngestStatus::Uploading => Some("ingesting perf diagnostics…".into()),
            IngestStatus::Done(run_id) => Some(format!(
                "perf run #{run_id} ingested — see the GGO Charts panel"
            )),
            IngestStatus::Failed(e) => Some(format!("perf ingest failed: {e}")),
        }
    }
}

pub struct EmuPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
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
    /// The running emulator, if any. Dropping it signals the thread to
    /// stop (see [`Session::stop`]).
    session: Option<Session>,
    /// Pumps [`Frame`]s from the emulator thread onto the UI thread. Its
    /// completion (the emulator thread dropping its sender) is also how
    /// the panel learns a run ended on its own.
    _pump_task: Option<Task<()>>,
    /// Last run's exit/error line, shown under the transport.
    status: Option<String>,
    /// The cart-visible frame number of the last frame received -- the
    /// pane's "is it actually running" readout.
    frame: u32,
    /// Latched pad mask, published into the session on every change.
    input: InputState,

    /// fps / dropped frames / step cost for the current run.
    stats: RunStats,
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
}

impl EmuPanel {
    pub fn new(
        workspace: Option<WeakEntity<Workspace>>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
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
            for image in [
                this.previous_rendered_frame.take(),
                this.current_rendered_frame.take(),
                this.latest_frame.take(),
            ]
            .into_iter()
            .flatten()
            {
                cx.drop_image(image, None);
            }
        })
        .detach();

        Self {
            focus_handle,
            position: DockPosition::Right,
            workspace,
            root_override: None,
            db_path_override: None,
            project_root: None,
            selected: None,
            run_generation: 0,
            session: None,
            _pump_task: None,
            status: None,
            frame: 0,
            input: InputState::default(),
            stats: RunStats::default(),
            fps_window_started: Instant::now(),
            console: None,
            console_expanded: false,
            ingest_status: IngestStatus::Idle,
            latest_frame: None,
            current_rendered_frame: None,
            previous_rendered_frame: None,
            _focus_out,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree) -- the directory `run` joins the selected rel path onto.
    /// MUST NOT run while the workspace itself is mid-update (it reads the
    /// workspace entity); see the deferrals in `set_active` and in
    /// [`Self::open_rel_path`].
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
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
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(cart)) = (self.project_root.clone(), self.selected.clone()) else {
            self.status = Some("no cart selected".to_string());
            cx.notify();
            return;
        };
        self.stop(window, cx);
        // AFTER `stop`, not before: `stop` -> `finish_run` reads
        // `run_generation` to tag the run it is finishing (the OLD one),
        // so it must still see the pre-bump value. This is the new
        // current run from here on -- see `run_generation`'s doc.
        self.run_generation += 1;

        let (session, rx) = drive::start(root.join(&cart), cart);
        self.console = Some(session.uart().clone());
        self.session = Some(session);
        self.status = None;
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
        self.input.clear();
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
            let status = match &finished.perf {
                // ggo-ide's "no frames recorded (cart never reached
                // vsync)" guard, and the only case for a cart that failed
                // to load: writing a zero-frame run row would just be
                // noise in the charts picker.
                None => IngestStatus::NoFrames,
                Some(perf) if perf.frames == 0 => IngestStatus::NoFrames,
                Some(perf) => match db_path_override.or_else(ggo_common::default_db_path) {
                    None => IngestStatus::Failed("no HOME to resolve ~/.ggo".into()),
                    Some(db_path) => match ingest::ingest_run(
                        &db_path,
                        &perf.perf_json,
                        &finished.uart,
                        Some(&label),
                    ) {
                        Ok(run) => IngestStatus::Done(run.run_id),
                        Err(e) => IngestStatus::Failed(e),
                    },
                },
            };
            (finished.reason, status)
        });
        cx.spawn(async move |this, cx| {
            let (reason, status) = finish.await;
            this.update(cx, |this, cx| {
                if this.run_generation != generation {
                    // A later run has started (and possibly already
                    // ended) since this one was taken -- this completion
                    // is stale, so don't let it stomp the live run's
                    // status.
                    return;
                }
                this.status = Some(reason);
                this.ingest_status = status;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stop the run and blank the pane: signal the thread (which drops the
    /// core on its way out), release the pad, and hand every atlas tile
    /// the pane still owns back to the window -- no further render will
    /// come to retire them through the double buffer. The run's perf
    /// snapshot and console lines are collected off-thread by
    /// [`Self::finish_run`].
    fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    // ---------------------------------------------------------- render

    fn render_transport(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let running = self.is_running();
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
            .children(self.session.as_ref().map(|session| {
                Label::new(format!("{} · frame {}", session.cart, self.frame))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .into_any_element()
    }

    /// The pane itself: the framebuffer at whatever size the dock gives
    /// it, or a message. `w_full`/`h_full` on the img rather than a fixed
    /// 320x240 -- gpui scales the one image, which is what the standalone
    /// binary's `--scale` does with `scale_nearest`.
    fn render_screen(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(frame) = &self.latest_frame {
            return div()
                .size_full()
                .flex()
                .justify_center()
                .items_center()
                .bg(gpui::black())
                .child(img(frame.clone()).w_full().h_full())
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
    /// drops / step-time triple. Only shown once a run has produced a
    /// frame; an all-zero row before that is noise.
    fn render_stats(&self) -> Option<gpui::AnyElement> {
        (self.frame > 0 || self.stats.dropped > 0).then(|| {
            Label::new(self.stats.label())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
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
                .child(toggle)
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

        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &Run, window, cx| this.run(window, cx)))
            .on_action(cx.listener(|this, _: &Stop, window, cx| this.stop(window, cx)))
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
            .child(div().flex_1().min_h_0().child(self.render_screen(cx)))
            .children(self.render_stats())
            .children(self.status.as_ref().map(|status| {
                Label::new(status.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
            .children(self.ingest_status.label().map(|label| {
                Label::new(label).size(LabelSize::XSmall).color(
                    if matches!(self.ingest_status, IngestStatus::Failed(_)) {
                        Color::Error
                    } else {
                        Color::Muted
                    },
                )
            }))
            .children(self.render_console(cx))
    }
}

impl Focusable for EmuPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for EmuPanel {}

impl Panel for EmuPanel {
    fn persistent_name() -> &'static str {
        "GGO Emulator"
    }

    fn panel_key() -> &'static str {
        GGO_EMU_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call the other three GGO panels made: no settings
        // persistence yet, and Bottom would squash a 4:3 screen.
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        DEFAULT_WIDTH
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        // `PlayOutlined` over `PlayFilled`: dock icons in this fork are
        // outline glyphs (see `IconName::Debug`/`Terminal` usage), and
        // the filled play is already spoken for by this panel's own Run
        // button, where the weight difference reads as "the action" vs
        // "the panel". `DebugPause`/`Stop` are the other transport
        // glyphs available; neither names a panel.
        Some(IconName::PlayOutlined)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO Emulator")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8, `ggo_sprite_panel` 9,
        // `ggo_charts_panel` 10 (grep activation_priority across
        // crates/).
        11
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred for the same reason `ggo_world_panel::set_active`
            // defers its own refresh: `set_active` fires inside the
            // workspace's own update (dock toggle), and `refresh_root`
            // reads the project's worktrees -- a re-entrant read of the
            // entity currently being updated. Still worth doing with the
            // picker gone: the root can change under the panel (a folder
            // opened after the panel was first shown), and `run` needs it.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_root(cx)).ok();
            });
        }
    }
}

impl EmuPanel {
    /// A workspace-less panel in a real test window -- the shape
    /// `TestAppContext::add_window_view` wants. Tests that don't need a
    /// window call `Self::new(None, None, cx)` directly.
    #[cfg(test)]
    fn test_new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(None, Some(window), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext};
    use project::{FakeFs, Project, WorktreeId};
    use workspace::dock::DockPosition;
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
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock. Goes through
    /// `MultiWorkspace::test_new` rather than a bare `Workspace::test_new`
    /// -- `register_action` handlers are only mounted into the dispatch
    /// tree once something renders `Workspace::actions`, which in
    /// production is `MultiWorkspace`'s render (the other three GGO
    /// panels' tests carry the same note).
    #[gpui::test]
    async fn test_toggle_focus_opens_panel(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.panel::<EmuPanel>(cx).is_some(),
                "EmuPanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<EmuPanel>(cx)
                .expect("EmuPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
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
        let (session, rx) = drive::start("/definitely/not/here.cart".into(), "gone.cart".into());
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
        let (session_a, rx_a) =
            drive::start("/definitely/not/here.cart".into(), "run-a.cart".into());
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
            panel.session = Some(drive::start("/also/not/here.cart".into(), "run-b.cart".into()).0);
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
                matches!(panel.ingest_status, IngestStatus::Done(_)),
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
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<EmuPanel>(cx)
                .expect("init() adds the panel")
        });

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
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "routing must open the panel's dock even if it was closed"
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
                matches!(panel.ingest_status, IngestStatus::Done(_)),
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
        let mut waited = std::time::Duration::ZERO;
        let step = std::time::Duration::from_millis(10);
        while panel.read_with(cx, |panel, _| panel.latest_frame.is_none()) {
            assert!(
                waited < std::time::Duration::from_secs(10),
                "no frame reached the panel from the emulator thread"
            );
            std::thread::sleep(step);
            cx.run_until_parked();
            waited += step;
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

        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<EmuPanel>(cx)
                .expect("init() adds the emu panel")
        });
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
                0,
                "a claimed path must open no editor tab"
            );
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
                1,
                "an unclaimed path still opens the normal way"
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

        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<EmuPanel>(cx)
                .expect("init() adds the emu panel")
        });
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
                0,
                "a claimed path must open no editor tab, split or not"
            );
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
}
