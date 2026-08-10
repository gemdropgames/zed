//! GGO Charts panel: right-dock replacement for the Tauri IDE's Reports
//! page, reading perf runs straight out of `~/.ggo/ggo_ide.db` -- the SAME
//! database file, not a copy (see `loader`'s doc).
//!
//! Two views. The picker (C1) lists every run (cart/label/date), loaded
//! off-thread with a load-generation guard against a stale result -- the
//! same shape `ggo_world_panel`/`ggo_sprite_panel` use. Selecting a
//! run (C2) kicks off a second background pass for its samples and then
//! renders the same chart set ggo-ide's run-detail page shows for it, with
//! a hover readout over the nearest sample.
//!
//! The picker also carries the DEVICE-run history rail (R3): `ggo-diag`'s
//! `~/.ggo/diag.db` cloned into our own database and listed newest first,
//! with a per-run log viewer. Those are a different table in a different id
//! space from the perf runs above them and nothing converts between the two
//! -- see [`history`]'s module doc for that, and for why the rail clones
//! rather than reading `diag.db` live. The run detail gained the stored
//! guest-UART console (R3) and a Re-run entry that hands the run's cart back
//! to the emulator pane through `ggo_common`'s cart-runner registry.
//!
//! **Everything a selection needs is queried AND derived off-thread**, in
//! one background spawn per selection -- see [`detail`]'s module doc, which
//! is also where the guard that keeps it that way lives.
//!
//! The chart work splits three ways, so only the last of them needs a
//! window to test: `chart_set` decides WHICH charts a run gets (mirroring
//! `reports.rs::charts_section`), `chart_geom` decides WHERE everything
//! goes and emits a renderer-independent `ChartScene`, and `chart_paint`
//! turns that scene into gpui paint calls -- the same
//! `build_draw_list`/`paint_scene` split `ggo_world_panel` uses.
//!
//! Structural mirror of `ggo_world_panel`/`ggo_sprite_panel`: `Panel`
//! impl, `ToggleFocus`, `observe_new` registration into every new
//! workspace, and a `KeymapEventChannel` observer scaffold (no panel
//! keybinds exist yet -- see `bind_panel_keys`'s doc for why the observer
//! is still wired up now rather than added later).

mod chart_geom;
mod chart_paint;
mod chart_set;
mod detail;
mod history;
// `pub`: `ggo_emu_panel`'s ingest round-trip test reads its own writes back
// through THESE query functions (as a dev-dependency), which is what proves
// the two panels agree on `ggo_ide.db`'s schema rather than each having its
// own idea of it.
pub mod loader;
mod report;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Action, App, Bounds, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseMoveEvent, Pixels, Render, Styled, Task, WeakEntity, Window, actions, div, px,
    uniform_list,
};
use ui::Tooltip;
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use chart_geom::{ChartSpec, build_chart_scene};
use history::RunSummary;
use loader::RunListing;

actions!(
    ggo_charts,
    [
        /// Toggles focus on the GGO charts panel.
        ToggleFocus
    ]
);

const GGO_CHARTS_PANEL_KEY: &str = "GGOChartsPanel";

/// The panel's key-dispatch context identifier. No bindings are scoped to
/// it yet (see `bind_panel_keys`) -- chart interaction is pointer-only so
/// far -- but it exists so a later keyboard affordance (run navigation,
/// chart zoom) has a context to land in without touching this module's
/// `init`/`Render` wiring.
const KEY_CONTEXT: &str = "GgoChartsPanel";

/// Fixed default width until the panel grows real settings persistence
/// (same call `ggo_world_panel`/`ggo_sprite_panel` made at their own
/// skeleton stage).
const DEFAULT_WIDTH: Pixels = px(360.);

/// Height of one chart canvas, matching ggo-ide's `CHART_HEIGHT` (240).
const CHART_HEIGHT: Pixels = px(240.);

// `debug_selector` handles for the three run-detail sections that are not
// canvases. gpui records a selector's painted bounds only in debug builds
// (`div.rs:845-856` -- the release impl discards the closure unevaluated),
// which is what lets `test_the_detail_view_paints_its_sections_in_order`
// assert that a section was laid out, and where, without a way to read
// text back out of a painted frame.
const KPI_ROW_SELECTOR: &str = "ggo-charts-kpi-row";
const FAILURES_SELECTOR: &str = "ggo-charts-failures";
const PANICS_SELECTOR: &str = "ggo-charts-panics";
const CONSOLE_SELECTOR: &str = "ggo-charts-console";
const HISTORY_RAIL_SELECTOR: &str = "ggo-charts-history-rail";
const DEVICE_LOG_SELECTOR: &str = "ggo-charts-device-log";

/// Height of a log/console scroll region -- ggo-ide's `UART_HEIGHT` (220),
/// which is also what its history log viewer uses.
const LOG_HEIGHT: Pixels = px(220.);

/// Heading of the stored-run console. ggo-ide's `uart_section` heading
/// verbatim, so a reader who knows that page knows this pane.
const CONSOLE_TITLE: &str = "Console — guest UART";

/// Heading of a device run's log viewer. `run_log` is the pipeline's own
/// narration (see `history`'s doc), not guest UART, and the two must not
/// read as the same thing.
const DEVICE_LOG_TITLE: &str = "Log — ggo-diag pipeline";

/// A device run that cloned no `run_log` rows. Like `report::NO_UART` this
/// names a STATE, not a cause: a run can reach `runs` with no narration
/// because it died before its first log write, because only its `uart`
/// rows were populated, or because the clone caught it mid-run.
const NO_DEVICE_LOG: &str = "no log lines recorded for this run";

/// Re-run refused: the run carries no cart path to hand the emulator.
/// Only runs the emulator pane itself ingested do -- `ggo_emu_panel`
/// writes the rel path into `run.label`, while `ggo-emu`/`ggo-server`
/// captures arrive with none.
const NO_RERUN_PATH: &str = "no cart path recorded for this run, so it cannot be re-run from here";

/// Re-run refused: nothing in this build claims carts. `run_cart`'s
/// registry is empty when `ggo_emu_panel::init` never ran.
const NO_CART_RUNNER: &str = "no emulator pane is available to run this cart";

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as `ggo_world_panel::init`/`ggo_sprite_panel::init`:
    // `zed::reload_keymaps` clears and rebuilds ALL key bindings on every
    // keymap/settings change (including once at startup), and keymap
    // assets are upstream files this fork doesn't edit. Re-running
    // `bind_panel_keys` on `KeymapEventChannel` keeps the panel's
    // bindings alive across reloads -- required scaffolding for any
    // panel with keybinds even before it has any (C2 will add chart
    // interaction keys here).
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        // The weak handle is what the Re-run entry routes through -- see
        // `ChartsPanel::rerun_selected`. Taken here, the way
        // `ggo_emu_panel::init` takes its own, because a panel cannot
        // recover its workspace from a `Context<Self>`.
        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| ChartsPanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ChartsPanel>(window, cx);
        });
    })
    .detach();
}

/// No panel-specific keybinds exist yet: run selection is a click and
/// the chart readout follows the pointer, so nothing in this panel is
/// reachable only by key. Kept as its own fn (rather than inlined into `init`)
/// so it matches `ggo_world_panel`/`ggo_sprite_panel`'s shape
/// exactly: `init` calls it once at startup AND the `KeymapEventChannel`
/// observer calls it again on every reload.
fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([]);
}

// ------------------------------------------------------------- view state

enum LoadState {
    /// Nothing loaded yet -- before the panel's first activation.
    Empty,
    Loading,
    Ready(Vec<RunListing>),
    Error(String),
}

/// The selected run's own load, independent of the runs list's: picking a
/// run kicks off a second background pass for it.
enum DetailState {
    Loading,
    /// The finished view model, queried AND derived on the background
    /// thread -- see [`detail`]'s module doc for why the derivation moved
    /// there in F5.4 R3. Nothing in the update closure that receives this
    /// does any work beyond storing it.
    Ready(detail::Detail),
    Error(String),
}

/// A device run's `run_log`, loaded on the same background/generation
/// machinery a perf run's detail is.
enum DeviceLogState {
    Loading,
    Ready(Arc<Vec<String>>),
    Error(String),
}

/// The device-run history rail's own load.
enum HistoryState {
    /// Nothing loaded yet -- before the panel's first activation.
    Empty,
    Loading,
    Ready(history::History),
}

/// What the panel is showing, when it is showing a run rather than the
/// picker. The two kinds come from two DIFFERENT tables in two different
/// id spaces -- `run` (INTEGER `id`, this app's own perf captures) and
/// `runs` (TEXT `id`, device runs cloned out of `ggo-diag`'s `diag.db`) --
/// and nothing here ever converts one into the other (R1/R2's trap (3)).
enum Selection {
    Perf(RunListing),
    Device(RunSummary),
}

/// Which chart the cursor is over, and where inside it (canvas-local px).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Hover {
    chart: usize,
    x: f32,
    y: f32,
}

pub struct ChartsPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    /// The workspace this panel was added to, for the Re-run handoff --
    /// `None` in the unit tests that build a bare panel with no workspace
    /// at all, which is exactly when Re-run has nowhere to go anyway.
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass `~/.ggo` resolution and point straight at a
    /// fixture db (`ggo_world_panel::root_override`'s analog).
    db_path_override: Option<PathBuf>,
    /// Test hook for the OTHER file the panel reads -- `ggo-diag`'s own
    /// `~/.ggo/diag.db`, which the history rail clones out of. Separate
    /// from `db_path_override` because they are separate files owned by
    /// separate tools; see `history`'s module doc.
    diag_db_path_override: Option<PathBuf>,
    state: LoadState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    /// The run being shown, `None` for the picker view.
    selected: Option<Selection>,
    detail: Option<DetailState>,
    /// Set instead of `detail` when the selection is a device run.
    device_log: Option<DeviceLogState>,
    /// Why the last Re-run click went nowhere, if it did.
    rerun_note: Option<&'static str>,
    /// Separate from `load_generation`: a run selection and a runs-list
    /// refresh are independent loads, and a stale one of either must not
    /// clobber the other's result. Shared by BOTH selection kinds, which is
    /// what makes switching from a perf run to a device run (or back)
    /// discard the one being left.
    detail_generation: u64,
    _detail_task: Option<Task<()>>,
    history: HistoryState,
    history_generation: u64,
    _history_task: Option<Task<()>>,
    hover: Option<Hover>,
    /// Each chart canvas's last painted bounds, recorded in its prepaint
    /// (the only place an element learns where it landed) so a mouse-move
    /// on the wrapper can be converted to canvas-local coordinates. Shared
    /// via `Rc<RefCell<..>>` for exactly the reason
    /// `ggo_world_panel`'s `view` is: the prepaint closure is `'static`
    /// and cannot borrow `self`.
    chart_bounds: Rc<RefCell<Vec<Option<Bounds<Pixels>>>>>,
}

impl ChartsPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            db_path_override: None,
            diag_db_path_override: None,
            state: LoadState::Empty,
            load_generation: 0,
            _load_task: None,
            selected: None,
            detail: None,
            device_log: None,
            rerun_note: None,
            detail_generation: 0,
            _detail_task: None,
            history: HistoryState::Empty,
            history_generation: 0,
            _history_task: None,
            hover: None,
            chart_bounds: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn db_path(&self) -> Option<PathBuf> {
        self.db_path_override
            .clone()
            .or_else(ggo_common::default_db_path)
    }

    /// `ggo-diag`'s `~/.ggo/diag.db` -- a DIFFERENT file, owned by a
    /// different tool, which this panel only ever clones out of.
    fn diag_db_path(&self) -> Option<PathBuf> {
        self.diag_db_path_override
            .clone()
            .or_else(ggo_common::default_diag_db_path)
    }

    /// Select a perf run and kick off its load -- same off-thread shape and
    /// same load-generation staleness guard as [`Self::refresh_runs`], on
    /// its own generation counter.
    ///
    /// **The whole pass is off-thread**, queries and derivations both:
    /// `detail::load` is the single background call, and the closure below
    /// only stores what it hands back. That is F5.4 R3's inherited fix --
    /// C2/R2 built the charts and the report in this closure, i.e. on the
    /// UI thread, which R2's review measured at 327 ms for a 100,000-line
    /// UART. No second load path was invented for it: the four queries R2
    /// added still ride this same spawn and this same generation guard, and
    /// R3's console rides it too. `detail::no_build_here` is the tripwire
    /// that keeps it that way -- see that fn's doc.
    fn select_run(&mut self, run: RunListing, cx: &mut Context<Self>) {
        let Some(db_path) = self.db_path() else {
            self.detail = Some(DetailState::Error(
                "could not resolve a home directory".to_string(),
            ));
            cx.notify();
            return;
        };
        let run_id = run.id;
        self.begin_selection(Selection::Perf(run));
        let generation = self.detail_generation;
        self.detail = Some(DetailState::Loading);
        cx.notify();

        let load = cx.background_spawn(async move { detail::load(&db_path, run_id) });
        self._detail_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                let _no_build_here = detail::no_build_here();
                if this.detail_generation != generation {
                    return;
                }
                this.detail = Some(match result {
                    Ok(detail) => DetailState::Ready(detail),
                    Err(e) => DetailState::Error(e),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Select a DEVICE run (a `runs` row cloned out of `diag.db`) and load
    /// its pipeline log. Same background spawn, same generation counter as
    /// [`Self::select_run`] -- sharing the counter is what makes switching
    /// between the two kinds discard the load being left behind.
    fn select_device_run(&mut self, run: RunSummary, cx: &mut Context<Self>) {
        let Some(db_path) = self.db_path() else {
            self.device_log = Some(DeviceLogState::Error(
                "could not resolve a home directory".to_string(),
            ));
            cx.notify();
            return;
        };
        let run_id = run.id.clone();
        self.begin_selection(Selection::Device(run));
        let generation = self.detail_generation;
        self.device_log = Some(DeviceLogState::Loading);
        cx.notify();

        let load = cx.background_spawn(async move { history::log(&db_path, &run_id) });
        self._detail_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.detail_generation != generation {
                    return;
                }
                this.device_log = Some(match result {
                    Ok(lines) => DeviceLogState::Ready(Arc::new(lines)),
                    Err(e) => DeviceLogState::Error(e),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// The bookkeeping both selection paths share: adopt `next`, drop
    /// whatever the previous selection left behind, and bump the generation
    /// so an in-flight load for the run being replaced cannot land on top.
    fn begin_selection(&mut self, next: Selection) {
        self.selected = Some(next);
        self.detail = None;
        self.device_log = None;
        self.rerun_note = None;
        self.hover = None;
        self.chart_bounds.borrow_mut().clear();
        self.detail_generation += 1;
    }

    /// Show run `run_id`, looking its listing up off-thread first.
    ///
    /// The entry point `ggo_emu_panel` uses when a Re-run's perf ingest
    /// lands: that panel knows the `run_id` its ingest wrote and nothing
    /// else about the run, while [`Self::select_run`] needs the whole
    /// [`RunListing`] (the picker's own click already has one). The runs
    /// list is refreshed either way, so the row is there when the user
    /// hits Back -- and so a run id that has somehow gone missing lands on
    /// the picker rather than on an error.
    pub fn open_run(&mut self, run_id: i64, cx: &mut Context<Self>) {
        self.refresh_runs(cx);
        let Some(db_path) = self.db_path() else {
            return;
        };
        let load = cx.background_spawn(async move { loader::list_runs(&db_path) });
        cx.spawn(async move |this, cx| {
            let listing = load
                .await
                .ok()
                .and_then(|runs| runs.into_iter().find(|run| run.id == run_id));
            if let Some(run) = listing {
                this.update(cx, |this, cx| this.select_run(run, cx)).ok();
            }
        })
        .detach();
    }

    /// Read runs from `path` instead of `~/.ggo/ggo_ide.db`.
    ///
    /// Public only so `ggo_emu_panel`'s "Re-run hops to the charts panel"
    /// test can aim BOTH panels at one temporary database -- without it
    /// that test would either read the developer's real database or not be
    /// writable at all. Production code never calls it; the panel resolves
    /// its own path through [`ggo_common::default_db_path`].
    pub fn set_db_path_override(&mut self, path: PathBuf) {
        self.db_path_override = Some(path);
    }

    /// Back to the run picker.
    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        // Bump the generation so an in-flight load for the run being
        // dismissed can't land afterwards and re-show it.
        self.detail_generation += 1;
        self.selected = None;
        self.detail = None;
        self.device_log = None;
        self.rerun_note = None;
        self.hover = None;
        self.chart_bounds.borrow_mut().clear();
        cx.notify();
    }

    /// **Re-run**: hand the selected run's cart back to the emulator pane.
    ///
    /// The reverse hop (a finished emulator run opening HERE) ships from
    /// F5.2/S4 as a direct call, because `ggo_emu_panel` depends on this
    /// crate. This direction cannot be a direct call for exactly that
    /// reason, so it goes through `ggo_common`'s `CartRunner` registry --
    /// see that type's doc. The emulator pane's registered runner ends in
    /// its ORDINARY `EmuPanel::rerun`, the same entry S4's context menu
    /// uses; no second run path exists on either side.
    ///
    /// The cart's identity is the run's `label` column, which
    /// `ggo_emu_panel::finish_run` writes as the project-relative path it
    /// ran. A run ingested by `ggo-emu`/`ggo-server` (or by ggo-ide) has no
    /// label, and this says so rather than guessing: matching a perf-db
    /// cart NAME back onto a stored `.cart` is ggo-ide's
    /// `rerun::matches_stored`, which reaches into that app's own cart
    /// library and deliberately did not travel in R1.
    fn rerun_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rerun_note = None;
        let Some(Selection::Perf(run)) = &self.selected else {
            return;
        };
        let Some(rel) = run.label.clone().filter(|l| !l.is_empty()) else {
            self.rerun_note = Some(NO_RERUN_PATH);
            cx.notify();
            return;
        };
        let claimed = self
            .workspace
            .as_ref()
            .and_then(|workspace| {
                workspace
                    .update(cx, |workspace, cx| {
                        ggo_common::run_cart(workspace, &rel, window, cx)
                    })
                    .ok()
            })
            .unwrap_or(false);
        if !claimed {
            self.rerun_note = Some(NO_CART_RUNNER);
        }
        cx.notify();
    }

    /// The currently-assembled chart specs, or `&[]` in any other state.
    /// Test-only: `render_detail` matches on `self.detail` directly (it
    /// needs the other arms' messages anyway), so this exists purely so a
    /// test can assert the chart set and build scenes without a window.
    #[cfg(test)]
    fn chart_specs(&self) -> &[Arc<ChartSpec>] {
        match &self.detail {
            Some(DetailState::Ready(detail)) => &detail.charts,
            _ => &[],
        }
    }

    /// The assembled non-chart report, or `None` in any other state.
    /// Test-only, for the same reason [`Self::chart_specs`] is: the render
    /// path matches on `self.detail` directly.
    #[cfg(test)]
    fn report(&self) -> Option<&report::RunReport> {
        match &self.detail {
            Some(DetailState::Ready(detail)) => Some(&detail.report),
            _ => None,
        }
    }

    /// The selected run's stored console lines, `&[]` in any other state.
    #[cfg(test)]
    fn console(&self) -> &[String] {
        match &self.detail {
            Some(DetailState::Ready(detail)) => &detail.console,
            _ => &[],
        }
    }

    /// The scene chart `ix` paints at `size`, honoring the current hover
    /// -- the identical `build_chart_scene(spec, size, hover)` call
    /// `render_chart`'s prepaint closure makes (that closure is
    /// `'static` and clones its inputs, so it can't share this method
    /// directly; the arguments are what's shared).
    #[cfg(test)]
    fn scene_for(&self, ix: usize, size: (f32, f32)) -> Option<chart_geom::ChartScene> {
        let spec = self.chart_specs().get(ix)?;
        let hover = self.hover.filter(|h| h.chart == ix).map(|h| (h.x, h.y));
        Some(build_chart_scene(spec, size, hover))
    }

    /// Kick off the off-thread run listing. Runs on every panel
    /// activation (`set_active`), same trigger `ggo_world_panel::
    /// refresh_worlds` uses -- cheap enough (one `SELECT` against a
    /// small local db) to just re-run rather than cache indefinitely, so
    /// a run ingested by `ggo-emu`/`ggo-server` while the panel sits open
    /// shows up the next time it's focused.
    fn refresh_runs(&mut self, cx: &mut Context<Self>) {
        let Some(db_path) = self.db_path() else {
            self.state = LoadState::Error("could not resolve a home directory".to_string());
            cx.notify();
            return;
        };

        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = LoadState::Loading;
        cx.notify();

        let load = cx.background_spawn(async move { loader::list_runs(&db_path) });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    // Superseded by a later refresh (rapid re-activation)
                    // -- drop this stale result, same guard
                    // `ggo_world_panel::load_rel_path` uses.
                    return;
                }
                this.state = match result {
                    Ok(runs) => LoadState::Ready(runs),
                    Err(e) => LoadState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Clone whatever `~/.ggo/diag.db` has that we do not, then list the
    /// device runs. Its own generation counter, for the same reason
    /// [`Self::select_run`] has one: this refresh and the perf-run listing
    /// are independent loads on independent tables.
    ///
    /// Runs on every panel activation alongside [`Self::refresh_runs`]. The
    /// clone is idempotent (`diag_db::clone_runs` skips a run whose `runs`
    /// row already exists), so re-running it is a `SELECT id FROM runs`
    /// against a small local file, and a diag run that finished while the
    /// panel sat open shows up the next time it is focused.
    ///
    /// A panel with no resolvable home directory shows an EMPTY rail rather
    /// than an error: with no `~/.ggo` there is no `diag.db` either, which
    /// is the state `history::NO_DIAG_DB` already describes.
    fn refresh_history(&mut self, cx: &mut Context<Self>) {
        let (Some(db_path), Some(diag_db_path)) = (self.db_path(), self.diag_db_path()) else {
            self.history = HistoryState::Ready(history::History {
                runs: Vec::new(),
                note: Some(history::NO_DIAG_DB.to_string()),
            });
            cx.notify();
            return;
        };

        self.history_generation += 1;
        let generation = self.history_generation;
        self.history = HistoryState::Loading;
        cx.notify();

        let load = cx.background_spawn(async move {
            history::load(&diag_db_path, &db_path, history::HISTORY_LIMIT)
        });
        self._history_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.history_generation != generation {
                    return;
                }
                this.history = HistoryState::Ready(result);
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.selected {
            Some(Selection::Perf(_)) => self.render_detail(cx),
            Some(Selection::Device(run)) => self.render_device_detail(run, cx),
            None => self.render_picker(cx),
        }
    }

    /// The picker: this app's own perf runs, then the device-run history
    /// rail. One scroll region, because at the dock's width a side-by-side
    /// rail (ggo-ide's `history_section` layout) would leave neither column
    /// readable.
    fn render_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .id("ggo-charts-picker")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_3()
            .child(self.render_runs_section(cx))
            .child(self.render_history_rail(cx))
            .into_any_element()
    }

    fn render_runs_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.state {
            LoadState::Empty => Self::note("Select the panel to load runs").into_any_element(),
            LoadState::Loading => Self::note("Loading runs…").into_any_element(),
            LoadState::Error(e) => {
                Self::note(format!("Failed to load runs: {e}")).into_any_element()
            }
            LoadState::Ready(runs) if runs.is_empty() => {
                Self::note("No perf runs recorded yet").into_any_element()
            }
            LoadState::Ready(runs) => v_flex()
                .w_full()
                .children(runs.iter().enumerate().map(|(ix, run)| {
                    let run = run.clone();
                    h_flex()
                        .id(("ggo-charts-run", ix))
                        .w_full()
                        .justify_between()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .hover(|s| s.bg(cx.theme().colors().element_hover))
                        .cursor_pointer()
                        .child(Label::new(run.display_title()))
                        .child(Label::new(run.started_at.clone()).color(Color::Muted))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.select_run(run.clone(), cx);
                        }))
                }))
                .into_any_element(),
        };
        v_flex()
            .w_full()
            .gap_1()
            .child(Label::new("Perf runs").size(LabelSize::Small))
            .child(body)
            .into_any_element()
    }

    /// The device-run history rail: every run cloned out of `~/.ggo/diag.db`,
    /// newest first, capped at `history::HISTORY_LIMIT`.
    ///
    /// **Undifferentiated by run kind, deliberately.** The `runs` table has
    /// no run-kind column and cart vs full-system is a path convention at
    /// ingest time, so this rail groups nothing and labels nothing by kind
    /// -- it shows `started_at`, `state` and `verdict`, which are columns
    /// that exist. See `history`'s module doc.
    fn render_history_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.history {
            HistoryState::Empty => {
                vec![Self::note("Select the panel to load device runs").into_any_element()]
            }
            HistoryState::Loading => vec![Self::note("Loading device runs…").into_any_element()],
            HistoryState::Ready(history) => {
                let mut rows: Vec<AnyElement> = history
                    .runs
                    .iter()
                    .enumerate()
                    .map(|(ix, run)| {
                        let summary = run.clone();
                        v_flex()
                            .id(("ggo-charts-device-run", ix))
                            .w_full()
                            .px_2()
                            .py_1()
                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                            .cursor_pointer()
                            .child(Label::new(run.started_at.clone()).size(LabelSize::Small))
                            .child(
                                Label::new(format!(
                                    "{}  {}",
                                    run.state,
                                    run.verdict.as_deref().unwrap_or("—")
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.select_device_run(summary.clone(), cx);
                            }))
                            .into_any_element()
                    })
                    .collect();
                // The reason goes UNDER the rows, not instead of them: a
                // clone that failed against rows cloned earlier means "these
                // may be stale, here is why", which is worth both halves.
                if let Some(note) = &history.note {
                    rows.push(Self::note(note.clone()).into_any_element());
                }
                rows
            }
        };
        v_flex()
            .w_full()
            .gap_1()
            .debug_selector(|| HISTORY_RAIL_SELECTOR.to_string())
            .child(Label::new("Device runs").size(LabelSize::Small))
            .children(body)
            .into_any_element()
    }

    /// A device run: the same header shape a perf run gets, over its
    /// `ggo-diag` pipeline log. There are no charts, no KPIs and no
    /// diagnostic tables here -- a `runs` row carries none of the perf
    /// columns those are derived from.
    fn render_device_detail(&self, run: &RunSummary, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body = match &self.device_log {
            None | Some(DeviceLogState::Loading) => self.render_message("Loading log…", cx),
            Some(DeviceLogState::Error(e)) => {
                self.render_message(format!("Failed to load log: {e}"), cx)
            }
            Some(DeviceLogState::Ready(lines)) => v_flex()
                .id("ggo-charts-device-body")
                .size_full()
                .overflow_y_scroll()
                .p_2()
                .child(Self::render_log(
                    DEVICE_LOG_TITLE,
                    DEVICE_LOG_SELECTOR,
                    lines,
                    NO_DEVICE_LOG,
                    cx,
                ))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .child(
                v_flex()
                    .id("ggo-charts-device-header")
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                IconButton::new("ggo-charts-device-back", IconName::ArrowLeft)
                                    .tooltip(Tooltip::text("Back to runs"))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.clear_selection(cx);
                                    })),
                            )
                            .child(Label::new(run.started_at.clone()).size(LabelSize::Small)),
                    )
                    .child(
                        Label::new(format!(
                            "{} · {} · {}",
                            run.id,
                            run.state,
                            run.verdict.as_deref().unwrap_or("no verdict")
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// The selected run's view: a back row, then one titled canvas per
    /// chart in [`chart_set::build_charts`]'s order.
    fn render_detail(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let title = match &self.selected {
            Some(Selection::Perf(run)) => run.display_title(),
            _ => String::new(),
        };

        let body = match &self.detail {
            None | Some(DetailState::Loading) => self.render_message("Loading samples…", cx),
            Some(DetailState::Error(e)) => {
                self.render_message(format!("Failed to load samples: {e}"), cx)
            }
            Some(DetailState::Ready(detail)) => {
                let (charts, report) = (&detail.charts, &detail.report);
                self.chart_bounds.borrow_mut().resize(charts.len(), None);
                // ggo-ide's `detail_view` order, top to bottom: the two
                // diagnostic tables, the stored console, then the KPI row,
                // then the charts. The tables come FIRST there and here for
                // a reason worth not "tidying" away: the run most in need of
                // them is a cart that panicked before it ever reached
                // vsync_wait, which has no frames, no KPIs and no charts at
                // all -- burying the panic under the empty-charts message
                // would hide the only thing that run has to say. The
                // console sits with them, above the same branch, for
                // exactly the same reason.
                v_flex()
                    .id("ggo-charts-list")
                    .size_full()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_3()
                    .child(self.render_failures_table(&report.diagnostics, cx))
                    .child(self.render_panics_table(&report.diagnostics, cx))
                    .child(Self::render_log(
                        CONSOLE_TITLE,
                        CONSOLE_SELECTOR,
                        &detail.console,
                        report::NO_UART,
                        cx,
                    ))
                    .children(if charts.is_empty() {
                        // An explicit message, never a blank canvas -- and
                        // which message matters. A run that recorded no
                        // frames at all and a run whose only frame is the
                        // ignored frame 0 both land here, but only the
                        // first never reached vsync_wait; saying so about
                        // the second is simply false. `report.no_frames`
                        // is what distinguishes them (ggo-ide keeps the
                        // same two apart at `reports.rs:1015`).
                        //
                        // No KPI tiles either way -- every one of them
                        // would read 0 (or a 100% hit rate) for a run that
                        // measured nothing, and ggo-ide skips `kpi_row` on
                        // this same condition.
                        vec![
                            Label::new(report.no_frames.unwrap_or(report::NO_FRAMES_RECORDED))
                                .color(Color::Muted)
                                .into_any_element(),
                        ]
                    } else {
                        let mut out = vec![self.render_kpi_row(&report.tiles, cx)];
                        out.extend(
                            charts
                                .iter()
                                .enumerate()
                                .map(|(ix, spec)| self.render_chart(ix, spec, cx)),
                        );
                        out
                    })
                    .into_any_element()
            }
        };

        let config_line = match &self.detail {
            Some(DetailState::Ready(detail)) => detail.report.config_line.clone(),
            _ => None,
        };

        v_flex()
            .size_full()
            .child(
                v_flex()
                    .id("ggo-charts-detail-header")
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                IconButton::new("ggo-charts-back", IconName::ArrowLeft)
                                    .tooltip(Tooltip::text("Back to runs"))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.clear_selection(cx);
                                    })),
                            )
                            .child(self.render_rerun_button(cx))
                            .child(Label::new(title).size(LabelSize::Small))
                            .children(self.ignored_caption()),
                    )
                    .children(
                        self.rerun_note.map(|note| {
                            Label::new(note).size(LabelSize::XSmall).color(Color::Muted)
                        }),
                    )
                    // The run-config line (frame budget, wire-model
                    // constants, wire-wait tag) -- `kpi::run_config_line`
                    // verbatim, so an emulator run and a device run read
                    // exactly as they do on ggo-ide's page. It wraps
                    // rather than truncates: in a 360 px dock it is
                    // several lines, and the wire-wait tag it ends with
                    // ("calibrated"/"pessimistic 2x"/"idealized/pre-wait")
                    // is the part a reader most needs.
                    .children(
                        config_line.map(|line| {
                            Label::new(line).size(LabelSize::XSmall).color(Color::Muted)
                        }),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// The Re-run entry -- see [`Self::rerun_selected`] for where it goes.
    ///
    /// Disabled, with the reason in its tooltip, for a run with no cart
    /// path: an enabled button that reports a refusal only after the click
    /// is a worse affordance than one that says so up front. It stays
    /// visible either way so the entry is discoverable on the runs that
    /// cannot use it as well as the ones that can.
    fn render_rerun_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let rel = match &self.selected {
            Some(Selection::Perf(run)) => run.label.clone().filter(|l| !l.is_empty()),
            _ => None,
        };
        let tooltip = match &rel {
            Some(rel) => format!("Re-run {rel} in the emulator"),
            None => NO_RERUN_PATH.to_string(),
        };
        IconButton::new("ggo-charts-rerun", IconName::RotateCcw)
            .disabled(rel.is_none())
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(|this, _event, window, cx| {
                this.rerun_selected(window, cx);
            }))
            .into_any_element()
    }

    /// A titled, scrollable, monospace log region -- the stored run's guest
    /// UART and a device run's pipeline log, which differ in what they hold
    /// but not at all in how they are read.
    ///
    /// `uniform_list`, not a `Vec` of labels: a run's `uart` table is
    /// unbounded from this side. The fork's own emulator caps what it
    /// PERSISTS at `UART_LOG_CAP` (2000), but that is a producer-side
    /// promise -- `ggo-emu`, `ggo-server` and `ggo_fixture` all write this
    /// table too -- and the console must not fall over on a run that
    /// ignored it. `uniform_list` lays out only the rows actually on
    /// screen, so a 100,000-line run costs the same as a 10-line one.
    ///
    /// The emulator pane's LIVE console is deliberately not shared with
    /// this one. It reads a `UartLog` (an `Arc<Mutex<..>>` the emulator
    /// thread is still writing) rather than a finished `Vec`, it shows a
    /// fixed 100-line tail because it re-renders at 60 Hz, it carries a
    /// collapse toggle stored in that panel's state, and it is not
    /// monospace. Sharing would mean moving an element builder into
    /// `ggo_common` (which has no `ui` dependency today) and
    /// parameterising all four differences -- more coupling than the
    /// ~20 lines it would save, between two crates that already share the
    /// `uart` TABLE, which is the part that actually has to agree.
    fn render_log(
        title: &'static str,
        selector: &'static str,
        lines: &Arc<Vec<String>>,
        empty_state: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section = v_flex()
            .w_full()
            .gap_1()
            .debug_selector(|| selector.to_string())
            .child(Label::new(title).size(LabelSize::Small));
        if lines.is_empty() {
            return section.child(Self::note(empty_state)).into_any_element();
        }
        // Refcount bump, not a deep clone: `uniform_list`'s renderer is
        // `'static` and has to own what it reads, and this is the whole
        // run's log.
        let lines = lines.clone();
        section
            .child(
                uniform_list(selector, lines.len(), move |range, _window, cx| {
                    range
                        .map(|ix| {
                            Label::new(lines[ix].clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                        })
                        .collect::<Vec<_>>()
                })
                .h(LOG_HEIGHT)
                .w_full()
                .rounded_sm()
                .bg(cx.theme().colors().editor_background),
            )
            .into_any_element()
    }

    /// A muted caption -- an empty state, a reason, a hint.
    fn note(text: impl Into<SharedString>) -> Label {
        Label::new(text).size(LabelSize::XSmall).color(Color::Muted)
    }

    /// The KPI tile row above the plots -- ggo-ide's `kpi_row`, wrapped
    /// instead of `chunks(KPI_TILES_PER_ROW)`'d: that page picks a fixed
    /// tiles-per-row because it owns its window width, while this panel is
    /// a user-resizable dock, so the row reflows.
    fn render_kpi_row(&self, tiles: &[report::KpiTile], cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .flex_wrap()
            .gap_2()
            // Test-only handle (a no-op outside debug builds -- see
            // gpui's `div.rs:855`) so a render test can assert this row
            // was actually laid out, and where relative to the charts.
            .debug_selector(|| KPI_ROW_SELECTOR.to_string())
            .children(tiles.iter().map(|tile| {
                v_flex()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().element_background)
                    .child(
                        Label::new(tile.label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(tile.value.clone()).size(LabelSize::Small))
            }))
            .into_any_element()
    }

    /// ggo-ide's "Failed asset loads" table: one row per `(kind, path)`,
    /// in first-seen order, with how many times it appeared.
    fn render_failures_table(
        &self,
        diagnostics: &report::Diagnostics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = diagnostics.failures();
        Self::table(
            "Failed asset loads",
            FAILURES_SELECTOR,
            diagnostics.empty_state(rows.len()),
            cx,
        )
        .children(rows.iter().map(|f| {
            h_flex()
                .w_full()
                .gap_2()
                .justify_between()
                .child(Label::new(f.kind.clone()).size(LabelSize::Small))
                .child(
                    Label::new(f.path.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx),
                )
                .child(
                    Label::new(ggo_worldlib::charts::reports::fmt::with_thousands(f.count))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
        }))
        .into_any_element()
    }

    /// ggo-ide's "Panics" table: the frame tag (a dash for an untagged
    /// line from an older run) and the panic message.
    fn render_panics_table(
        &self,
        diagnostics: &report::Diagnostics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = diagnostics.panics();
        Self::table(
            "Panics",
            PANICS_SELECTOR,
            diagnostics.empty_state(rows.len()),
            cx,
        )
        .children(rows.iter().map(|p| {
            h_flex()
                .w_full()
                .gap_2()
                .items_start()
                .child(
                    Label::new(p.frame.map_or_else(|| "—".to_string(), |n| n.to_string()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(p.message.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    ),
                )
        }))
        .into_any_element()
    }

    /// A titled table section, already carrying its empty-state line when
    /// it has one. Shared by both diagnostic tables so the two agree on
    /// heading weight and spacing.
    fn table(
        title: &'static str,
        selector: &'static str,
        empty_state: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        v_flex()
            .w_full()
            .gap_1()
            .debug_selector(|| selector.to_string())
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .color(Color::Default),
            )
            .children(
                empty_state
                    .map(|line| Label::new(line).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .bg(cx.theme().colors().panel_background)
    }

    /// `"1 frame ignored"` when the default ignore filter dropped
    /// anything. ggo-ide surfaces the same fact through a chip editor
    /// that lets a user change the set; this panel has no editor yet, so
    /// without a caption a user just sees an x-axis that starts at 1 with
    /// no explanation. Read-only for now -- the editor is the follow-up.
    fn ignored_caption(&self) -> Option<Label> {
        let Some(DetailState::Ready(detail)) = &self.detail else {
            return None;
        };
        let n = detail.ignored;
        if n == 0 {
            return None;
        }
        let plural = if n == 1 { "frame" } else { "frames" };
        Some(
            Label::new(format!("{n} {plural} ignored"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
    }

    fn render_chart(
        &self,
        ix: usize,
        spec: &Arc<ChartSpec>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let title = SharedString::from(spec.title.clone());
        // Refcount bump, not a deep clone -- see `DetailState::Ready`.
        let spec = spec.clone();
        let hover = self.hover.filter(|h| h.chart == ix).map(|h| (h.x, h.y));
        let palette = chart_paint::Palette::from_theme(cx);
        let chart_bounds = self.chart_bounds.clone();

        let canvas = gpui::canvas(
            move |canvas_bounds, _window, _cx| {
                // Prepaint: record where this chart landed (for
                // hit-testing) and build its scene at the size it
                // actually got.
                if let Some(slot) = chart_bounds.borrow_mut().get_mut(ix) {
                    *slot = Some(canvas_bounds);
                }
                build_chart_scene(
                    &spec,
                    (
                        f32::from(canvas_bounds.size.width),
                        f32::from(canvas_bounds.size.height),
                    ),
                    hover,
                )
            },
            move |canvas_bounds, scene, window, cx| {
                chart_paint::paint_scene(&scene, &palette, canvas_bounds, window, cx)
            },
        )
        .size_full();

        v_flex()
            .w_full()
            .gap_1()
            .child(Label::new(title).size(LabelSize::Small))
            .child(
                div()
                    .id(("ggo-chart", ix))
                    .w_full()
                    .h(CHART_HEIGHT)
                    .rounded_sm()
                    .overflow_hidden()
                    .child(canvas)
                    .on_mouse_move(
                        cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                            this.hover_chart(ix, event.position, cx);
                        }),
                    )
                    // `on_mouse_move` is hitbox-gated, so it never fires
                    // once the cursor leaves this chart -- moving into the
                    // gap between charts, onto the header, or off the
                    // panel entirely would otherwise leave the crosshair
                    // and readout pinned where the cursor last was.
                    // `on_hover(false)` is the only signal for that.
                    .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                        if !*hovered {
                            this.clear_hover(ix, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    /// Window-space cursor -> canvas-local hover on chart `ix`. A move
    /// outside the recorded bounds clears the hover rather than pinning a
    /// stale readout.
    ///
    /// Every hover change `cx.notify()`s, which re-renders ALL of the
    /// run's charts, each rebuilding its scene from scratch
    /// (`accumulate`/`bins`/`envelope` over the full sample set). That is
    /// fine at the frame counts a normal capture produces and it is what
    /// keeps the render path stateless, but it is O(charts x frames) per
    /// mouse-move frame and would bite at ingest's 100,000-frame cap.
    /// The upgrade, if a real run ever makes this visible: memoize each
    /// chart's scene keyed by `(canvas size, hover)` so a hover move only
    /// rebuilds the chart actually under the cursor.
    fn hover_chart(&mut self, ix: usize, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let bounds = self.chart_bounds.borrow().get(ix).copied().flatten();
        let next = bounds.filter(|b| b.contains(&position)).map(|b| Hover {
            chart: ix,
            x: f32::from(position.x - b.origin.x),
            y: f32::from(position.y - b.origin.y),
        });
        if self.hover != next {
            self.hover = next;
            cx.notify();
        }
    }

    /// Drops the hover if (and only if) chart `ix` currently owns it --
    /// the guard matters because hover-end on the chart being left can
    /// arrive AFTER hover-start on the chart being entered, which would
    /// otherwise clear the new chart's fresh readout.
    fn clear_hover(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.hover.is_some_and(|h| h.chart == ix) {
            self.hover = None;
            cx.notify();
        }
    }

    fn render_message(
        &self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message.into()).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }
}

impl Render for ChartsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}

impl Focusable for ChartsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ChartsPanel {}

impl Panel for ChartsPanel {
    fn persistent_name() -> &'static str {
        "GGO Charts"
    }

    fn panel_key() -> &'static str {
        GGO_CHARTS_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call as `ggo_world_panel`/`ggo_sprite_panel`: no
        // settings persistence yet, and Bottom isn't a sensible spot for
        // a charts sidebar.
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
        // `IconName` has no dedicated Chart/Graph/BarChart glyph in this
        // fork (checked against crates/icons/src/icons.rs). `SignalHigh`
        // is chosen over `GitGraph` (the other chart-ish candidate):
        // `assets/icons/signal_high.svg` draws four ascending vertical
        // bars -- literally a small bar chart -- while `git_graph.svg` is
        // a commit-graph glyph (nodes + branch lines), which reads as
        // version control, not perf data.
        Some(IconName::SignalHigh)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO Charts")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8, `ggo_sprite_panel` took 9 (grep
        // activation_priority across crates/).
        10
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred for the same reason `ggo_world_panel::set_active`
            // defers its own refresh: `set_active` fires inside the
            // workspace's own update (dock toggle), and a re-entrant read
            // isn't needed here today (this loader doesn't touch the
            // workspace/project at all) -- kept deferred anyway so this
            // stays structurally identical to the two panels it mirrors,
            // in case a later task adds a project-relative read.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| {
                    this.refresh_runs(cx);
                    this.refresh_history(cx);
                })
                .ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::dock::DockPosition;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock and focuses the
    /// panel. Goes through `MultiWorkspace::test_new` rather than a bare
    /// `Workspace::test_new` -- `register_action` handlers (like
    /// `ToggleFocus`) are only mounted into the dispatch tree once
    /// something renders `Workspace::actions`, which in production is
    /// `MultiWorkspace`'s render (`ggo_world_panel`'s test carries the
    /// same note).
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
                workspace.panel::<ChartsPanel>(cx).is_some(),
                "ChartsPanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<ChartsPanel>(cx)
                .expect("ChartsPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    /// A fixture db authored through `ggo_db::migrate` plus one inserted
    /// run (same pattern `ggo-ide`'s `backend/perf.rs` tests and
    /// `loader::tests::list_runs_reads_seeded_rows_newest_first` use),
    /// pointed at via `db_path_override` -- proves the panel reaches
    /// `Ready` off-thread with the fixture run listed, end to end through
    /// `refresh_runs`/the background load, not just through
    /// `loader::list_runs` directly. Calls `refresh_runs` directly rather
    /// than through `set_active` (which needs a live `Window`) -- the
    /// same shortcut `ggo_world_panel`'s `ready_panel` test helper takes
    /// with `refresh_worlds`/`load_rel_path`.
    #[gpui::test]
    async fn test_refresh_runs_loads_fixture_runs_into_ready(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&db_path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 3, 'arena')",
                (),
            )
            .await
            .unwrap();
        });

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(db_path.clone());
                panel
            })
        });

        panel.update(cx, |panel, cx| panel.refresh_runs(cx));
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| match &panel.state {
            LoadState::Ready(runs) => {
                assert_eq!(runs.len(), 1);
                assert_eq!(runs[0].id, 1);
                assert_eq!(runs[0].display_title(), "demo — arena");
            }
            LoadState::Empty => panic!("expected Ready, got Empty"),
            LoadState::Loading => panic!("expected Ready, got Loading"),
            LoadState::Error(e) => panic!("expected Ready, got Error({e})"),
        });
    }

    // ------------------------------------------------------ run detail

    /// A fixture db with one run whose frames exercise every chart gate,
    /// so `select_run` produces the full 13-chart set. `frames` is
    /// `1..=n` PLUS a frame 0 (the default-ignored one), matching real
    /// captures.
    fn seed_run_with_samples(db_path: &std::path::Path, frames: i64) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, frame_budget_cycles, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', ?1, 555549, 'arena')",
                [frames + 1],
            )
            .await
            .unwrap();
            for n in 0..=frames {
                conn.execute(
                    "INSERT INTO frame
                       (run_id, n, instrs, i_hits, i_misses, d_hits, d_misses,
                        scanout_wire, blit_wire, miss_wire, wire_total, over_budget,
                        sc_upload, sc_oam, bg_evictions, tile_load_wire,
                        apu_fetch_wire, bg_tiles_distinct, spr_tiles_distinct)
                     VALUES (1, ?1, ?2, 0, ?3, 0, ?4, 164400, 30, 40, ?5, 0,
                             5, 2, 1, 60, 8, 20, 12)",
                    (n, 1_000 + n * 10, 10 + n, 4 + n, 164_470 + n * 100),
                )
                .await
                .unwrap();
            }
            conn.execute(
                "INSERT INTO profile (run_id, frame, caller, func, misses, evicted)
                 VALUES (1, 2, '', 'update_entities', 12, 4)",
                (),
            )
            .await
            .unwrap();
        });
    }

    /// Drives a panel to `DetailState::Ready` for the seeded fixture run,
    /// going through `select_run`'s real off-thread load.
    async fn ready_detail_panel(
        cx: &mut TestAppContext,
        frames: i64,
    ) -> (tempfile::TempDir, gpui::Entity<ChartsPanel>) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, frames);

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(db_path.clone());
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        (dir, panel)
    }

    /// Selecting a run loads its samples off-thread and assembles the
    /// full reports-page chart set for it.
    #[gpui::test]
    async fn test_select_run_loads_samples_into_the_full_chart_set(cx: &mut TestAppContext) {
        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        panel.update(cx, |panel, _cx| {
            let titles: Vec<&str> = panel
                .chart_specs()
                .iter()
                .map(|c| c.title.as_str())
                .collect();
            assert_eq!(
                titles,
                vec![
                    "Wire cycles per frame vs budget",
                    "Wire breakdown per frame",
                    "Cache misses per frame",
                    "Syscalls per frame",
                    "Tile working set vs cache capacity",
                    "I$ misses by function",
                    "I$ eviction victims by function",
                    "i_misses distribution",
                    "d_misses distribution",
                    "PPU tile-cache evictions per frame",
                    "Tile-load wire per frame",
                    "APU fetch wire per frame",
                    "Instructions per frame",
                ]
            );
            // Frame 0 is ignored by default, so the axis starts at 1.
            assert_eq!(panel.chart_specs()[0].x, vec![1.0, 2.0, 3.0, 4.0]);
        });
    }

    /// Every chart kind (line, stacked, histogram) produces a non-empty
    /// paint description at a realistic canvas size -- the render-side
    /// assertion the brief asks for, made against the same
    /// `build_chart_scene` call `render_chart`'s prepaint makes.
    #[gpui::test]
    async fn test_every_chart_kind_builds_a_non_empty_scene(cx: &mut TestAppContext) {
        use chart_geom::{ChartKind, Primitive};

        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        panel.update(cx, |panel, _cx| {
            let mut seen_line = false;
            let mut seen_stacked = false;
            let mut seen_histogram = false;
            for (ix, spec) in panel.chart_specs().iter().enumerate() {
                let scene = panel
                    .scene_for(ix, (344.0, 240.0))
                    .expect("every index must have a spec");
                assert!(
                    !scene.primitives.is_empty(),
                    "{} produced no primitives",
                    spec.title
                );
                match spec.kind {
                    ChartKind::Line { .. } => {
                        seen_line = true;
                        assert!(
                            scene
                                .primitives
                                .iter()
                                .any(|p| matches!(p, Primitive::Polyline { .. })),
                            "{} should paint a series polyline",
                            spec.title
                        );
                    }
                    ChartKind::Stacked => {
                        seen_stacked = true;
                        assert!(
                            scene
                                .primitives
                                .iter()
                                .any(|p| matches!(p, Primitive::Polygon { .. })),
                            "{} should paint stacked bands",
                            spec.title
                        );
                    }
                    ChartKind::Histogram => {
                        seen_histogram = true;
                        assert!(
                            scene
                                .primitives
                                .iter()
                                .any(|p| matches!(p, Primitive::Quad { .. })),
                            "{} should paint bars",
                            spec.title
                        );
                    }
                }
            }
            assert!(seen_line && seen_stacked && seen_histogram);
        });
    }

    /// Hover at the pixel a known frame maps to -> that frame's readout.
    /// Drives `hover_chart` through the real bounds bookkeeping (a
    /// window-space cursor, chart bounds recorded as the prepaint would),
    /// not by poking `self.hover` directly.
    #[gpui::test]
    async fn test_hover_reads_out_the_sample_under_the_cursor(cx: &mut TestAppContext) {
        use chart_geom::{LEGEND_ROW_HEIGHT, LINE_MARGINS, full_x_domain, plot_rect, x_scale};
        use gpui::{point, size};

        let (_dir, panel) = ready_detail_panel(cx, 4).await;

        let canvas = (344.0f32, 240.0f32);
        // The chart canvas sits at a nonzero window offset, like a real
        // right-dock panel does -- so this also pins the window-space ->
        // canvas-local conversion, not just the geometry.
        let origin = point(px(600.), px(120.));
        panel.update(cx, |panel, _cx| {
            let mut slots = panel.chart_bounds.borrow_mut();
            slots.resize(panel.chart_specs().len(), None);
            slots[0] = Some(gpui::bounds(origin, size(px(canvas.0), px(canvas.1))));
        });

        // Chart 0 is the single-series wire chart, so its legend is one
        // row tall; frame 3 is the third plotted sample (frame 0 ignored).
        let plot = plot_rect(canvas, LINE_MARGINS, LEGEND_ROW_HEIGHT);
        let xs = x_scale(plot, full_x_domain(&[1.0, 2.0, 3.0, 4.0]));
        let local_x = xs.map(3.0);
        let local_y = plot.y + plot.h / 2.0;

        panel.update(cx, |panel, cx| {
            panel.hover_chart(0, point(origin.x + px(local_x), origin.y + px(local_y)), cx);
        });

        panel.update(cx, |panel, _cx| {
            let scene = panel.scene_for(0, canvas).expect("chart 0 exists");
            let readout = scene
                .readout
                .expect("a hover inside the plot must read out");
            assert_eq!(readout.index, 2, "frames 1,2,3,4 -> frame 3 is index 2");
            assert_eq!(readout.title, "frame 3");
            assert_eq!(readout.rows.len(), 1);
            assert_eq!(readout.rows[0].label, "wire_total");
            // Seeded as 164_470 + n * 100 for n = 3.
            assert_eq!(readout.rows[0].value, "164,770");
        });
    }

    /// A move outside the chart's bounds clears the hover instead of
    /// pinning a stale readout.
    #[gpui::test]
    async fn test_hover_outside_the_canvas_clears_the_readout(cx: &mut TestAppContext) {
        use gpui::{point, size};

        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        let origin = point(px(600.), px(120.));
        panel.update(cx, |panel, cx| {
            let len = panel.chart_specs().len();
            {
                let mut slots = panel.chart_bounds.borrow_mut();
                slots.resize(len, None);
                slots[0] = Some(gpui::bounds(origin, size(px(344.), px(240.))));
            }
            panel.hover_chart(0, point(px(700.), px(220.)), cx);
            assert!(panel.hover.is_some(), "a cursor inside the canvas hovers");
            panel.hover_chart(0, point(px(10.), px(10.)), cx);
            assert!(panel.hover.is_none(), "a cursor outside clears the hover");
        });
    }

    /// `on_mouse_move` is hitbox-gated, so leaving a chart (into the gap
    /// between charts, the header, or off the panel) fires no move event
    /// at all -- only the hover-end signal, which must drop the readout
    /// rather than leave the crosshair pinned. The cross-chart guard
    /// matters too: hover-end on the chart being LEFT can arrive after
    /// hover-start on the chart being ENTERED, and must not clear the
    /// new chart's fresh readout.
    #[gpui::test]
    async fn test_hover_end_clears_only_the_chart_that_owned_the_hover(cx: &mut TestAppContext) {
        use gpui::{point, size};

        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        let origin = point(px(600.), px(120.));
        panel.update(cx, |panel, cx| {
            let len = panel.chart_specs().len();
            {
                let mut slots = panel.chart_bounds.borrow_mut();
                slots.resize(len, None);
                slots[0] = Some(gpui::bounds(origin, size(px(344.), px(240.))));
            }
            panel.hover_chart(0, point(px(700.), px(220.)), cx);
            assert!(panel.hover.is_some());

            // Hover-end from a DIFFERENT chart must leave chart 0's
            // readout alone.
            panel.clear_hover(1, cx);
            assert!(
                panel.hover.is_some(),
                "another chart's hover-end must not clear this one's readout"
            );

            // Hover-end from the owning chart drops it.
            panel.clear_hover(0, cx);
            assert!(panel.hover.is_none());
        });
    }

    /// The header caption that explains why the x-axis starts at 1.
    #[gpui::test]
    async fn test_the_ignored_frame_count_is_captioned(cx: &mut TestAppContext) {
        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(&panel.detail, Some(DetailState::Ready(detail)) if detail.ignored == 1),
                "the fixture seeds frame 0, which the default filter drops"
            );
            assert!(
                panel.ignored_caption().is_some(),
                "a dropped frame must be captioned"
            );
        });
    }

    /// A run whose only frame is the default-ignored frame 0 reaches
    /// `Ready` with an EMPTY chart set -- the panel's explicit
    /// "no frames recorded" message, never a blank canvas.
    #[gpui::test]
    async fn test_a_run_with_no_plottable_frames_is_ready_but_empty(cx: &mut TestAppContext) {
        let (_dir, panel) = ready_detail_panel(cx, 0).await;
        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(&panel.detail, Some(DetailState::Ready(detail)) if detail.charts.is_empty()),
                "expected Ready with no charts"
            );
        });
    }

    // ------------------------------------------- KPI tiles and tables

    /// Seeds `run_id` 1's UART lines. Separate from
    /// [`seed_run_with_samples`] so a test can choose between the three
    /// states the failure tables distinguish: no UART at all, UART with
    /// nothing wrong in it, and UART with failures.
    fn seed_uart(db_path: &std::path::Path, lines: &[&str]) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            for (seq, text) in lines.iter().enumerate() {
                conn.execute(
                    "INSERT INTO uart (run_id, seq, text) VALUES (1, ?1, ?2)",
                    (seq as i64, *text),
                )
                .await
                .unwrap();
            }
        });
    }

    /// A `run` row with no `frame` rows at all -- the cart that never
    /// reached vsync_wait.
    fn seed_run_without_frames(db_path: &std::path::Path) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 0, 'arena')",
                (),
            )
            .await
            .unwrap();
        });
    }

    /// A run with only the always-on counters -- no PPU evictions, no
    /// tile working set, no sprite scanline peak, no VRAM uploads, no APU
    /// traffic. The fixture behind "a run that never measured those must
    /// render no tile for them".
    fn seed_run_without_ppu_counters(db_path: &std::path::Path) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, frame_budget_cycles,
                                  scanout_wire_cycles, refill_cycles, writeback_cycles,
                                  wire_wait_cycles, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 3, 555549, 164400, 100, 65, 10, 'arena')",
                (),
            )
            .await
            .unwrap();
            for n in 0..=2i64 {
                conn.execute(
                    "INSERT INTO frame
                       (run_id, n, instrs, i_hits, i_misses, d_hits, d_misses,
                        scanout_wire, blit_wire, miss_wire, wire_total, over_budget)
                     VALUES (1, ?1, 1000, 990, 10, 495, 5, 164400, 30, 40, 164470, 0)",
                    [n],
                )
                .await
                .unwrap();
            }
        });
    }

    /// Drives a panel to `Ready` for whatever the caller seeded at
    /// `db_path`, through `select_run`'s real off-thread load.
    async fn ready_panel_for(
        cx: &mut TestAppContext,
        db_path: PathBuf,
    ) -> gpui::Entity<ChartsPanel> {
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(db_path);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        panel
    }

    fn tile_value<'a>(report: &'a report::RunReport, label: &str) -> Option<&'a str> {
        report
            .tiles
            .iter()
            .find(|t| t.label == label)
            .map(|t| t.value.as_str())
    }

    /// End to end through the real db: the KPI tiles a selected run shows
    /// are derived from the IGNORE-FILTERED frames (R1's concern (1)).
    /// The fixture seeds frames 0..=4 with `i_misses = 10 + n` and
    /// `wire_total = 164_470 + n*100`, so dropping frame 0 is visible in
    /// three independent tiles at once: 4 frames not 5, max i_miss 14 not
    /// 14 (unchanged -- the max is in frame 4), and an I$ hit rate over
    /// frames 1..=4 only.
    #[gpui::test]
    async fn test_the_kpi_tiles_are_derived_from_the_ignore_filtered_frames(
        cx: &mut TestAppContext,
    ) {
        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        panel.update(cx, |panel, _cx| {
            let report = panel.report().expect("a loaded run has a report");
            assert_eq!(
                tile_value(report, "Frames"),
                Some("4"),
                "frame 0 is dropped, so 5 seeded frames report as 4"
            );
            // i_misses over the kept frames 1..=4 are 11,12,13,14 = 50;
            // including frame 0's 10 would make it 60. i_hits are 0
            // throughout, so the hit rate is 0.0% either way -- the
            // discriminating tile here is the max.
            assert_eq!(tile_value(report, "Max i_miss / frame"), Some("14"));
            assert_eq!(tile_value(report, "I$ hit rate"), Some("0.0%"));
            // wire_total 164,570/164,670/164,770/164,870 -> avg 164,720,
            // /555,549 = 29.6%. With frame 0 folded in it would be 29.6%
            // too at one decimal, so this is a sanity check, not the
            // discriminator.
            assert_eq!(tile_value(report, "Avg wire vs budget"), Some("29.6%"));
        });
    }

    /// The conditional tiles that ARE present for the full fixture -- it
    /// uploads 5 tiles a frame, so "VRAM uploads / frame" appears, while
    /// its working sets (20 and 12 distinct tiles) fit inside the 64-tile
    /// cache and its `peak_spr_line`/`apu_underruns` are never written, so
    /// those three do not.
    #[gpui::test]
    async fn test_only_the_measured_conditional_tiles_appear(cx: &mut TestAppContext) {
        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        panel.update(cx, |panel, _cx| {
            let report = panel.report().unwrap();
            assert_eq!(tile_value(report, "VRAM uploads / frame"), Some("5.0"));
            for absent in [
                "Peak sprites / scanline",
                "Sprite working set",
                "BG working set",
                "APU underruns",
            ] {
                assert_eq!(tile_value(report, absent), None, "{absent}");
            }
        });
    }

    /// A run that never measured PPU or APU counters renders NONE of the
    /// five conditional tiles -- not five tiles reading zero.
    #[gpui::test]
    async fn test_a_run_without_ppu_counters_renders_no_conditional_tiles(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_without_ppu_counters(&db_path);
        let panel = ready_panel_for(cx, db_path).await;

        panel.update(cx, |panel, _cx| {
            let report = panel.report().unwrap();
            assert_eq!(
                report.tiles.len(),
                8,
                "only the unconditional eight: {:?}",
                report.tiles.iter().map(|t| t.label).collect::<Vec<_>>()
            );
            // And the run's own charts agree: no PPU/APU/tile-working-set
            // chart either, through the same `gates` module.
            assert_eq!(
                panel
                    .chart_specs()
                    .iter()
                    .map(|c| c.title.as_str())
                    .collect::<Vec<_>>(),
                vec![
                    "Wire cycles per frame vs budget",
                    "Wire breakdown per frame",
                    "Cache misses per frame",
                    "i_misses distribution",
                    "d_misses distribution",
                    "Instructions per frame",
                ]
            );
        });
    }

    /// The run-config line, off the `run` row's own wire-model columns.
    #[gpui::test]
    async fn test_the_header_carries_the_run_config_line(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_without_ppu_counters(&db_path);
        let panel = ready_panel_for(cx, db_path).await;

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.report().unwrap().config_line.as_deref(),
                Some(
                    "frame budget 555,549 cyc (60 fps) · scanout 164,400 · refill 100 · \
                     writeback 65 · wire wait 10 (calibrated)"
                )
            );
        });
    }

    /// Both tables, rendering rows out of a seeded fixture db's UART.
    #[gpui::test]
    async fn test_the_failure_tables_render_rows_from_the_seeded_uart(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        seed_uart(
            &db_path,
            &[
                "== GGO OS booted ==",
                "asset: MISS \"grooble.til\"",
                "asset: MISS \"grooble.til\"",
                "gfx: inert tileset \"x.til\" (asset_load=0)",
                "f=3| panicked at 'boom', src/main.rs:1:1",
            ],
        );
        let panel = ready_panel_for(cx, db_path).await;

        panel.update(cx, |panel, _cx| {
            let diagnostics = &panel.report().unwrap().diagnostics;
            let failures = diagnostics.failures();
            assert_eq!(failures.len(), 2, "the two repeats collapse into one row");
            assert_eq!(failures[0].kind, "MISS");
            assert_eq!(failures[0].path, "grooble.til");
            assert_eq!(failures[0].count, 2);
            assert_eq!(failures[1].kind, "inert tileset");

            let panics = diagnostics.panics();
            assert_eq!(panics.len(), 1);
            assert_eq!(panics[0].frame, Some(3));

            assert_eq!(
                diagnostics.empty_state(failures.len()),
                None,
                "a table with rows shows no empty state"
            );
        });
    }

    /// The two empty states, which must NOT read the same. A run that
    /// recorded clean UART says "none recorded" -- a claim its data
    /// supports. A run with zero UART rows says only that no lines were
    /// captured, offering causes without asserting one.
    ///
    /// The `silent` half is deliberately seeded through the CURRENT
    /// schema, with a run written moments ago: that is exactly
    /// `ingest_run`'s documented "caller had no uart lines -- zero rows
    /// written, not an error" case, and it is why this state may not be
    /// reported as "the run predates UART capture". The fixture is the
    /// counter-example to that claim, so it is pinned here rather than
    /// left as a comment.
    #[gpui::test]
    async fn test_the_two_empty_states_are_distinct(cx: &mut TestAppContext) {
        let clean_dir = tempfile::tempdir().unwrap();
        let clean_path = clean_dir.path().join("ggo_ide.db");
        seed_run_with_samples(&clean_path, 4);
        seed_uart(&clean_path, &["== GGO OS booted ==", "wave 1 start"]);
        let clean = ready_panel_for(cx, clean_path).await;
        clean.update(cx, |panel, _cx| {
            let diagnostics = &panel.report().unwrap().diagnostics;
            assert!(diagnostics.failures().is_empty());
            assert!(diagnostics.panics().is_empty());
            assert_eq!(diagnostics.empty_state(0), Some(report::NONE_RECORDED));
        });

        // `seed_run_with_samples` writes a present-day run and no UART.
        let (_dir, silent) = ready_detail_panel(cx, 4).await;
        silent.update(cx, |panel, _cx| {
            let diagnostics = &panel.report().unwrap().diagnostics;
            assert_eq!(*diagnostics, report::Diagnostics::NoUart);
            assert_eq!(diagnostics.empty_state(0), Some(report::NO_UART));
            assert!(
                !report::NO_UART.contains("it predates"),
                "this fixture is a present-day run, so the message must not \
                 assert that the run is old: {}",
                report::NO_UART
            );
        });

        assert_ne!(report::NONE_RECORDED, report::NO_UART);
    }

    /// F4: the two reasons a run has no charts are different facts, and
    /// only one of them is "never reached vsync_wait". A run whose only
    /// frame is frame 0 reached it exactly once and then got filtered.
    #[gpui::test]
    async fn test_the_two_chartless_reasons_are_distinct(cx: &mut TestAppContext) {
        // `seed_run_with_samples(.., 0)` writes frame 0 and nothing else,
        // so the run HAS a frame and the ignore filter removes it.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 0);
        let only_frame_zero = ready_panel_for(cx, db_path).await;
        only_frame_zero.update(cx, |panel, _cx| {
            assert!(panel.chart_specs().is_empty());
            assert_eq!(
                panel.report().unwrap().no_frames,
                Some(report::ALL_FRAMES_IGNORED),
                "a run with a frame 0 did reach vsync_wait once"
            );
        });

        // A run row with no `frame` rows at all is the other case.
        let bare_dir = tempfile::tempdir().unwrap();
        let bare_path = bare_dir.path().join("ggo_ide.db");
        seed_run_without_frames(&bare_path);
        let never_sampled = ready_panel_for(cx, bare_path).await;
        never_sampled.update(cx, |panel, _cx| {
            assert_eq!(
                panel.report().unwrap().no_frames,
                Some(report::NO_FRAMES_RECORDED)
            );
        });

        assert_ne!(report::ALL_FRAMES_IGNORED, report::NO_FRAMES_RECORDED);
    }

    /// A run that panicked before it ever reached vsync_wait has no
    /// frames, so no KPI tiles and no charts -- but its panic table is
    /// exactly what a user opened the panel for, so it must still render.
    #[gpui::test]
    async fn test_a_run_with_no_frames_still_shows_its_panics(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 0);
        seed_uart(&db_path, &["panicked at 'early boom', src/main.rs:1:1"]);
        let panel = ready_panel_for(cx, db_path).await;

        panel.update(cx, |panel, _cx| {
            assert!(panel.chart_specs().is_empty(), "no plottable frames");
            let report = panel.report().unwrap();
            assert!(
                report.tiles.is_empty(),
                "no KPI tiles for a run that measured nothing"
            );
            assert_eq!(report.diagnostics.panics().len(), 1);
        });
    }

    /// F3: the restructured render path itself, not just the view model
    /// behind it. Every other test in this module asserts on
    /// `report::RunReport`, which would leave `render_detail`'s actual
    /// changes unexercised -- the header's `h_flex` -> `v_flex` with the
    /// config line as a second child, the two diagnostic tables hoisted
    /// ABOVE the charts-empty branch, and the `.children(if
    /// charts.is_empty())` split.
    ///
    /// gpui offers no way to read text back out of a painted frame, so
    /// this cannot assert wording (that is the view-model tests' job).
    /// What it can do is ask the window where each section was laid out,
    /// via the `debug_selector`/`debug_bounds` pair Zed's own tests use
    /// for this. That pins the two things the restructure is:
    ///
    /// * the tables are painted for BOTH a charted and a frameless run
    ///   (the hoist -- if a refactor moved them back inside the else-arm,
    ///   the frameless run would paint nothing but a message);
    /// * they are painted ABOVE the KPI row and the first chart canvas
    ///   (the order, which is ggo-ide's `detail_view` order).
    #[gpui::test]
    async fn test_the_detail_view_paints_its_sections_in_order(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });

        // A run with the full chart set, plus UART so both tables have
        // rows rather than an empty state.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        seed_uart(
            &db_path,
            &[
                "asset: MISS \"grooble.til\"",
                "f=3| panicked at 'boom', src/main.rs:1:1",
            ],
        );

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path);
            panel
        });
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(!panel.chart_specs().is_empty(), "the charted branch");
            assert!(panel.report().unwrap().config_line.is_some());
        });

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(2000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        let failures = cx
            .debug_bounds(FAILURES_SELECTOR)
            .expect("the failed-asset-loads table must be painted");
        let panics = cx
            .debug_bounds(PANICS_SELECTOR)
            .expect("the panics table must be painted");
        let console = cx
            .debug_bounds(CONSOLE_SELECTOR)
            .expect("the stored console must be painted");
        let kpis = cx
            .debug_bounds(KPI_ROW_SELECTOR)
            .expect("the KPI row must be painted for a run with frames");
        let first_chart = panel.update(cx, |panel, _cx| {
            panel.chart_bounds.borrow()[0].expect("chart 0 must have been laid out")
        });

        assert!(
            failures.origin.y < panics.origin.y,
            "failed asset loads sits above panics"
        );
        assert!(
            panics.origin.y < console.origin.y,
            "the stored console sits under the two tables -- ggo-ide's \
             detail_view order is loads, panics, console, KPIs, charts"
        );
        assert!(console.origin.y < kpis.origin.y, "and above the KPI row");
        assert!(
            kpis.origin.y < first_chart.origin.y,
            "the KPI row sits above the plots"
        );

        // And the other branch: a frameless run still paints both tables
        // (the whole point of hoisting them) but no KPI row.
        let bare_dir = tempfile::tempdir().unwrap();
        let bare_path = bare_dir.path().join("ggo_ide.db");
        seed_run_without_frames(&bare_path);
        seed_uart(&bare_path, &["panicked at 'early boom', src/main.rs:1:1"]);

        let (bare, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(bare_path);
            panel
        });
        bare.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: None,
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        bare.update(cx, |panel, _cx| {
            assert!(panel.chart_specs().is_empty(), "the frameless branch");
        });

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(800.)),
            |_window, _cx| bare.clone().into_any_element(),
        );

        assert!(
            cx.debug_bounds(FAILURES_SELECTOR).is_some(),
            "a frameless run must STILL paint its failure table -- this is \
             the regression the hoist exists to prevent"
        );
        assert!(
            cx.debug_bounds(PANICS_SELECTOR).is_some(),
            "a frameless run must still paint its panics table"
        );
        assert!(
            cx.debug_bounds(CONSOLE_SELECTOR).is_some(),
            "and its console -- the frameless run is exactly the one whose \
             last few UART lines say what went wrong"
        );
        assert!(
            cx.debug_bounds(KPI_ROW_SELECTOR).is_none(),
            "no KPI row for a run that measured nothing"
        );
    }

    /// R1's concern (5): every `perf_db` call blocks and spins its own
    /// tokio runtime, so the load must go through the panel's background
    /// spawn. Asserted by observing that `select_run` RETURNS with the
    /// panel still `Loading` -- if the query ran inline on the UI thread,
    /// the state would already be `Ready` by the time the update closure
    /// finished -- and that the result only lands once the executor is
    /// allowed to run. The load-generation guard is the same one
    /// `refresh_runs` uses; this task added no second one.
    #[gpui::test]
    async fn test_a_run_load_does_not_block_the_ui_thread(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(db_path.clone());
                panel
            })
        });
        let before = panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: None,
                },
                cx,
            );
            // Still inside the UI-thread update that started the load.
            matches!(panel.detail, Some(DetailState::Loading))
        });
        assert!(
            before,
            "select_run must hand back a Loading panel, not a finished query"
        );

        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.report().is_some(),
                "the background load lands once the executor runs"
            );
        });
    }

    /// Going back to the picker drops the selection and the hover, and
    /// bumps the detail generation so an in-flight load can't resurrect
    /// the dismissed run's charts.
    #[gpui::test]
    async fn test_clear_selection_returns_to_the_picker(cx: &mut TestAppContext) {
        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        panel.update(cx, |panel, cx| {
            let before = panel.detail_generation;
            panel.clear_selection(cx);
            assert!(panel.selected.is_none());
            assert!(panel.detail.is_none());
            assert!(panel.hover.is_none());
            assert!(panel.detail_generation > before);
        });
    }

    // ------------------------------------------ R3: the stored console

    /// The console shows the run's persisted UART, in `seq` order and
    /// verbatim -- the table `perf_db::run_uart` reads and that nothing in
    /// the fork rendered back until now.
    #[gpui::test]
    async fn test_the_stored_console_renders_the_runs_uart(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        seed_uart(
            &db_path,
            &[
                "== GGO OS booted ==",
                "[audio] output opened",
                "wave 1 start",
            ],
        );
        let panel = ready_panel_for(cx, db_path).await;

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.console(),
                [
                    "== GGO OS booted ==",
                    // R6 added the `[audio]` / `[audio unavailable]`
                    // markers to the run console, so they land in `uart`
                    // and therefore here too. Nothing filters them: this
                    // console is the run's log as recorded.
                    "[audio] output opened",
                    "wave 1 start",
                ]
            );
        });
    }

    /// A run with no UART at all gets the console's empty state, which is
    /// `report::NO_UART` -- the same hedged sentence the two diagnostic
    /// tables use, single-sourced rather than re-spelled here. Zero rows is
    /// a STATE (see `report::Diagnostics`' doc); this surface may not turn
    /// it into a cause.
    #[gpui::test]
    async fn test_a_run_with_no_stored_uart_gets_the_hedged_empty_state(cx: &mut TestAppContext) {
        let (_dir, panel) = ready_detail_panel(cx, 4).await;
        panel.update(cx, |panel, _cx| {
            assert!(panel.console().is_empty(), "the fixture seeds no UART");
            assert_eq!(
                panel.report().unwrap().diagnostics.empty_state(0),
                Some(report::NO_UART),
                "the console's empty state IS the tables', not a second copy"
            );
        });
        // And the device log's empty state is a different sentence about a
        // different table -- `run_log` is pipeline narration, not guest
        // UART, so borrowing NO_UART's wording for it would be wrong.
        assert_ne!(NO_DEVICE_LOG, report::NO_UART);
    }

    // ------------------------------ R3: the load is entirely off-thread

    /// Seed `n` UART lines for run 1, in batched multi-row inserts (one
    /// `execute` per line is minutes, not seconds, at these counts).
    fn seed_many_uart_lines(db_path: &std::path::Path, n: usize) {
        const BATCH: usize = 250;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            for chunk in (0..n).collect::<Vec<_>>().chunks(BATCH) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|seq| format!("(1, {seq}, 'line {seq}')"))
                    .collect();
                conn.execute(
                    &format!(
                        "INSERT INTO uart (run_id, seq, text) VALUES {}",
                        values.join(", ")
                    ),
                    (),
                )
                .await
                .unwrap();
            }
        });
    }

    /// **The inherited defect (R2's carried concern (1)), fixed and
    /// pinned.** `report::build` and `chart_set::build_charts` used to run
    /// in `select_run`'s UPDATE closure -- on the UI thread, including the
    /// UART parse, measured at 327 ms per 100,000 lines. They now run
    /// inside the same `cx.background_spawn` the queries always did.
    ///
    /// Two independent assertions, because neither alone is enough:
    ///
    /// * **Nothing happens inline.** `select_run` returns with the panel
    ///   `Loading`, and the panel PAINTS a real frame in that state, with a
    ///   6,000-line load outstanding -- i.e. the UI thread is free while it
    ///   runs. (6,000 is deliberately 3x `UART_LOG_CAP`: the producer's cap
    ///   is a promise this reader must not depend on.)
    /// * **Nothing is built afterwards either.** `select_run`'s closure
    ///   holds a `detail::no_build_here` guard, which panics if any
    ///   derivation runs while it is held. That guard is what actually
    ///   pins the fix -- gpui's test scheduler runs background and
    ///   foreground runnables on one thread, so no test here can tell them
    ///   apart by thread id. `detail::tests::the_tripwire_fires_when_
    ///   something_is_built_inside_the_guarded_scope` proves the guard
    ///   catches what it claims to.
    #[gpui::test]
    async fn test_a_large_uart_load_leaves_the_panel_responsive(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        seed_many_uart_lines(&db_path, 6_000);

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path);
            panel
        });
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: None,
                },
                cx,
            );
            assert!(
                matches!(panel.detail, Some(DetailState::Loading)),
                "select_run must hand back a Loading panel, not a finished load"
            );
        });

        // A whole frame, painted with the load still outstanding.
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(2000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.debug_bounds(CONSOLE_SELECTOR).is_none(),
            "nothing of the run is painted yet -- the load has not landed"
        );

        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.console().len(),
                6_000,
                "every stored line comes back -- the reader imposes no cap"
            );
            assert_eq!(panel.console()[5_999], "line 5999");
            assert!(!panel.chart_specs().is_empty(), "and the charts are built");
        });

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(2000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.debug_bounds(CONSOLE_SELECTOR).is_some(),
            "the console paints once the run has landed"
        );
    }

    // ------------------------------------------- R3: the history rail

    /// Seed `ggo-diag`'s OWN database (a separate file, per the
    /// no-shared-dbs rule `history`'s module doc explains) with one run and
    /// a couple of `run_log` lines.
    fn seed_device_run(diag_path: &std::path::Path, id: &str, started_at: &str, verdict: &str) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(diag_path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO runs (id, started_at, branch, commit_hash, git_describe, \
                 hostname, state, verdict) \
                 VALUES (?1, ?2, 'main', 'abc123', 'v1.2.3', 'test-host', 'done', ?3)",
                (id, started_at, verdict),
            )
            .await
            .unwrap();
            for (seq, text) in ["==> compile", "RESULT: PASS"].iter().enumerate() {
                conn.execute(
                    "INSERT INTO run_log (run_id, seq, text) VALUES (?1, ?2, ?3)",
                    (id, seq as i64, *text),
                )
                .await
                .unwrap();
            }
        });
    }

    /// A panel pointed at both files, with its history rail loaded.
    async fn history_panel(
        cx: &mut TestAppContext,
        db_path: PathBuf,
        diag_path: PathBuf,
    ) -> gpui::Entity<ChartsPanel> {
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(db_path);
                panel.diag_db_path_override = Some(diag_path);
                panel
            })
        });
        panel.update(cx, |panel, cx| panel.refresh_history(cx));
        cx.executor().run_until_parked();
        panel
    }

    fn history_of(panel: &ChartsPanel) -> &history::History {
        match &panel.history {
            HistoryState::Ready(history) => history,
            HistoryState::Empty => panic!("expected Ready, got Empty"),
            HistoryState::Loading => panic!("expected Ready, got Loading"),
        }
    }

    /// The rail lists the cloned device runs, newest first, off-thread.
    #[gpui::test]
    async fn test_the_history_rail_lists_seeded_runs_newest_first(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let diag_path = dir.path().join("diag.db");
        seed_device_run(&diag_path, "run-older", "2026-08-01T00:00:00Z", "PASS");
        seed_device_run(&diag_path, "run-newer", "2026-08-02T00:00:00Z", "FAIL");

        let panel = history_panel(cx, db_path, diag_path).await;
        panel.update(cx, |panel, _cx| {
            let history = history_of(panel);
            let ids: Vec<&str> = history.runs.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(ids, vec!["run-newer", "run-older"]);
            assert_eq!(history.runs[0].verdict.as_deref(), Some("FAIL"));
            assert_eq!(history.note, None, "a rail with rows carries no reason");
        });
    }

    /// Selecting a rail row swaps what the panel is showing -- from the
    /// picker (or from a perf run) to that device run and its pipeline log.
    /// The two selections are different id spaces and neither leaks into
    /// the other: the perf detail is dropped, not reinterpreted.
    #[gpui::test]
    async fn test_selecting_a_device_run_swaps_the_panels_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let diag_path = dir.path().join("diag.db");
        seed_run_with_samples(&db_path, 4);
        seed_device_run(&diag_path, "run-1", "2026-08-02T00:00:00Z", "PASS");

        let panel = history_panel(cx, db_path, diag_path).await;

        // Start on a PERF run, so the swap has something to displace.
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(!panel.chart_specs().is_empty());
        });

        let summary = panel.update(cx, |panel, _cx| history_of(panel).runs[0].clone());
        panel.update(cx, |panel, cx| panel.select_device_run(summary, cx));
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            match &panel.selected {
                Some(Selection::Device(run)) => assert_eq!(run.id, "run-1"),
                _ => panic!("the selection must now be the device run"),
            }
            assert!(
                panel.detail.is_none(),
                "the perf run's detail is dropped, not carried over"
            );
            match &panel.device_log {
                Some(DeviceLogState::Ready(lines)) => {
                    assert_eq!(**lines, vec!["==> compile", "RESULT: PASS"]);
                }
                _ => panic!("the device run's log must have loaded"),
            }
        });

        // ...and Back returns to the picker from this kind too.
        panel.update(cx, |panel, cx| panel.clear_selection(cx));
        panel.update(cx, |panel, _cx| {
            assert!(panel.selected.is_none());
            assert!(panel.device_log.is_none());
        });
    }

    /// **The fresh-machine case.** No `~/.ggo/diag.db` is the norm, and it
    /// must produce an empty rail with a legible reason -- never a panic,
    /// never a silent blank, and never a database file created as a side
    /// effect of a read (`turso::Builder::new_local` creates an empty,
    /// tableless file on open, which is why the `exists()` guards are in
    /// front of everything).
    #[gpui::test]
    async fn test_an_absent_diag_db_yields_the_reason_state_and_creates_no_file(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let diag_path = dir.path().join("diag.db");

        let panel = history_panel(cx, db_path.clone(), diag_path.clone()).await;
        panel.update(cx, |panel, _cx| {
            let history = history_of(panel);
            assert!(history.runs.is_empty());
            assert_eq!(history.note.as_deref(), Some(history::NO_DIAG_DB));
        });
        assert!(
            !diag_path.exists(),
            "a read must not create ggo-diag's file"
        );
        assert!(!db_path.exists(), "nor ours");
    }

    // ----------------------------------------------- R3: the Re-run hop

    /// Records what the charts panel handed to `ggo_common::run_cart`.
    /// The panel cannot name `EmuPanel` (that crate depends on THIS one),
    /// so what this side can assert is that the run's cart path reaches the
    /// registry with the right value; `ggo_emu_panel`'s own
    /// `test_the_registered_cart_runner_runs_the_cart_in_the_pane` asserts
    /// what the registered runner then does to that panel's state.
    #[derive(Default)]
    struct Reran(Vec<String>);
    impl gpui::Global for Reran {}

    fn recording_cart_runner(
        _workspace: &mut Workspace,
        rel: &str,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        cx.default_global::<Reran>().0.push(rel.to_string());
        true
    }

    /// A panel inside a real workspace (which is where the Re-run entry
    /// gets its `WeakEntity<Workspace>` from), showing the seeded run.
    async fn rerun_panel<'a>(
        cx: &'a mut TestAppContext,
        label: Option<&str>,
    ) -> (
        tempfile::TempDir,
        gpui::Entity<ChartsPanel>,
        &'a mut gpui::VisualTestContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);

        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<ChartsPanel>(cx)
                .expect("init() adds the charts panel")
        });
        panel.update(cx, |panel, cx| {
            panel.db_path_override = Some(db_path);
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: label.map(str::to_string),
                },
                cx,
            );
        });
        cx.run_until_parked();
        (dir, panel, cx)
    }

    /// **Re-run routes to the emulator.** The run's `label` is the rel path
    /// `ggo_emu_panel::finish_run` recorded for it, and that is what
    /// reaches the cart-runner registry -- no name matching, no second run
    /// path.
    #[gpui::test]
    async fn test_rerun_hands_the_runs_cart_path_to_the_cart_runner(cx: &mut TestAppContext) {
        let (_dir, panel, cx) = rerun_panel(cx, Some("carts/green.cart")).await;
        cx.update(|_window, cx| ggo_common::register_cart_runner(cx, recording_cart_runner));

        panel.update_in(cx, |panel, window, cx| panel.rerun_selected(window, cx));

        assert_eq!(
            cx.update(|_window, cx| cx.default_global::<Reran>().0.clone()),
            vec!["carts/green.cart".to_string()],
        );
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.rerun_note, None, "a claimed Re-run reports nothing");
        });
    }

    /// A run with no cart path cannot be re-run, and says which of the two
    /// refusals it is rather than doing nothing. (`ggo-emu`/`ggo-server`
    /// captures arrive with no `label`; only the emulator pane's own runs
    /// carry one.)
    #[gpui::test]
    async fn test_rerun_is_refused_for_a_run_with_no_cart_path(cx: &mut TestAppContext) {
        let (_dir, panel, cx) = rerun_panel(cx, None).await;
        cx.update(|_window, cx| ggo_common::register_cart_runner(cx, recording_cart_runner));

        panel.update_in(cx, |panel, window, cx| panel.rerun_selected(window, cx));

        assert!(
            cx.update(|_window, cx| cx.default_global::<Reran>().0.is_empty()),
            "nothing may be handed to the runner"
        );
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.rerun_note, Some(NO_RERUN_PATH));
        });
        assert_ne!(NO_RERUN_PATH, NO_CART_RUNNER);
    }

    /// With nothing registered -- a build without the emulator pane --
    /// Re-run says so instead of silently doing nothing.
    #[gpui::test]
    async fn test_rerun_with_no_registered_runner_says_so(cx: &mut TestAppContext) {
        let (_dir, panel, cx) = rerun_panel(cx, Some("carts/green.cart")).await;
        panel.update_in(cx, |panel, window, cx| panel.rerun_selected(window, cx));
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.rerun_note, Some(NO_CART_RUNNER));
        });
    }
}
