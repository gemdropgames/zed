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
//! R4 made the plots interactive: clicking a point on one of the four
//! frame-selectable charts opens an inspect pane for that frame beneath
//! it, and the I$ profile table under the charts sorts by misses either
//! way. Both are [`inspect`]'s view models, derived with everything else
//! in the one background pass -- the panel picks and paints, and the
//! guard that keeps it that way now reaches event listeners too
//! ([`ChartsPanel::guarded_listener`]).
//!
//! R5 finished the interaction set: a **Historic overlay** of up to five
//! earlier runs of the same cart (grey, dimmer with age, and folded into
//! the y-scale so a taller prior run is visible rather than off-canvas)
//! and **click-drag x-zoom with a double-click reset** on the line charts.
//! Both are state the panel owns and hands down as a
//! [`chart_geom::ChartView`]; the overlay's series were derived in the same
//! background pass as everything else, so the toggle paints and never
//! fetches. The same pass is now also where every chart's scene is
//! memoized -- see [`CachedScene`].
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
mod charts_item;
mod detail;
mod history;
mod inspect;
// `pub`: `ggo_emu_panel`'s ingest round-trip test reads its own writes back
// through THESE query functions (as a dev-dependency), which is what proves
// the two panels agree on `ggo_ide.db`'s schema rather than each having its
// own idea of it.
pub mod loader;
mod report;

pub use charts_item::ChartsItem;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, FocusHandle, Focusable, IntoElement, MouseMoveEvent, Pixels, Render,
    Styled, Task, WeakEntity, Window, div, px, uniform_list,
};
use ui::prelude::*;
use ui::{Checkbox, Tooltip};
use workspace::Workspace;

use chart_geom::{ChartSpec, build_chart_scene};
use history::RunSummary;
use loader::RunListing;

/// The panel's key-dispatch context identifier. No bindings are scoped to
/// it yet (see `bind_panel_keys`) -- chart interaction is pointer-only so
/// far -- but it exists so a later keyboard affordance (run navigation,
/// chart zoom) has a context to land in without touching this module's
/// `init`/`Render` wiring.
const KEY_CONTEXT: &str = "GgoChartsPanel";

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
const HISTORY_RAIL_SELECTOR: &str = "ggo-charts-history-rail";
const PROFILE_TABLE_SELECTOR: &str = "ggo-charts-profile-table";
const FRAME_INSPECT_SELECTOR: &str = "ggo-charts-frame-inspect";
const PROFILE_SORT_SELECTOR: &str = "ggo-charts-profile-sort";
const HISTORIC_TOGGLE_SELECTOR: &str = "ggo-charts-historic-toggle";
// The four navigation/dismissal buttons, each wrapped in a selector-bearing
// div (the `PROFILE_SORT_SELECTOR` pattern) so a render test can click the
// button itself rather than reaching past it to the handler.
const BACK_BUTTON_SELECTOR: &str = "ggo-charts-back-button";
const DEVICE_BACK_SELECTOR: &str = "ggo-charts-device-back-button";
const RERUN_BUTTON_SELECTOR: &str = "ggo-charts-rerun-button";
const INSPECT_CLOSE_SELECTOR: &str = "ggo-charts-inspect-close-button";

/// The picker row for perf run at list index `ix` -- one selector per row,
/// so a test can aim a real click at a specific run.
fn run_row_selector(ix: usize) -> String {
    format!("ggo-charts-run-{ix}")
}

/// The history rail row for the device run at list index `ix`.
fn device_run_row_selector(ix: usize) -> String {
    format!("ggo-charts-device-run-{ix}")
}

/// The historic overlay has nothing to draw.
///
/// **Names a STATE and never a cause** -- the rule R2's blocker, R3's F2
/// and R4's F3 each landed on, and the reason this is a constant with a
/// test pinning it character for character rather than a sentence built at
/// the call site. The signal behind it is exactly "`load_prior_runs`
/// returned no runs", and that covers several different situations at
/// once: this really is the cart's first run; every other run of it was
/// ingested later and so carries a HIGHER id; the run has no `run` row to
/// take a `cart_id` from at all. The surface cannot tell those apart, so
/// it says what it knows -- there are none -- and claims nothing about
/// why. (`load_prior_runs`' doc is where "prior" is defined; a reader who
/// wants the rule goes there.)
const NO_PRIOR_RUNS: &str = "no prior runs of this cart to overlay";

/// `RunPage.tsx`'s label for a leaf that shares its caller's name: that
/// caller's own samples, as opposed to something inlined into it.
const SELF_LEAF_LABEL: &str = "<self>";

/// Height of a log/console scroll region -- ggo-ide's `UART_HEIGHT` (220),
/// which is also what its history log viewer uses.
const LOG_HEIGHT: Pixels = px(220.);

/// A device run that cloned no `run_log` rows. Like `report::NO_UART` this
/// names a STATE, not a cause: a run can reach `runs` with no narration
/// because it died before its first log write, because only its `uart`
/// rows were populated, or because the clone caught it before its first
/// line landed.
const NO_DEVICE_LOG: &str = "no log lines recorded for this run";

/// The two log surfaces this panel renders. Every string that identifies
/// one hangs off this enum rather than being passed to [`ChartsPanel::render_log`]
/// as a loose argument, and that is the whole point: an empty-state
/// sentence handed over as a literal at the call site is a sentence no test
/// can read. R2's review blocked a `NoUart` message that asserted a cause
/// the signal cannot support, and with literals at the call site the exact
/// same sentence could be reintroduced with the suite still green. Ask
/// [`LogKind::empty_state`] instead -- that is what `render_log` uses, so a
/// test and the renderer cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    /// A perf run's persisted guest UART (`uart`, via `perf_db::run_uart`).
    Console,
    /// A device run's `ggo-diag` pipeline narration (`run_log`).
    DeviceLog,
}

impl LogKind {
    /// ggo-ide's `uart_section` heading verbatim for the console, so a
    /// reader who knows that page knows this pane. The device log gets its
    /// own, because `run_log` is pipeline narration, not guest UART, and
    /// the two must not read as the same thing.
    fn title(self) -> &'static str {
        match self {
            Self::Console => "Console — guest UART",
            Self::DeviceLog => "Log — ggo-diag pipeline",
        }
    }

    fn selector(self) -> &'static str {
        match self {
            Self::Console => "ggo-charts-console",
            Self::DeviceLog => "ggo-charts-device-log",
        }
    }

    /// What the surface says instead of lines when it has none.
    ///
    /// The console's is `report::NO_UART` itself -- the same hedged
    /// sentence the two diagnostic tables print for the same zero-rows
    /// signal, single-sourced rather than re-spelled, because the console
    /// and those tables read the *same* `uart` rows and cannot honestly
    /// draw different conclusions from their absence.
    fn empty_state(self) -> &'static str {
        match self {
            Self::Console => report::NO_UART,
            Self::DeviceLog => NO_DEVICE_LOG,
        }
    }
}

/// Re-run refused: the run carries no cart path to hand the emulator.
/// Only runs the emulator pane itself ingested do -- `ggo_emu_panel`
/// writes the rel path into `run.label`, while `ggo-emu`/`ggo-server`
/// captures arrive with none.
const NO_RERUN_PATH: &str = "no cart path recorded for this run, so it cannot be re-run from here";

/// Re-run refused: nothing in this build claims carts. `run_cart`'s
/// registry is empty when `ggo_emu_panel::init` never ran.
const NO_CART_RUNNER: &str = "no emulator pane is available to run this cart";

/// Nothing to register: the reports tab opens from the emulator and
/// the keybindings live in the keymap assets.
pub fn init(_cx: &mut App) {}

/// Open (or focus) THE center-pane reports tab and run `f` against its
/// panel -- the reports view is a singleton item, so the emulator's
/// finished-run hop and any later entry land in the same tab, with the
/// center area's real screen space. Refreshes the runs/history lists on
/// every open (the job the dock's `set_active` used to do).
pub fn open_charts_item(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut ChartsPanel, &mut Window, &mut Context<ChartsPanel>),
) -> bool {
    // Opening a report is a HEAVY action: fold every center split into
    // one pane first, so the screen goes to the report.
    ggo_common::collapse_center_splits(workspace, window, cx);
    let existing = workspace.items_of_type::<ChartsItem>(cx).next();
    let item = match existing {
        Some(item) => {
            workspace.activate_item(&item, true, true, window, cx);
            item
        }
        None => {
            let weak = workspace.weak_handle();
            let item = cx.new(|cx| ChartsItem::new(weak, cx));
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            item
        }
    };
    let panel = item.read(cx).panel().clone();
    panel.update(cx, |panel, cx| {
        panel.refresh_runs(cx);
        panel.refresh_history(cx);
        f(panel, window, cx)
    });
    true
}

/// Close the Reports tab. With `run`, only if that is the perf run it
/// shows -- an agent closing "its" report must not take down the one
/// the user switched to. `Ok(false)` when there is no tab.
///
/// The pane is updated from inside the workspace lease, which is safe:
/// only `Workspace` is leased, and `Pane::remove_item` reaches the
/// workspace through emitted events and `window.defer`, never inline.
pub fn close_charts_item(
    workspace: &mut Workspace,
    run: Option<i64>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<bool, String> {
    let Some(item) = workspace.items_of_type::<ChartsItem>(cx).next() else {
        return Ok(false);
    };
    let showing = item.read(cx).panel().read(cx).selected_run_id();
    match (run, showing) {
        (Some(run), Some(showing)) if run != showing => {
            return Err(format!("the report tab shows run {showing}, not {run}"));
        }
        (Some(run), None) => {
            return Err(format!("the report tab is on the runs list, not run {run}"));
        }
        _ => {}
    }
    // `pane_for` reads an index filled by a deferred event, so a tab
    // opened in this same update is not in it yet; the panes are.
    let item_id = item.entity_id();
    let pane = workspace
        .pane_for(&item)
        .or_else(|| {
            workspace
                .panes()
                .iter()
                .find(|pane| pane.read(cx).items().any(|it| it.item_id() == item_id))
                .cloned()
        })
        .ok_or("report tab has no pane")?;
    pane.update(cx, |pane, cx| {
        pane.remove_item(item.entity_id(), false, true, window, cx)
    });
    Ok(true)
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

/// One chart's memoized scene, plus every input it was built from.
///
/// **Why this exists.** `render_chart`'s prepaint closure rebuilds a
/// chart's whole scene from the run's full sample set, and it runs once
/// per chart per painted frame -- so on every hover mouse-move it ran for
/// every chart on the page (up to 13 for a fully-gated run, 11 in the
/// fixture measured below), all but one of them for charts the cursor is
/// nowhere near, since `hover_chart` notifies and gpui re-renders the
/// whole panel. That was
/// pre-existing (C2) and honestly documented but never addressed; R5
/// measured it before touching it, because R5 both adds up to five overlay
/// series per chart and puts a gesture on the same mouse-move.
///
/// Measured against this tree, debug build, 11 charts, one painted frame.
/// Median of 3 passes of 5 reps after a warm-up; the memoized column is
/// averaged over WHICH chart the cursor is on, since the charts differ in
/// cost and picking one flatters or punishes the result:
///
/// | frames | Historic off | Historic on (5 prior runs) | memoized hover move |
/// |---|---|---|---|
/// | 3,000 | 2.41 ms | 5.37 ms | 0.54 ms |
/// | 100,000 | 53.5 ms | 128.6 ms | 11.8 ms |
///
/// (A 300-frame run measures ~0.6 ms and 0.086 ms, but this harness's
/// floor is around 0.6 ms, so that row says nothing useful and is left
/// out.) So the overlay roughly doubles a per-frame cost that was already
/// the panel's largest, and at ingest's 100,000-frame cap that cost was
/// past a frame budget before R5 existed.
///
/// The fix is not to make the build cheaper but to stop doing it for the
/// ten charts nothing changed about. A scene is a pure function of
/// `(spec, size, view)`, so caching it on exactly those three is sound by
/// construction, and a hover move becomes one rebuild plus ten clones of
/// an already-decimated scene -- 10x cheaper at both sizes above.
///
/// **Still over budget at the cap, though, and by design of what is left.**
/// 11.8 ms for one hover move in debug is at or past a 16.7 ms frame once
/// the rest of the frame is counted. The cache removed the ten scenes that
/// did not change; the one that did is a full `O(frames)` rebuild, and it
/// is essentially the whole residual -- cloning all eleven cached scenes
/// measures **0.050 ms** at 100,000 frames, 0.4% of it, because
/// `MAX_PLOT_POINTS` caps a painted polyline at 2048 points regardless of
/// run length. The rebuild does not get that cap until too late:
/// `plot_points` materialises all 100,000 points and `envelope` decimates
/// afterwards. Decimating by index before mapping is the next real win and
/// is deliberately not in R5's scope.
///
/// The spec is held as an `Arc` and compared with `ptr_eq` rather than by
/// value: comparing thirteen charts' worth of `Vec<f32>` per frame would
/// give back everything this saves, and holding the `Arc` is what makes
/// the pointer comparison safe -- a cached entry keeps its allocation
/// alive, so a later spec cannot land on the same address and read as
/// equal.
struct CachedScene {
    spec: Arc<ChartSpec>,
    size: (f32, f32),
    view: chart_geom::ChartView,
    scene: chart_geom::ChartScene,
}

/// A left button held down on chart `chart`, from canvas-local `from_x` to
/// wherever the cursor is now.
///
/// Only ever a PREVIEW: the zoom is committed on release, and it is
/// committed from the click event's own down/up positions rather than from
/// this. That independence is deliberate -- the preview is per-chart UI
/// state that several things can legitimately drop, while the event pair
/// is the gesture itself.
///
/// The cursor leaving the chart mid-drag does NOT drop it (gpui stops
/// delivering `on_mouse_move` past the hitbox, so the band simply stops
/// widening and holds its last edge); the release does, wherever it lands.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    chart: usize,
    from_x: f32,
    to_x: f32,
}

pub struct ChartsPanel {
    focus_handle: FocusHandle,
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
    /// `ggo-diag`'s consolidated log for the selected perf run, the file
    /// the header's Copy button hands out -- see `ggo_emu_remote::diag_log_path`.
    run_log_path: Option<PathBuf>,
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
    /// Whether the historic overlay is switched on -- `reports.rs`'s
    /// `historic_enabled`. OFF by default, as there: the prior runs are
    /// already loaded either way (see [`detail::load`]), so this is a
    /// display switch and nothing more, and grey ghosts appearing on a run
    /// nobody asked to compare would be the surprising default.
    historic_enabled: bool,
    /// Per-chart zoomed x-domain, keyed by chart INDEX. Like
    /// [`inspect::FrameInspect::chart`] that index means something only
    /// within one selection's chart set, so this is cleared alongside it
    /// (R4's concern (4)). Per chart rather than shared because that is
    /// what `line.rs` does -- each `canvas::Program` retains its own zoom
    /// -- and because a drag on one chart saying something about the chart
    /// six rows down would be startling.
    zoom: std::collections::HashMap<usize, (f32, f32)>,
    /// The drag in progress, if any -- at most one, since it takes a held
    /// button.
    drag: Option<Drag>,
    /// The frame the user last clicked, already grouped -- `RunPage.tsx`'s
    /// `selFrame`. `None` until a click lands, and dropped whenever the
    /// selection changes (a frame number means nothing across runs).
    frame_inspect: Option<inspect::FrameInspect>,
    /// Which way the I$ profile table's "misses" header sorts --
    /// `reports.rs`'s `profile_sort_ascending`. Descending (the biggest
    /// offender first) is the default, here as there. A direction only:
    /// both orders are already derived, so this picks between them and
    /// never sorts anything.
    profile_sort_ascending: bool,
    /// Each chart's last built scene, keyed by everything that scene was
    /// built from -- see [`CachedScene`]. Shared with the prepaint closure
    /// the same way (and for the same reason) `chart_bounds` is.
    scenes: Rc<RefCell<Vec<Option<CachedScene>>>>,
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
            run_log_path: None,
            detail_generation: 0,
            _detail_task: None,
            history: HistoryState::Empty,
            history_generation: 0,
            _history_task: None,
            hover: None,
            historic_enabled: false,
            zoom: std::collections::HashMap::new(),
            drag: None,
            frame_inspect: None,
            profile_sort_ascending: false,
            chart_bounds: Rc::new(RefCell::new(Vec::new())),
            scenes: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// `cx.listener`, with `detail`'s no-derivation guard held across the
    /// whole handler body.
    ///
    /// R3's review named arbitrary event listeners as the gap in that
    /// guard's reach -- it wrapped `select_run`'s update closure and the
    /// whole of `Render::render`, and a listener that re-derived would
    /// have sailed past both. R4 adds the first listeners that could
    /// plausibly want to (a click that opens a per-frame table, a header
    /// that reorders one), so the gap is closed at the seam every
    /// listener in this panel already goes through rather than at the two
    /// call sites that happen to need it today. Nothing outside this
    /// function calls `cx.listener` any more; a `guarded_listener`-shaped
    /// grep is what keeps that true.
    ///
    /// Free outside `cfg(test)`: the guard is a ZST whose `Drop` does not
    /// exist in a release build.
    ///
    /// Returns a `Box<dyn Fn>` rather than an `impl Fn`, which is the one
    /// non-obvious line here. An unboxed wrapper does not compile: under
    /// edition 2024's RPIT capture rules `Context::listener`'s own opaque
    /// type captures the borrow of `cx`, so a `use<E, H>` return that
    /// wraps it is `E0700`, and without `use<..>` the listener becomes
    /// unnameable outside the `update` that built it -- which the drill
    /// test needs, since it has to hold one and then call it. Boxing
    /// erases that lifetime, satisfies gpui's `impl Fn(..) + 'static`
    /// handler bounds unchanged, and costs one allocation per listener
    /// per render. Copying `Context::listener`'s body into this crate
    /// would also compile, and was rejected: an upstream internal
    /// hand-copied into `crates/ggo` is fork drift we would carry
    /// forever to save an allocation nobody can measure next to
    /// `build_chart_scene`.
    fn guarded_listener<E: ?Sized + 'static, H>(
        cx: &Context<Self>,
        handler: H,
    ) -> Box<dyn Fn(&E, &mut Window, &mut App) + 'static>
    where
        H: Fn(&mut Self, &E, &mut Window, &mut Context<Self>) + 'static,
    {
        Box::new(cx.listener(move |view, event: &E, window, cx| {
            let _no_build_here = detail::no_build_here();
            handler(view, event, window, cx);
        }))
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
        let started_at = run.started_at.clone();
        self.begin_selection(Selection::Perf(run));
        // ponytail: one read_dir of ~/.ggo/diag/logs on the UI thread per
        // selection; move it into the detail load if the directory grows
        // past a few thousand files.
        self.run_log_path = self
            .diag_db_path()
            .and_then(|db| Some(db.parent()?.join("diag").join("logs")))
            .and_then(|logs_dir| ggo_emu_remote::diag_log_path(&logs_dir, &started_at));
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
        self.run_log_path = None;
        self.hover = None;
        // A frame number and a chart index both mean something only
        // within one run's chart set, so neither survives a selection
        // change -- and R5's zoom is keyed by that same chart index over a
        // frame domain, so it goes too (R4's concern (4)). The sort
        // DIRECTION survives: it is a reading preference, not run state,
        // and ggo-ide keeps it across runs too. `historic_enabled` is the
        // same kind of preference, but ggo-ide resets it per run
        // (`SelectRun` sets `historic_enabled = false`) because the
        // overlay is about THIS run's history, so it is reset here too.
        self.frame_inspect = None;
        self.historic_enabled = false;
        self.zoom.clear();
        self.drag = None;
        self.chart_bounds.borrow_mut().clear();
        self.scenes.borrow_mut().clear();
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
    ///
    /// **Generation-guarded like every other load here**, which it was not
    /// when R3 first added the device-run selection alongside it. This
    /// lookup is detached and lands whenever it lands; without the guard,
    /// a user who picked a device run (or hit Back) while it was in flight
    /// had that choice silently replaced by a hop they had already moved
    /// past. Whoever bumped [`Self::detail_generation`] last wins, and the
    /// stale arrival is dropped -- the same rule `select_run`,
    /// `select_device_run` and `clear_selection` already follow through
    /// [`Self::begin_selection`].
    pub fn open_run(&mut self, run_id: i64, cx: &mut Context<Self>) {
        self.refresh_runs(cx);
        let Some(db_path) = self.db_path() else {
            return;
        };
        // Claimed NOW, on the UI thread, not when the lookup lands.
        self.detail_generation += 1;
        let generation = self.detail_generation;
        let load = cx.background_spawn(async move { loader::list_runs(&db_path) });
        cx.spawn(async move |this, cx| {
            let listing = load
                .await
                .ok()
                .and_then(|runs| runs.into_iter().find(|run| run.id == run_id));
            if let Some(run) = listing {
                this.update(cx, |this, cx| {
                    if this.detail_generation != generation {
                        return;
                    }
                    this.select_run(run, cx);
                })
                .ok();
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
        self.run_log_path = None;
        self.hover = None;
        self.frame_inspect = None;
        self.historic_enabled = false;
        self.zoom.clear();
        self.drag = None;
        self.chart_bounds.borrow_mut().clear();
        self.scenes.borrow_mut().clear();
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

    /// Everything chart `ix` is currently being viewed under: the hover if
    /// it owns it, its own zoom window, its own in-progress drag, and the
    /// Historic switch.
    ///
    /// The ONE place that value is assembled. `render_chart`'s prepaint
    /// paints through it and [`Self::select_frame`] hit-tests through it,
    /// so a zoomed chart cannot answer a click with a frame it did not
    /// draw under the cursor (R4's concern (1)).
    fn view_for(&self, ix: usize) -> chart_geom::ChartView {
        chart_geom::ChartView {
            hover: self.hover.filter(|h| h.chart == ix).map(|h| (h.x, h.y)),
            zoom: self.zoom.get(&ix).copied(),
            drag: self
                .drag
                .filter(|d| d.chart == ix)
                .map(|d| (d.from_x, d.to_x)),
            historic: self.historic_enabled,
        }
    }

    /// The scene chart `ix` paints at `size` -- the identical
    /// `build_chart_scene(spec, size, &view)` call `render_chart`'s
    /// prepaint closure makes (that closure is `'static` and clones its
    /// inputs, so it can't share this method directly; the arguments are
    /// what's shared, [`Self::view_for`] included).
    #[cfg(test)]
    fn scene_for(&self, ix: usize, size: (f32, f32)) -> Option<chart_geom::ChartScene> {
        let spec = self.chart_specs().get(ix)?;
        Some(build_chart_scene(spec, size, &self.view_for(ix)))
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
    /// row it already holds unchanged AND whose linked device perf run it
    /// has already cloned), so re-running it is a handful of `SELECT`s
    /// against a small local file, and a diag run that finished -- or that
    /// this build learned to copy more of -- while the panel sat open shows
    /// up the next time it is focused.
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
            LoadState::Error(e) => ggo_common::CopyableText::new(
                "ggo-charts-runs-error-copy",
                Self::runs_error_message(e),
            )
            .color(Color::Muted)
            .into_any_element(),
            LoadState::Ready(runs) if runs.is_empty() => {
                Self::note("No perf runs recorded yet").into_any_element()
            }
            LoadState::Ready(runs) => v_flex()
                .w_full()
                .children(runs.iter().enumerate().map(|(ix, run)| {
                    let run = run.clone();
                    h_flex()
                        .id(("ggo-charts-run", ix))
                        .debug_selector(move || run_row_selector(ix))
                        .w_full()
                        .justify_between()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .hover(|s| s.bg(cx.theme().colors().element_hover))
                        .cursor_pointer()
                        .child(Label::new(run.display_title()))
                        .child(Label::new(run.started_at.clone()).color(Color::Muted))
                        .on_click(Self::guarded_listener(
                            cx,
                            move |this, _event, _window, cx| {
                                this.select_run(run.clone(), cx);
                            },
                        ))
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
                            .debug_selector(move || device_run_row_selector(ix))
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
                            .on_click(Self::guarded_listener(
                                cx,
                                move |this, _event, _window, cx| {
                                    this.select_device_run(summary.clone(), cx);
                                },
                            ))
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
            Some(DeviceLogState::Error(e)) => self.render_load_error(
                "ggo-charts-log-error-copy",
                Self::device_log_error_message(e),
                cx,
            ),
            Some(DeviceLogState::Ready(lines)) => v_flex()
                .id("ggo-charts-device-body")
                .size_full()
                .overflow_y_scroll()
                .p_2()
                .child(Self::render_log(LogKind::DeviceLog, lines, cx))
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
                                div()
                                    .debug_selector(|| DEVICE_BACK_SELECTOR.to_string())
                                    .child(
                                        IconButton::new(
                                            "ggo-charts-device-back",
                                            IconName::ArrowLeft,
                                        )
                                        .tooltip(Tooltip::text("Back to runs"))
                                        .on_click(
                                            Self::guarded_listener(
                                                cx,
                                                |this, _event, _window, cx| {
                                                    this.clear_selection(cx);
                                                },
                                            ),
                                        ),
                                    ),
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
    /// chart in [`chart_set::build_charts`]'s order -- except for that
    /// order's first chart on a device run, which is hoisted to the top
    /// of the page (see the hero split below).
    fn render_detail(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let title = match &self.selected {
            Some(Selection::Perf(run)) => run.display_title(),
            _ => String::new(),
        };

        let body = match &self.detail {
            None | Some(DetailState::Loading) => self.render_message("Loading samples…", cx),
            Some(DetailState::Error(e)) => self.render_load_error(
                "ggo-charts-samples-error-copy",
                Self::samples_error_message(e),
                cx,
            ),
            Some(DetailState::Ready(detail)) => {
                let (charts, report) = (&detail.charts, &detail.report);
                self.chart_bounds.borrow_mut().resize(charts.len(), None);
                self.scenes.borrow_mut().resize_with(charts.len(), || None);
                // The report's HERO, and the one departure from ggo-ide's
                // `detail_view` order below: on a device run
                // `build_charts` leads with the measured-fps chart, and
                // that chart is the answer to the question the whole page
                // is opened to ask, so it is painted above everything --
                // including the tables. Matched on the title rather than
                // on the index because only the fps chart earns the slot:
                // a run that gets no fps chart (an emulator run, or a
                // device run with no budget on its `run` row) leads with
                // an ordinary chart, which belongs under the tables with
                // the rest of them.
                //
                // `HERO_IX` is 0 by construction -- `hero` IS
                // `charts[0]` -- and `rest` is therefore indexed from 1.
                // Those are indices into the ORIGINAL `charts` vec and
                // must stay that way: `chart_bounds` and `scenes` are
                // sized `charts.len()` and `render_chart` uses `ix` to
                // hit-test and cache with, so renumbering the split
                // halves would file a chart's bounds under its
                // neighbour's slot.
                const HERO_IX: usize = 0;
                let (hero, rest) = match charts.split_first() {
                    Some((first, rest)) if first.title == chart_set::FPS_CHART_TITLE => {
                        (Some(first), rest)
                    }
                    _ => (None, &charts[..]),
                };
                // ggo-ide's `detail_view` order for everything under the
                // hero, top to bottom: the two diagnostic tables, the
                // stored console, then the KPI row, then the charts. The
                // tables come FIRST there and here for
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
                    .children(hero.map(|spec| self.render_chart(HERO_IX, spec, cx)))
                    .child(self.render_failures_table(&report.diagnostics, cx))
                    .child(self.render_panics_table(&report.diagnostics, cx))
                    .child(Self::render_log(LogKind::Console, &detail.console, cx))
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
                        // Directly above the first of the `rest` charts
                        // -- `charts_section` pushes `historic_toggle_row`
                        // as its own first row for the same reason: it is
                        // a statement about every chart below it, not
                        // about any one of them. The hoisted hero is the
                        // one chart it does not sit above, and it says
                        // nothing about that one either: the fps chart
                        // carries no historic overlay to switch on (see
                        // `chart_set`, which attaches overlays only to the
                        // charts ggo-ide overlays).
                        out.push(self.render_historic_toggle(detail.prior_runs, cx));
                        // `rest`, still under its original indices: the
                        // hero (when there is one) took index 0, so what
                        // is left starts at 1.
                        let first_rest_ix = HERO_IX + usize::from(hero.is_some());
                        out.extend(
                            rest.iter()
                                .enumerate()
                                .map(|(ix, spec)| self.render_chart(first_rest_ix + ix, spec, cx)),
                        );
                        out
                    })
                    // Last, and for every run -- ggo-ide pushes
                    // `profile_section` outside the frames branch for the
                    // same reason the two diagnostic tables are hoisted
                    // above it: it has something to say (its own empty
                    // state) about a run with no frames at all.
                    .child(self.render_profile_table(&detail.profiles, cx))
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
                                div()
                                    .debug_selector(|| BACK_BUTTON_SELECTOR.to_string())
                                    .child(
                                        IconButton::new("ggo-charts-back", IconName::ArrowLeft)
                                            .tooltip(Tooltip::text("Back to runs"))
                                            .on_click(Self::guarded_listener(
                                                cx,
                                                |this, _event, _window, cx| {
                                                    this.clear_selection(cx);
                                                },
                                            )),
                                    ),
                            )
                            .child(self.render_rerun_button(cx))
                            .child(self.render_copy_log_button())
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

    /// The perf run on screen, if the panel is showing one.
    pub fn selected_run_id(&self) -> Option<i64> {
        match &self.selected {
            Some(Selection::Perf(run)) => Some(run.id),
            _ => None,
        }
    }

    /// Copy the run's `ggo-diag` log path, for pasting into an agent's
    /// prompt. Disabled, with the reason in its tooltip, for a run with no
    /// such log -- an emulator run, or one whose log was pruned.
    fn render_copy_log_button(&self) -> gpui::AnyElement {
        let button = IconButton::new("ggo-charts-copy-log-path", IconName::Copy);
        match &self.run_log_path {
            Some(path) => {
                let text = path.to_string_lossy().into_owned();
                button
                    .tooltip(Tooltip::text(format!("Copy log path\n{text}")))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                    })
                    .into_any_element()
            }
            None => button
                .disabled(true)
                .tooltip(Tooltip::text("No ggo-diag log for this run"))
                .into_any_element(),
        }
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
        div()
            .debug_selector(|| RERUN_BUTTON_SELECTOR.to_string())
            .child(
                IconButton::new("ggo-charts-rerun", IconName::RotateCcw)
                    .disabled(rel.is_none())
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(Self::guarded_listener(cx, |this, _event, window, cx| {
                        this.rerun_selected(window, cx);
                    })),
            )
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
    fn render_log(kind: LogKind, lines: &Arc<Vec<String>>, cx: &mut Context<Self>) -> AnyElement {
        let selector = kind.selector();
        let section = v_flex()
            .w_full()
            .gap_1()
            .debug_selector(move || selector.to_string())
            .child(Label::new(kind.title()).size(LabelSize::Small));
        if lines.is_empty() {
            return section
                .child(Self::note(kind.empty_state()))
                .into_any_element();
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

    // The three load-failure sentences, exactly as the renderer prints
    // them. Named methods rather than literals at the render sites for
    // `LogKind::empty_state`'s reason: a sentence handed over as a literal
    // at the call site is a sentence no test can read, and these are the
    // sentences the error-state tests assert on.

    /// What the picker says when the runs list itself failed to load.
    fn runs_error_message(error: &str) -> String {
        format!("Failed to load runs: {error}")
    }

    /// What the detail view says when the selected run's samples failed.
    fn samples_error_message(error: &str) -> String {
        format!("Failed to load samples: {error}")
    }

    /// What a device run's view says when its pipeline log failed.
    fn device_log_error_message(error: &str) -> String {
        format!("Failed to load log: {error}")
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

    /// The I$ profile table: every function's `misses`/`evicted` across
    /// the run's kept frames, with a sortable "misses" header --
    /// `reports.rs`'s `profile_section`.
    ///
    /// Last in the detail view, and rendered for every run, exactly as
    /// there: a run with no profile rows gets the heading and the reason
    /// it is empty rather than silently nothing, which is how a user
    /// learns the data exists at all.
    fn render_profile_table(
        &self,
        profiles: &inspect::Profiles,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ascending = self.profile_sort_ascending;
        let section = Self::table(
            "I$ profile — misses by function",
            PROFILE_TABLE_SELECTOR,
            profiles.table_empty_state(),
            cx,
        );
        let rows = profiles.table().rows(ascending);
        if rows.is_empty() {
            return section.into_any_element();
        }
        section
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new("function")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        // The one interactive column header. The arrow is
                        // the affordance AND the state readout, matching
                        // ggo-ide's `arrow_up`/`arrow_down` icon. Wrapped
                        // in a selector-bearing div so a render test can
                        // click the header itself rather than reaching
                        // past it to the handler.
                        div()
                            .debug_selector(|| PROFILE_SORT_SELECTOR.to_string())
                            .child(
                                Button::new("ggo-charts-profile-sort", "misses")
                                    .label_size(LabelSize::XSmall)
                                    .end_icon(Icon::new(if ascending {
                                        IconName::ArrowUp
                                    } else {
                                        IconName::ArrowDown
                                    }))
                                    .tooltip(Tooltip::text(if ascending {
                                        "Sort by misses, largest first"
                                    } else {
                                        "Sort by misses, smallest first"
                                    }))
                                    .on_click(Self::guarded_listener(
                                        cx,
                                        |this, _event: &gpui::ClickEvent, _window, cx| {
                                            this.toggle_profile_sort(cx);
                                        },
                                    )),
                            ),
                    )
                    .child(
                        Label::new("evicted")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .children(rows.iter().map(|agg| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(agg.func.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx),
                        ),
                    )
                    .child(
                        Label::new(ggo_worldlib::charts::reports::fmt::with_thousands(
                            agg.misses,
                        ))
                        .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(ggo_worldlib::charts::reports::fmt::with_thousands(
                            agg.evicted,
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
            }))
            .into_any_element()
    }

    /// The click-to-inspect pane: one clicked frame's I$ misses, grouped
    /// by calling function with its inlined callees indented under it --
    /// `reports.rs`'s `inspect_panel`.
    fn render_frame_inspect(
        &self,
        selected: &inspect::FrameInspect,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let frame = selected.frame;
        v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .rounded_sm()
            .bg(cx.theme().colors().element_background)
            .debug_selector(|| FRAME_INSPECT_SELECTOR.to_string())
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        Label::new(format!("Frame {frame} — I$ misses by function"))
                            .size(LabelSize::Small),
                    )
                    .child(
                        div()
                            .debug_selector(|| INSPECT_CLOSE_SELECTOR.to_string())
                            .child(
                                IconButton::new("ggo-charts-inspect-close", IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Close the frame inspector"))
                                    .on_click(Self::guarded_listener(
                                        cx,
                                        |this, _event: &gpui::ClickEvent, _window, cx| {
                                            this.frame_inspect = None;
                                            cx.notify();
                                        },
                                    )),
                            ),
                    ),
            )
            // ggo-ide's subtitle verbatim: `evicted` is the column most
            // often misread (it counts this function's lines DISPLACED by
            // someone else, not lines it displaced).
            .child(
                Self::note(
                    "outer function > inlined callees; evicted = lines of that function \
                     displaced this frame (the victims)",
                )
                .size(LabelSize::XSmall),
            )
            .children(selected.empty_state().map(Self::note))
            .children(selected.groups().iter().flat_map(|group| {
                let mut rows = vec![Self::inspect_row(
                    group.caller.clone(),
                    group.misses,
                    group.evicted,
                    false,
                    cx,
                )];
                rows.extend(group.leaves.iter().map(|leaf| {
                    // A leaf sharing its caller's name is that caller's
                    // own (non-inlined) samples -- `RunPage.tsx`'s
                    // `<self>`, applied per leaf and independently of the
                    // whole-group collapse `group_frame_profile` resolved.
                    let label = if leaf.func == group.caller {
                        SELF_LEAF_LABEL.to_string()
                    } else {
                        leaf.func.clone()
                    };
                    Self::inspect_row(label, leaf.misses, leaf.evicted, true, cx)
                }));
                rows
            }))
            .into_any_element()
    }

    /// One row of the inspect pane, indented when it is an inlined callee.
    fn inspect_row(
        label: String,
        misses: i64,
        evicted: i64,
        leaf: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        h_flex()
            .w_full()
            .gap_2()
            .justify_between()
            .when(leaf, |el| el.pl_3())
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .color(if leaf { Color::Muted } else { Color::Default })
                        .buffer_font(cx),
                ),
            )
            .child(
                Label::new(ggo_worldlib::charts::reports::fmt::with_thousands(misses))
                    .size(LabelSize::Small),
            )
            .child(
                Label::new(ggo_worldlib::charts::reports::fmt::with_thousands(evicted))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
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

    /// The Historic checkbox + how many prior runs it has to draw --
    /// `reports.rs`'s `historic_toggle_row`.
    ///
    /// The count is stated either way, switched on or off, because "the
    /// overlay is on and I see nothing" and "there is nothing to overlay"
    /// are the two states a reader has to be able to tell apart. When
    /// there are none the checkbox is disabled and [`NO_PRIOR_RUNS`] says
    /// so; ggo-ide renders "(0 prior runs)" beside a live checkbox, which
    /// invites a click that cannot do anything.
    fn render_historic_toggle(
        &self,
        prior_runs: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let plural = if prior_runs == 1 { "run" } else { "runs" };
        h_flex()
            .w_full()
            .gap_2()
            .debug_selector(|| HISTORIC_TOGGLE_SELECTOR.to_string())
            .child(
                Checkbox::new(
                    "ggo-charts-historic",
                    ToggleState::from(self.historic_enabled),
                )
                .label("Historic")
                .disabled(prior_runs == 0)
                .on_click(Self::guarded_listener(
                    cx,
                    |this, _toggle: &ToggleState, _window, cx| this.toggle_historic(cx),
                )),
            )
            .child(
                Label::new(if prior_runs == 0 {
                    NO_PRIOR_RUNS.to_string()
                } else {
                    format!("{prior_runs} prior {plural}")
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_chart(
        &self,
        ix: usize,
        spec: &Arc<ChartSpec>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let title = SharedString::from(spec.title.clone());
        let selectable = spec.selectable;
        // Drag-zoom is a LINE chart affordance, exactly as in ggo-ide:
        // `line.rs` owns `zoom_domain` and `stacked.rs`/`histogram.rs`
        // state plainly that they have "no zoom, no drag". A histogram has
        // no frame axis to zoom in the first place.
        let zoomable =
            matches!(spec.kind, chart_geom::ChartKind::Line { .. }) && !spec.x.is_empty();
        // Refcount bump, not a deep clone -- see `DetailState::Ready`.
        let spec = spec.clone();
        let view = self.view_for(ix);
        let palette = chart_paint::Palette::from_theme(cx);
        let chart_bounds = self.chart_bounds.clone();
        let scenes = self.scenes.clone();

        let canvas = gpui::canvas(
            move |canvas_bounds, _window, _cx| {
                // Prepaint: record where this chart landed (for
                // hit-testing) and build its scene at the size it
                // actually got -- or reuse the last one, when nothing this
                // chart's scene depends on has changed. See `CachedScene`
                // for the measurement that put the cache here; the short
                // version is that a hover move re-renders the panel and
                // this closure then ran for every chart, not just the one
                // under the cursor.
                if let Some(slot) = chart_bounds.borrow_mut().get_mut(ix) {
                    *slot = Some(canvas_bounds);
                }
                let size = (
                    f32::from(canvas_bounds.size.width),
                    f32::from(canvas_bounds.size.height),
                );
                let mut cache = scenes.borrow_mut();
                let Some(slot) = cache.get_mut(ix) else {
                    // No slot: the chart list is being laid out before
                    // `render_detail` sized the cache. Build uncached
                    // rather than growing it from a paint closure.
                    return build_chart_scene(&spec, size, &view);
                };
                if let Some(cached) = slot.as_ref()
                    && cached.size == size
                    && cached.view == view
                    && Arc::ptr_eq(&cached.spec, &spec)
                {
                    return cached.scene.clone();
                }
                let scene = build_chart_scene(&spec, size, &view);
                *slot = Some(CachedScene {
                    spec: spec.clone(),
                    size,
                    view,
                    scene: scene.clone(),
                });
                scene
            },
            move |canvas_bounds, scene, window, cx| {
                chart_paint::paint_scene(&scene, &palette, canvas_bounds, window, cx)
            },
        )
        .size_full();

        // The inspect pane renders directly beneath the chart that opened
        // it -- see `inspect::FrameInspect::chart` for why not ggo-ide's
        // one fixed slot.
        let inspect_pane = self
            .frame_inspect
            .as_ref()
            .filter(|sel| sel.chart == ix)
            .map(|sel| self.render_frame_inspect(sel, cx));

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
                    // One handler for the whole press/release gesture,
                    // because the three outcomes are three readings of the
                    // SAME event pair and splitting them across
                    // `on_mouse_up` and `on_click` would race: gpui fires
                    // both on the release, and a zoom must not also select
                    // a frame. `ClickEvent::Mouse` carries the original
                    // `down` alongside the `up`, which is what makes the
                    // drag/click discrimination readable from one place --
                    // see `resolve_gesture`.
                    .when(selectable || zoomable, |el| {
                        el.cursor_pointer().on_click(Self::guarded_listener(
                            cx,
                            move |this, event: &gpui::ClickEvent, _window, cx| {
                                this.resolve_gesture(ix, zoomable, selectable, event, cx);
                            },
                        ))
                    })
                    // The drag PREVIEW only (the translucent band): press
                    // records where, the move handler below widens it, and
                    // a release outside the chart -- which gpui never turns
                    // into a click, so `resolve_gesture` never sees it --
                    // abandons it.
                    .when(zoomable, |el| {
                        el.on_mouse_down(
                            gpui::MouseButton::Left,
                            Self::guarded_listener(
                                cx,
                                move |this, event: &gpui::MouseDownEvent, _window, cx| {
                                    this.begin_drag(ix, event.position, cx);
                                },
                            ),
                        )
                        .on_mouse_up_out(
                            gpui::MouseButton::Left,
                            Self::guarded_listener(
                                cx,
                                move |this, _event: &gpui::MouseUpEvent, _window, cx| {
                                    this.cancel_drag(ix, cx);
                                },
                            ),
                        )
                    })
                    .on_mouse_move(Self::guarded_listener(
                        cx,
                        move |this, event: &MouseMoveEvent, _window, cx| {
                            this.hover_chart(ix, event.position, cx);
                        },
                    ))
                    // `on_mouse_move` is hitbox-gated, so it never fires
                    // once the cursor leaves this chart -- moving into the
                    // gap between charts, onto the header, or off the
                    // panel entirely would otherwise leave the crosshair
                    // and readout pinned where the cursor last was.
                    // `on_hover(false)` is the only signal for that -- and
                    // it is NOT a signal that a drag ended; see
                    // `clear_hover` for why it fires at every drag's start.
                    .on_hover(Self::guarded_listener(
                        cx,
                        move |this, hovered: &bool, _window, cx| {
                            if !*hovered {
                                this.clear_hover(ix, cx);
                            }
                        },
                    )),
            )
            .children(inspect_pane)
            .into_any_element()
    }

    /// Window-space cursor -> canvas-local hover on chart `ix`. A move
    /// outside the recorded bounds clears the hover rather than pinning a
    /// stale readout.
    ///
    /// Every hover change `cx.notify()`s, which re-renders ALL of the
    /// run's charts. Until R5 that meant every one of them rebuilt its
    /// scene from scratch (`accumulate`/`bins`/`envelope` over the full
    /// sample set) on every mouse-move frame -- O(charts x frames) for a
    /// change that can only ever affect one chart. It is now memoized on
    /// exactly what a scene is built from, so a hover move rebuilds the
    /// chart under the cursor and reuses the rest: see [`CachedScene`] for
    /// the measurement, and
    /// `test_a_hover_move_rebuilds_only_the_chart_under_the_cursor` for
    /// the assertion that it stays that way.
    fn hover_chart(&mut self, ix: usize, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let bounds = self.chart_bounds.borrow().get(ix).copied().flatten();
        let next = bounds.filter(|b| b.contains(&position)).map(|b| Hover {
            chart: ix,
            x: f32::from(position.x - b.origin.x),
            y: f32::from(position.y - b.origin.y),
        });
        // Widen the drag band to follow the cursor. Same event, because
        // the band's far edge IS the hover position; a second
        // `on_mouse_move` listener for it would just be this one again.
        let dragged = match (&mut self.drag, next) {
            (Some(drag), Some(hover)) if drag.chart == ix && drag.to_x != hover.x => {
                drag.to_x = hover.x;
                true
            }
            _ => false,
        };
        if self.hover != next {
            self.hover = next;
            cx.notify();
        } else if dragged {
            cx.notify();
        }
    }

    /// A left press on chart `ix`: start a drag preview there.
    ///
    /// The press does NOT clear the zoom or select anything -- both of
    /// those are decided on release ([`Self::resolve_gesture`]), where the
    /// gesture is finally known to have been a click, a drag or a
    /// double-click.
    fn begin_drag(&mut self, ix: usize, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.chart_bounds.borrow().get(ix).copied().flatten() else {
            return;
        };
        if !bounds.contains(&position) {
            return;
        }
        let x = f32::from(position.x - bounds.origin.x);
        self.drag = Some(Drag {
            chart: ix,
            from_x: x,
            to_x: x,
        });
        cx.notify();
    }

    /// The release, and the three things it can mean.
    ///
    /// 1. **Double-click** (`down.click_count >= 2`) -> drop this chart's
    ///    zoom, back to the full frame domain. `line.rs` hand-rolls this
    ///    from an `Instant` + a drift box because iced's canvas events
    ///    carry no click count; gpui's `MouseDownEvent` does, so the
    ///    platform's own double-click interval is used instead of a
    ///    400 ms constant of ours.
    /// 2. **A drag** -- press and release more than
    ///    [`chart_geom::DRAG_MIN_PX`] apart in x -> zoom to the dragged
    ///    window, and select NOTHING. This is the load-bearing half of the
    ///    discrimination now that R4 put a frame-selection click on these
    ///    same charts: without the threshold every click would zoom to a
    ///    two-frame window, and without the exclusion every zoom would
    ///    also open the inspect pane for whichever frame the release
    ///    happened to land on.
    /// 3. **A click** -> R4's frame selection, unchanged, on the charts
    ///    `chart_set` marked selectable.
    ///
    /// The endpoints come from the event's own `down`/`up`, not from
    /// [`Self::drag`]: that state is a per-chart preview that several
    /// things may legitimately drop, while the event pair IS the gesture.
    /// `mouse_position()` stays the source for case 3 for R4's reason -- a
    /// keyboard-dispatched click has no cursor and must select nothing
    /// rather than the hitbox's corner.
    fn resolve_gesture(
        &mut self,
        ix: usize,
        zoomable: bool,
        selectable: bool,
        event: &gpui::ClickEvent,
        cx: &mut Context<Self>,
    ) {
        let drag = self.drag.take();
        if drag.is_some() {
            cx.notify();
        }
        let gpui::ClickEvent::Mouse(mouse) = event else {
            return;
        };
        if zoomable && mouse.down.click_count >= 2 {
            self.reset_zoom(ix, cx);
            return;
        }
        let dx = f32::from(mouse.up.position.x - mouse.down.position.x).abs();
        if zoomable && dx >= chart_geom::DRAG_MIN_PX {
            self.zoom_chart(ix, mouse.down.position, mouse.up.position, cx);
            return;
        }
        if selectable && let Some(position) = event.mouse_position() {
            self.select_frame(ix, position, cx);
        }
    }

    /// Zoom chart `ix`'s x-domain to the window dragged between two
    /// window-space points.
    ///
    /// The drag is interpreted through the scale the chart is CURRENTLY
    /// painted at -- `view_for(ix)`'s zoom, not the full domain -- so a
    /// second drag zooms further in rather than re-reading the same pixels
    /// against the original domain. `zoom_domain` then clamps back inside
    /// the full domain, so no sequence of drags can walk off the data.
    fn zoom_chart(
        &mut self,
        ix: usize,
        from: gpui::Point<Pixels>,
        to: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(DetailState::Ready(detail)) = &self.detail else {
            return;
        };
        let Some(spec) = detail.charts.get(ix) else {
            return;
        };
        let Some(bounds) = self.chart_bounds.borrow().get(ix).copied().flatten() else {
            return;
        };
        if spec.x.is_empty() {
            return;
        }
        let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let plot = chart_geom::plot_for(spec, size);
        if !plot.is_drawable() {
            return;
        }
        let view = self.view_for(ix);
        let scale = chart_geom::x_scale_for(spec, plot, &view);
        let domain = chart_geom::zoom_domain(
            f32::from(from.x - bounds.origin.x),
            f32::from(to.x - bounds.origin.x),
            &scale,
            chart_geom::full_x_domain(&spec.x),
        );
        self.zoom.insert(ix, domain);
        cx.notify();
    }

    /// Back to the full frame domain on chart `ix` -- the double-click
    /// reset. A chart that was never zoomed is left exactly as it was.
    fn reset_zoom(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.zoom.remove(&ix).is_some() {
            cx.notify();
        }
    }

    /// The Historic overlay switch -- `reports.rs`'s `ToggleHistoric`.
    ///
    /// **Derives nothing.** The prior runs were fetched and their series
    /// built in the same background pass as everything else
    /// ([`detail::load`]), so this flips a bool and the next render paints
    /// (or stops painting) overlays the specs already carry. ggo-ide
    /// fetches lazily on this event instead; this panel deliberately does
    /// not, because that would be a blocking query hanging off a UI event
    /// and a second load path to keep generation-guarded.
    ///
    /// The chart LIST does not change, only what each chart draws, so the
    /// frame selection and the zoom windows -- both keyed by chart index
    /// -- stay valid and are deliberately left alone (R4's concern (4)).
    fn toggle_historic(&mut self, cx: &mut Context<Self>) {
        self.historic_enabled = !self.historic_enabled;
        cx.notify();
    }

    /// A click on chart `ix` at window-space `position`: select the frame
    /// under the cursor, and group that frame's profile rows for the
    /// inspect pane.
    ///
    /// Clicking the point that is already selected clears the selection
    /// instead -- `RunPage.tsx`'s `pickFrame` (`n === selFrame() ? null :
    /// n`), which is what makes the pane dismissable without a second
    /// affordance (there is a Close button too, for discoverability).
    /// The chart is part of "already selected" here and is not in
    /// `pickFrame`, because this pane is anchored under the chart that
    /// opened it rather than in one fixed slot: clicking the same frame
    /// on a DIFFERENT chart moves the pane there, which is what the click
    /// looks like it should do, instead of making it vanish.
    ///
    /// **Derives nothing that scales with the run.** The hit-test is
    /// `chart_geom::frame_at` (a legend layout and one pass over the
    /// frame axis) and the grouping reaches only the clicked frame's own
    /// rows, through the index `detail::build` already built off-thread.
    /// The listener that calls this holds the no-derivation guard, so a
    /// future edit that reached for `detail::build`/`load` here fails
    /// every test that clicks.
    fn select_frame(&mut self, ix: usize, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let next = {
            // Perf runs only. A device selection sets `device_log`, never
            // `detail`, so there is no chart to click and no profile to
            // inspect -- the two id spaces stay apart here as everywhere
            // else (`Selection`'s doc).
            let Some(DetailState::Ready(detail)) = &self.detail else {
                return;
            };
            let Some(spec) = detail.charts.get(ix) else {
                return;
            };
            if !spec.selectable {
                return;
            }
            let Some(bounds) = self.chart_bounds.borrow().get(ix).copied().flatten() else {
                return;
            };
            if !bounds.contains(&position) {
                return;
            }
            let Some(frame) = chart_geom::frame_at(
                spec,
                (f32::from(bounds.size.width), f32::from(bounds.size.height)),
                (
                    f32::from(position.x - bounds.origin.x),
                    f32::from(position.y - bounds.origin.y),
                ),
                // The SAME view the prepaint painted this chart under, so
                // a zoomed chart resolves the click against the domain
                // the user is looking at (R4's concern (1)).
                &self.view_for(ix),
            ) else {
                return;
            };
            match &self.frame_inspect {
                Some(selected) if selected.frame == frame && selected.chart == ix => None,
                _ => Some(detail.profiles.inspect(ix, frame)),
            }
        };
        self.frame_inspect = next;
        cx.notify();
    }

    /// The I$ profile table's "misses" column header -- flips the sort
    /// direction. `reports.rs`'s `ToggleProfileSortAscending`.
    ///
    /// Both orders were derived off-thread by
    /// `profile::aggregate_profile_sorted`, so this flips a bool and the
    /// render picks the other vector. It does not sort, and it does not
    /// reverse: R1 ported the sort out of ggo-ide's widget so that the
    /// panel would stop being the thing that expresses the ordering.
    fn toggle_profile_sort(&mut self, cx: &mut Context<Self>) {
        self.profile_sort_ascending = !self.profile_sort_ascending;
        cx.notify();
    }

    /// Drops the hover if (and only if) chart `ix` currently owns it --
    /// the guard matters because hover-end on the chart being left can
    /// arrive AFTER hover-start on the chart being entered, which would
    /// otherwise clear the new chart's fresh readout.
    ///
    /// **A hover-end that arrives while this chart is being dragged is not
    /// one.** gpui computes a div's hover as `has_mouse_down.is_none() &&
    /// !cx.has_active_drag() && hitbox.is_hovered(window)`
    /// (`div.rs:3038-3041`), so pressing the button on a chart flips its
    /// hover to false *by definition*, and `on_hover(false)` fires once at
    /// the start of every drag with the cursor still sitting on the chart.
    /// Taken at face value that ends the readout the moment a drag begins
    /// -- and, when this fn also dropped `self.drag`, it destroyed the
    /// selection band before a single frame could paint it, which is how
    /// R5 round 1 shipped a drag preview that never appeared. The drag is
    /// released by the gesture that ends it instead: `resolve_gesture` on
    /// a release inside the chart, [`Self::cancel_drag`] on one outside.
    fn clear_hover(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.drag.is_some_and(|d| d.chart == ix) {
            return;
        }
        if self.hover.is_some_and(|h| h.chart == ix) {
            self.hover = None;
            cx.notify();
        }
    }

    /// Abandon the drag preview on chart `ix` -- the release that happened
    /// somewhere else.
    ///
    /// gpui only fires a click when press and release share a hitbox, so a
    /// release outside the chart never reaches `resolve_gesture` and never
    /// zooms (`line.rs` clamps such a drag instead; matched to gpui's own
    /// rule rather than re-implemented). Without this the band would hang
    /// on screen over a gesture that has already ended.
    fn cancel_drag(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.drag.take_if(|d| d.chart == ix).is_some() {
            cx.notify();
        }
    }

    /// [`Self::render_message`] for FAILURES: same centered layout, but
    /// the text is copyable so the error can be pasted into a report.
    fn render_load_error(
        &self,
        id: &'static str,
        message: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(
                ggo_common::CopyableText::new(id, message)
                    .size(LabelSize::Default)
                    .color(Color::Muted),
            )
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
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
        // Nothing below may DERIVE anything -- this runs once per frame,
        // i.e. on every hover mouse-move while a run is showing, so a
        // re-derivation here is far worse than the once-per-selection one
        // `select_run` used to do. Same guard, same reason; see `detail`'s
        // module doc for exactly what it does and does not cover.
        let _no_build_here = detail::no_build_here();
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    /// The dock-era default width, kept as the tests' viewport width so
    /// the layout-sensitive assertions keep their historical geometry.
    const DEFAULT_WIDTH: Pixels = px(360.);

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// The agent's close: gone when there is a tab, `false` when there
    /// is not, and a refusal -- tab left alone -- when the run it names
    /// is not the one on screen.
    #[gpui::test]
    async fn test_close_charts_item_closes_only_the_run_it_names(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        workspace.update_in(cx, |workspace, window, cx| {
            open_charts_item(workspace, window, cx, |_, _, _| {});
            // The picker, not a run: a named run cannot be what it shows.
            let err = close_charts_item(workspace, Some(5), window, cx).unwrap_err();
            assert!(err.contains("runs list"), "{err}");
            assert_eq!(
                workspace.items_of_type::<ChartsItem>(cx).count(),
                1,
                "left alone"
            );
            assert_eq!(close_charts_item(workspace, None, window, cx), Ok(true));
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(
                workspace.items_of_type::<ChartsItem>(cx).count(),
                0,
                "closed"
            );
            assert_eq!(close_charts_item(workspace, None, window, cx), Ok(false));
        });
    }

    /// The reports view is a SINGLETON center tab: `open_charts_item`
    /// creates it once and every later call activates the same item.
    #[gpui::test]
    async fn test_open_charts_item_is_a_singleton_center_tab(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let first = workspace.update_in(cx, |workspace, window, cx| {
            open_charts_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<ChartsItem>(cx)
                .next()
                .expect("open_charts_item adds the item")
                .read(cx)
                .panel()
                .clone()
        });
        let second = workspace.update_in(cx, |workspace, window, cx| {
            open_charts_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<ChartsItem>(cx)
                .next()
                .expect("still there")
                .read(cx)
                .panel()
                .clone()
        });
        assert_eq!(
            first.entity_id(),
            second.entity_id(),
            "one reports view, re-focused"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<ChartsItem>(cx).count(), 1);
        });

        // HEAVY action: opening a report folds every center split into
        // one pane, moving (not closing) the splits' tabs.
        workspace.update_in(cx, |workspace, window, cx| {
            let active = workspace.active_pane().clone();
            let new_pane =
                workspace.split_pane(active, workspace::SplitDirection::Right, window, cx);
            let extra = cx.new(workspace::item::test::TestItem::new);
            new_pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(extra), true, true, None, window, cx);
            });
            assert_eq!(workspace.panes().len(), 2, "split exists before the report");
        });
        workspace.update_in(cx, |workspace, window, cx| {
            open_charts_item(workspace, window, cx, |_, _, _| {});
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 1, "the split folded away");
            assert_eq!(
                workspace.active_pane().read(cx).items_len(),
                2,
                "the split's tab moved into the surviving pane"
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

    /// The `frame_budget_cycles` every seeded fixture run carries: one
    /// 60 Hz vsync period at 33.33 MHz, the same number ggo-ide's runs
    /// record. Bound into the INSERT rather than spelled into its SQL so
    /// [`DEVICE_FRAME_CYCLES`] cannot describe itself as a multiple of a
    /// budget the fixture stopped using.
    const SEEDED_FRAME_BUDGET_CYCLES: i64 = 555_549;

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
                 VALUES (1, 1, '2026-08-01T00:00:00Z', ?1, ?2, 'arena')",
                (frames + 1, SEEDED_FRAME_BUDGET_CYCLES),
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

    /// Two vsync periods of the seeded run's budget: run 55's half-rate
    /// shape, and a `cyc` every frame can derive an fps from.
    const DEVICE_FRAME_CYCLES: i64 = 2 * SEEDED_FRAME_BUDGET_CYCLES;

    /// Turns the seeded fixture run into a DEVICE capture. `frame.cyc`
    /// is `NOT NULL DEFAULT 0` and only a device capture writes it, so
    /// `seed_run_with_samples` alone leaves an emulator-shaped run with
    /// no FPS chart at all -- which is exactly what the hero slot is
    /// keyed on.
    fn seed_frame_cycles(db_path: &std::path::Path) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("UPDATE frame SET cyc = ?1", [DEVICE_FRAME_CYCLES])
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
                    "frame budget 555,549 cyc (60 fps target) · scanout 164,400 · refill 100 · \
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
            panel.db_path_override = Some(db_path.clone());
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
            .debug_bounds(LogKind::Console.selector())
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
            cx.debug_bounds(LogKind::Console.selector()).is_some(),
            "and its console -- the frameless run is exactly the one whose \
             last few UART lines say what went wrong"
        );
        assert!(
            cx.debug_bounds(KPI_ROW_SELECTOR).is_none(),
            "no KPI row for a run that measured nothing"
        );
    }

    /// The one section that does NOT follow ggo-ide's `detail_view`
    /// order: on a device run the measured-fps chart is the report's
    /// HERO and is painted ABOVE the diagnostic tables, because "how
    /// fast is it actually running" is the question the page exists to
    /// answer and the tables are usually empty.
    ///
    /// Everything below it keeps the order the test above pins, and the
    /// remaining charts stay UNDER the KPI row -- promoting the hero
    /// must move one chart, not reshuffle the report. The bounds are
    /// read out of `chart_bounds` by index, which is the second thing
    /// this pins: the hero keeps its index into the ORIGINAL chart set
    /// (0), so `chart_bounds`/`scenes` -- both sized `charts.len()` and
    /// indexed by the same `ix` `render_chart` hit-tests and caches
    /// with -- stay aligned with `chart_specs()`.
    #[gpui::test]
    async fn test_the_fps_chart_is_painted_above_the_diagnostic_tables(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        seed_frame_cycles(&db_path);
        seed_uart(&db_path, &["asset: MISS \"grooble.til\""]);

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path.clone());
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
            assert_eq!(
                panel.chart_specs()[0].title,
                chart_set::FPS_CHART_TITLE,
                "the device gate is tripped and the fps chart leads the set"
            );
        });

        cx.simulate_resize(gpui::size(DEFAULT_WIDTH, px(6000.)));
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        let (hero, second) = panel.update(cx, |panel, _cx| {
            let bounds = panel.chart_bounds.borrow();
            (
                bounds[0].expect("the fps chart must have been laid out"),
                bounds[1].expect("the chart under it must have been laid out"),
            )
        });
        let failures = cx
            .debug_bounds(FAILURES_SELECTOR)
            .expect("the failed-asset-loads table must be painted");
        let kpis = cx
            .debug_bounds(KPI_ROW_SELECTOR)
            .expect("the KPI row must be painted for a run with frames");

        assert!(
            hero.origin.y < failures.origin.y,
            "the fps chart leads the whole report, above even the tables"
        );
        assert!(
            failures.origin.y < kpis.origin.y,
            "and nothing else moved: the tables still sit above the KPI row"
        );
        assert!(
            kpis.origin.y < second.origin.y,
            "only ONE chart is promoted -- the rest stay under the KPIs"
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
    }

    /// **The wording `render_log` actually uses**, read from the same place
    /// it reads it. Every other assertion in this module reaches a string
    /// through a view-model field; these two were passed to `render_log` as
    /// literal arguments, which meant a reviewer could replace the
    /// console's empty state with *"this run predates UART capture"* -- the
    /// precise sentence R2's review blocked -- and watch the suite stay
    /// green. `LogKind::empty_state` exists so there is one place to read.
    #[test]
    fn the_log_empty_states_are_the_hedged_ones_the_renderer_uses() {
        // The console's IS the diagnostic tables' -- the same `uart` rows
        // are the signal for all three, so they cannot draw different
        // conclusions from their absence.
        assert_eq!(LogKind::Console.empty_state(), report::NO_UART);
        // ...and it names a state, never a cause (R2's blocker, restated
        // at the surface that renders it).
        let console = LogKind::Console.empty_state();
        assert!(
            console.starts_with("no UART lines captured for this run"),
            "the fact comes first: {console}"
        );
        assert!(
            console.contains("none emitted")
                && console.contains("diag run")
                && console.contains("predating UART persistence"),
            "causes are offered as examples, not asserted: {console}"
        );

        // The device log is a different table (`run_log`, pipeline
        // narration) so it needs its own sentence -- but the same rule: it
        // says no lines were recorded, not that nothing happened.
        let device = LogKind::DeviceLog.empty_state();
        assert_eq!(device, NO_DEVICE_LOG);
        assert_ne!(device, console);
        assert!(
            device.starts_with("no log lines recorded"),
            "a state, not a cause: {device}"
        );

        // And the two surfaces stay distinguishable in a painted frame.
        assert_ne!(LogKind::Console.selector(), LogKind::DeviceLog.selector());
        assert_ne!(LogKind::Console.title(), LogKind::DeviceLog.title());
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
            cx.debug_bounds(LogKind::Console.selector()).is_none(),
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
            cx.debug_bounds(LogKind::Console.selector()).is_some(),
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

    /// `open_run` is the emu panel's hop and it lands whenever its detached
    /// lookup finishes. A user who picked a device run in the meantime has
    /// moved on, and the late hop must be dropped rather than silently
    /// replacing their choice with a run from the other id space.
    #[gpui::test]
    async fn test_a_device_selection_survives_an_in_flight_open_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let diag_path = dir.path().join("diag.db");
        seed_run_with_samples(&db_path, 4);
        seed_device_run(&diag_path, "run-1", "2026-08-02T00:00:00Z", "PASS");

        let panel = history_panel(cx, db_path, diag_path).await;
        let summary = panel.update(cx, |panel, _cx| history_of(panel).runs[0].clone());

        panel.update(cx, |panel, cx| {
            // The hop starts...
            panel.open_run(1, cx);
            // ...and the user picks a device run before it lands.
            panel.select_device_run(summary.clone(), cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            match &panel.selected {
                Some(Selection::Device(run)) => assert_eq!(run.id, "run-1"),
                _ => panic!("the user's later choice must win, not the stale hop"),
            }
            assert!(panel.detail.is_none(), "and no perf detail lands over it");
        });
    }

    /// The other direction: with nothing else selected, the hop still
    /// works. (A guard that dropped every hop would pass the test above.)
    #[gpui::test]
    async fn test_open_run_still_selects_the_run_it_was_given(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(db_path);
                panel
            })
        });
        panel.update(cx, |panel, cx| panel.open_run(1, cx));
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            match &panel.selected {
                Some(Selection::Perf(run)) => assert_eq!(run.id, 1),
                _ => panic!("the hop must select the run it was handed"),
            }
            assert!(!panel.chart_specs().is_empty());
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
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            open_charts_item(workspace, window, cx, |_, _, _| {});
            workspace
                .items_of_type::<ChartsItem>(cx)
                .next()
                .expect("open_charts_item adds the item")
                .read(cx)
                .panel()
                .clone()
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

    // --------------------------- click-to-inspect + the I$ profile table

    /// `seed_run_with_samples`'s single profile row is not enough to
    /// exercise the grouping, so this adds a real per-frame shape: a
    /// cold-cache burst on the ignored frame 0, two functions under one
    /// caller on frame 1, two callers on frame 3 -- and nothing at all on
    /// frame 4, which is the "no per-function data" case.
    fn seed_profile_rows(db_path: &std::path::Path) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            for (frame, caller, func, misses, evicted) in [
                (0i64, "boot", "boot", 9_000i64, 4_000i64),
                (1, "main", "update", 30, 2),
                (1, "main", "render", 10, 8),
                (3, "main", "update", 5, 1),
                (3, "draw", "blit", 40, 0),
            ] {
                conn.execute(
                    "INSERT INTO profile (run_id, frame, caller, func, misses, evicted)
                     VALUES (1, ?1, ?2, ?3, ?4, ?5)",
                    (frame, caller, func, misses, evicted),
                )
                .await
                .unwrap();
            }
        });
    }

    /// A second run in the same fixture db, so the run -> run selection
    /// path can be exercised for real. Deliberately a DIFFERENT frame
    /// range (10..=12) from run 1: a frame number that survived the
    /// switch would then name a frame this run does not even have.
    fn seed_second_run(db_path: &std::path::Path) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(db_path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, frame_budget_cycles, label)
                 VALUES (2, 1, '2026-08-03T00:00:00Z', 3, 555549, 'arena-2')",
                (),
            )
            .await
            .unwrap();
            for n in 10..=12 {
                conn.execute(
                    "INSERT INTO frame
                       (run_id, n, instrs, i_hits, i_misses, d_hits, d_misses,
                        scanout_wire, blit_wire, miss_wire, wire_total, over_budget)
                     VALUES (2, ?1, 1000, 0, 7, 0, 3, 164400, 30, 40, 164470, 0)",
                    [n],
                )
                .await
                .unwrap();
            }
        });
    }

    /// Window-space centre of the point chart `ix` drew for `frame`,
    /// derived from the bounds its own prepaint recorded and the same
    /// scale it painted with. This is what a user's cursor is over when
    /// they click that point.
    fn point_of(panel: &ChartsPanel, ix: usize, frame: f32) -> gpui::Point<Pixels> {
        let bounds = panel.chart_bounds.borrow()[ix].expect("chart must have been laid out");
        let spec = &panel.chart_specs()[ix];
        let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let plot = chart_geom::plot_for(spec, size);
        // Through the chart's CURRENT view, so a zoomed chart's frame 3 is
        // where the user now sees it rather than where it used to be.
        let x = chart_geom::x_scale_for(spec, plot, &panel.view_for(ix)).map(frame);
        gpui::point(
            bounds.origin.x + px(x),
            bounds.origin.y + px(plot.y + plot.h / 2.0),
        )
    }

    /// A press at `from`, a move, and a release at `to` -- the gesture the
    /// panel has to tell apart from a click.
    fn simulate_drag(
        cx: &mut gpui::VisualTestContext,
        from: gpui::Point<Pixels>,
        to: gpui::Point<Pixels>,
    ) {
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, gpui::Modifiers::default());
    }

    /// A press+release the platform reports as the second click of a
    /// double click. `simulate_click` only ever sends `click_count: 1`.
    fn simulate_double_click(cx: &mut gpui::VisualTestContext, at: gpui::Point<Pixels>) {
        cx.simulate_event(gpui::MouseDownEvent {
            position: at,
            modifiers: gpui::Modifiers::default(),
            button: gpui::MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: at,
            modifiers: gpui::Modifiers::default(),
            button: gpui::MouseButton::Left,
            click_count: 2,
        });
    }

    fn chart_index(panel: &ChartsPanel, title: &str) -> usize {
        panel
            .chart_specs()
            .iter()
            .position(|c| c.title == title)
            .unwrap_or_else(|| panic!("{title} must be in the chart set"))
    }

    /// A drawn window showing the seeded run's detail view, ready to be
    /// clicked. Tall enough that every chart is laid out rather than
    /// scrolled out of the frame.
    async fn drawn_detail_window(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        gpui::Entity<ChartsPanel>,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        seed_profile_rows(&db_path);

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path.clone());
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
        // Tall enough that every chart AND the profile table under them
        // are laid out inside the window rather than scrolled out of it
        // -- a hitbox the window never painted cannot be clicked.
        cx.simulate_resize(gpui::size(DEFAULT_WIDTH, px(6000.)));
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        (dir, db_path, panel, cx)
    }

    /// The whole click path, through a real mouse event on a real
    /// painted canvas: the frame under the cursor is resolved, its rows
    /// are grouped, and the pane is painted beneath the chart clicked.
    #[gpui::test]
    async fn test_clicking_a_plot_opens_the_inspect_pane_for_that_frame(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, at) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 3.0))
        });
        cx.simulate_click(at, gpui::Modifiers::default());

        panel.update(cx, |panel, _cx| {
            let selected = panel
                .frame_inspect
                .as_ref()
                .expect("the click must select a frame");
            assert_eq!(selected.frame, 3, "the frame the cursor was over");
            assert_eq!(selected.chart, ix, "and the chart it was over");
            let callers: Vec<&str> = selected
                .groups()
                .iter()
                .map(|g| g.caller.as_str())
                .collect();
            assert_eq!(callers, vec!["draw", "main"], "misses desc");
            assert_eq!(selected.empty_state(), None);
        });

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        let pane = cx
            .debug_bounds(FRAME_INSPECT_SELECTOR)
            .expect("the inspect pane must be painted");
        let chart = panel.update(cx, |panel, _cx| panel.chart_bounds.borrow()[ix].unwrap());
        assert!(
            pane.origin.y >= chart.origin.y,
            "the pane renders beneath the chart that opened it"
        );
    }

    /// Clicking the selected frame again dismisses the pane --
    /// `RunPage.tsx`'s `pickFrame` toggle.
    #[gpui::test]
    async fn test_clicking_the_selected_frame_again_clears_it(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let at = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 3.0)
        });
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| assert!(panel.frame_inspect.is_some()));
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.frame_inspect.is_none(),
                "the second click on the same point closes the pane"
            );
        });
    }

    /// ...but the same FRAME on a different chart moves the pane rather
    /// than closing it: the pane is anchored under the chart that opened
    /// it, so a click that looks like "show me this frame here" must not
    /// read as "dismiss".
    #[gpui::test]
    async fn test_the_same_frame_on_another_chart_moves_the_pane(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (first, at_first) = panel.update(cx, |panel, _cx| {
            let first = chart_index(panel, "Cache misses per frame");
            (first, point_of(panel, first, 3.0))
        });
        cx.simulate_click(at_first, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.frame_inspect.as_ref().unwrap().chart, first);
        });

        // Re-lay-out first: the pane the first click opened pushes every
        // chart below it down, so a point captured before it existed no
        // longer names the same chart.
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        let (second, at_second) = panel.update(cx, |panel, _cx| {
            let second = chart_index(panel, "I$ misses by function");
            (second, point_of(panel, second, 3.0))
        });
        cx.simulate_click(at_second, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            let selected = panel
                .frame_inspect
                .as_ref()
                .expect("the pane moves, it does not close");
            assert_eq!(selected.chart, second);
            assert_eq!(selected.frame, 3);
        });
    }

    /// A frame the profiler recorded nothing for still opens the pane --
    /// with the empty state, read from the view model the renderer reads
    /// rather than from a literal at the call site (R3's F2: a test that
    /// compares two constants proves nothing about what was painted).
    #[gpui::test]
    async fn test_a_frame_with_no_per_function_data_shows_the_empty_state(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let at = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 4.0)
        });
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            let selected = panel.frame_inspect.as_ref().expect("still a selection");
            assert_eq!(selected.frame, 4);
            assert!(selected.groups().is_empty());
            assert_eq!(
                selected.empty_state(),
                Some(inspect::NO_FRAME_PROFILE),
                "the pane says what is true of the frame, and claims no cause"
            );
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.debug_bounds(FRAME_INSPECT_SELECTOR).is_some(),
            "an empty frame gets the pane and its reason, never a blank"
        );
    }

    /// Only the four selectable charts are clickable. A click on the
    /// wire chart must leave the pane alone rather than opening an I$
    /// inspector about a frame the user was reading wire cycles on.
    #[gpui::test]
    async fn test_a_click_on_an_unselectable_chart_selects_nothing(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let at = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Wire cycles per frame vs budget");
            assert!(!panel.chart_specs()[ix].selectable);
            point_of(panel, ix, 3.0)
        });
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| assert!(panel.frame_inspect.is_none()));
    }

    /// A frame number means nothing in another run's chart set, so
    /// switching runs must drop the selection rather than carry it over.
    ///
    /// Both ways out of a run, because they are two different code paths:
    /// picking another run (`select_run` -> `begin_selection`) and going
    /// Back (`clear_selection`). Run 2's frames are 10..=12, so a
    /// selection that survived would be pointing at a frame that run does
    /// not have.
    #[gpui::test]
    async fn test_leaving_a_run_drops_the_frame_selection(cx: &mut TestAppContext) {
        let (_dir, db_path, panel, cx) = drawn_detail_window(cx).await;
        seed_second_run(&db_path);
        let at = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 3.0)
        });

        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.frame_inspect.as_ref().unwrap().frame, 3);
        });
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 2,
                    started_at: "2026-08-03T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena-2".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.frame_inspect.is_none(),
                "run 2 has no frame 3 -- the selection cannot follow the switch"
            );
            assert_eq!(
                panel.chart_specs()[0].x,
                vec![10.0, 11.0, 12.0],
                "and it really is the other run that is showing"
            );
        });

        // ...and the Back path, from a fresh selection on run 2.
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        let at = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 11.0)
        });
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.frame_inspect.as_ref().unwrap().frame, 11);
        });
        panel.update(cx, |panel, cx| panel.clear_selection(cx));
        panel.update(cx, |panel, _cx| assert!(panel.frame_inspect.is_none()));
    }

    /// The sort header, clicked for real: the table's order flips, and it
    /// flips because the panel asked worldlib for the other direction --
    /// not because anything here reversed a vector.
    #[gpui::test]
    async fn test_the_profile_sort_header_toggles_the_tables_order(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let order = |panel: &ChartsPanel| -> Vec<String> {
            let Some(DetailState::Ready(detail)) = &panel.detail else {
                panic!("ready");
            };
            detail
                .profiles
                .table()
                .rows(panel.profile_sort_ascending)
                .iter()
                .map(|a| a.func.clone())
                .collect()
        };
        let descending = panel.update(cx, |panel, _cx| {
            assert!(!panel.profile_sort_ascending, "descending is the default");
            order(panel)
        });
        assert_eq!(
            descending,
            vec!["blit", "update", "update_entities", "render"],
            "biggest offender first, frame 0's burst excluded"
        );

        let header = cx
            .debug_bounds(PROFILE_SORT_SELECTOR)
            .expect("the sortable header must be painted");
        cx.simulate_click(header.center(), gpui::Modifiers::default());

        let ascending = panel.update(cx, |panel, _cx| {
            assert!(panel.profile_sort_ascending, "the header click flipped it");
            order(panel)
        });
        assert_eq!(
            ascending,
            descending.iter().rev().cloned().collect::<Vec<_>>(),
            "the two directions are exact inverses -- worldlib's, not ours"
        );

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        let header = cx.debug_bounds(PROFILE_SORT_SELECTOR).unwrap();
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert!(!panel.profile_sort_ascending);
            assert_eq!(order(panel), descending, "toggling twice round-trips");
        });
    }

    /// The profile table is painted for a run that HAS profile rows...
    #[gpui::test]
    async fn test_the_profile_table_is_painted_below_the_charts(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let table = cx
            .debug_bounds(PROFILE_TABLE_SELECTOR)
            .expect("the I$ profile table must be painted");
        let last_chart = panel.update(cx, |panel, _cx| {
            let last = panel.chart_specs().len() - 1;
            panel.chart_bounds.borrow()[last].expect("laid out")
        });
        assert!(
            table.origin.y > last_chart.origin.y,
            "ggo-ide puts the profile section last, under the charts"
        );
    }

    /// ...and for one that has none, carrying the reason instead of
    /// silently disappearing -- which is how a reader learns the data
    /// exists at all. A frameless run has no charts either, so this also
    /// covers the branch where the table is the only thing left.
    #[gpui::test]
    async fn test_a_run_without_profile_rows_still_paints_the_table_and_its_reason(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_without_frames(&db_path);

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
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            let Some(DetailState::Ready(detail)) = &panel.detail else {
                panic!("ready");
            };
            assert!(panel.chart_specs().is_empty(), "the frameless branch");
            assert_eq!(
                detail.profiles.table_empty_state(),
                Some(inspect::NO_PROFILE_DATA)
            );
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(1200.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.debug_bounds(PROFILE_TABLE_SELECTOR).is_some(),
            "a run with no profile rows still gets the heading and the reason"
        );
        assert!(
            cx.debug_bounds(PROFILE_SORT_SELECTOR).is_none(),
            "with no rows there is nothing to sort"
        );
    }

    /// Inspect is perf-only. A device run comes out of the OTHER id space
    /// (`runs`, TEXT ids, cloned from `diag.db`), has no frames, no
    /// charts and no profile, and sets `device_log` instead of `detail` --
    /// so the click path has nothing to reach even if something called
    /// it. Asserted both ways: the entry point refuses, and neither
    /// profile surface is painted.
    #[gpui::test]
    async fn test_inspect_is_unreachable_from_a_device_selection(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let diag_path = dir.path().join("diag.db");
        seed_run_with_samples(&db_path, 4);
        seed_profile_rows(&db_path);
        seed_device_run(&diag_path, "dev-1", "2026-08-02T00:00:00Z", "pass");

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path);
            panel.diag_db_path_override = Some(diag_path);
            panel
        });
        // Show a perf run first, so the failure mode this guards against
        // -- a device selection still able to inspect the run it
        // replaced -- is actually reachable if the guard is missing.
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
        });
        cx.executor().run_until_parked();
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        let at = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 3.0)
        });

        panel.update(cx, |panel, cx| {
            panel.select_device_run(
                RunSummary {
                    id: "dev-1".to_string(),
                    started_at: "2026-08-02T00:00:00Z".to_string(),
                    state: "done".to_string(),
                    verdict: Some("pass".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            assert!(panel.detail.is_none(), "a device run sets device_log only");
            // The same click that selected frame 3 a moment ago.
            panel.select_frame(0, at, cx);
            assert!(
                panel.frame_inspect.is_none(),
                "there is no perf detail to inspect from a device selection"
            );
        });

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(2000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.debug_bounds(PROFILE_TABLE_SELECTOR).is_none(),
            "the device view paints no I$ profile table"
        );
        assert!(
            cx.debug_bounds(FRAME_INSPECT_SELECTOR).is_none(),
            "and no inspect pane"
        );
    }

    // ------------------------------------------- R5: drag-zoom + overlays

    /// A drag across a chart sets that chart's x-domain to what was
    /// dragged over -- and does NOT select a frame, even though the
    /// release lands on one and this chart is frame-selectable.
    #[gpui::test]
    async fn test_a_drag_zooms_the_chart_to_the_dragged_window(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, from, to) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 2.0), point_of(panel, ix, 4.0))
        });
        simulate_drag(cx, from, to);

        panel.update(cx, |panel, _cx| {
            let (lo, hi) = *panel.zoom.get(&ix).expect("the drag must zoom the chart");
            assert!(
                (lo - 2.0).abs() < 0.05 && (hi - 4.0).abs() < 0.05,
                "dragged frames 2..4, got {lo}..{hi}"
            );
            assert!(
                panel.frame_inspect.is_none(),
                "a drag is not a click -- it must not also open the inspect pane"
            );
            assert!(panel.drag.is_none(), "and the preview is released with it");
        });

        // The chart really is showing the window now: its readout at the
        // left edge is the window's first frame, not the run's.
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        panel.update(cx, |panel, cx| {
            let at = point_of(panel, ix, 2.0);
            panel.hover_chart(ix, at, cx);
            let bounds = panel.chart_bounds.borrow()[ix].unwrap();
            let scene = panel
                .scene_for(
                    ix,
                    (f32::from(bounds.size.width), f32::from(bounds.size.height)),
                )
                .unwrap();
            let readout = scene.readout.expect("hovering a sample");
            assert_eq!(readout.title, "frame 2");
        });
    }

    /// Double-clicking a zoomed chart puts it back to the whole run --
    /// and, because the reset consumes the gesture, does not leave a
    /// frame selected behind it.
    #[gpui::test]
    async fn test_a_double_click_restores_the_full_domain(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, from, to) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 1.0), point_of(panel, ix, 3.0))
        });
        simulate_drag(cx, from, to);
        panel.update(cx, |panel, _cx| assert!(panel.zoom.contains_key(&ix)));

        let at = panel.update(cx, |panel, _cx| point_of(panel, ix, 2.0));
        simulate_double_click(cx, at);
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.zoom.is_empty(),
                "a double click restores the full frame domain"
            );
            assert!(
                panel.frame_inspect.is_none(),
                "the reset gesture must not also select a frame"
            );
        });
    }

    /// **The threshold is what makes R4's click-to-inspect survive R5.**
    /// A press and release two pixels apart is a click: it selects the
    /// frame, and it must not zoom to the two-frame minimum window
    /// `zoom_domain` would widen it to.
    #[gpui::test]
    async fn test_a_sub_threshold_drag_selects_a_frame_instead_of_zooming(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let from = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 3.0)
        });
        assert!(
            chart_geom::DRAG_MIN_PX > 2.0,
            "the fixture's 2px jitter has to be below the threshold"
        );
        simulate_drag(cx, from, gpui::point(from.x + px(2.), from.y));

        panel.update(cx, |panel, _cx| {
            assert!(
                panel.zoom.is_empty(),
                "a 2px jitter is a click, not a zoom-to-nothing"
            );
            assert_eq!(
                panel.frame_inspect.as_ref().map(|s| s.frame),
                Some(3),
                "and it selects the frame under the cursor"
            );
        });
    }

    /// R4's concern (1) at the panel level: once a chart is zoomed, a
    /// click on it has to resolve against the domain the user is looking
    /// at. The click is aimed at a pixel that names a DIFFERENT frame
    /// under the full domain, so a hit-test that ignored the zoom would
    /// select the wrong one rather than merely a coincidentally right one.
    #[gpui::test]
    async fn test_a_click_on_a_zoomed_chart_selects_what_is_under_it(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, from, to) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 1.0), point_of(panel, ix, 3.0))
        });
        simulate_drag(cx, from, to);
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        let (at, unzoomed_answer) = panel.update(cx, |panel, _cx| {
            let at = point_of(panel, ix, 3.0);
            let bounds = panel.chart_bounds.borrow()[ix].unwrap();
            let spec = panel.chart_specs()[ix].clone();
            let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let local = (
                f32::from(at.x - bounds.origin.x),
                f32::from(at.y - bounds.origin.y),
            );
            // What the SAME pixel would have named without the zoom.
            (
                at,
                chart_geom::frame_at(&spec, size, local, &chart_geom::ChartView::default()),
            )
        });
        assert_ne!(
            unzoomed_answer,
            Some(3),
            "the fixture must aim at a pixel the two domains disagree about"
        );
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.frame_inspect.as_ref().map(|s| s.frame),
                Some(3),
                "the click follows the zoomed chart, not the original scale"
            );
        });
    }

    /// The zoom is keyed by chart INDEX, which means something only
    /// inside one selection's chart set -- so leaving the run drops it,
    /// exactly as the frame selection does (R4's concern (4)).
    #[gpui::test]
    async fn test_leaving_a_run_drops_the_zoom_and_the_overlay_switch(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, from, to) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 1.0), point_of(panel, ix, 3.0))
        });
        simulate_drag(cx, from, to);
        panel.update(cx, |panel, cx| {
            panel.toggle_historic(cx);
            assert!(panel.zoom.contains_key(&ix) && panel.historic_enabled);
            panel.clear_selection(cx);
            assert!(panel.zoom.is_empty(), "a chart index outlives nothing");
            assert!(
                !panel.historic_enabled,
                "the overlay is about THIS run's history"
            );
        });
    }

    /// The whole overlay path, end to end and through the real db: a
    /// second run of the same cart draws the first one underneath it, at
    /// the ramp's brightest step, with the prior run's OWN values.
    #[gpui::test]
    async fn test_the_overlay_draws_the_prior_run_of_the_same_cart(cx: &mut TestAppContext) {
        let (_dir, db_path, panel, cx) = drawn_detail_window(cx).await;
        seed_second_run(&db_path);
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 2,
                    started_at: "2026-08-03T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena-2".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let Some(DetailState::Ready(detail)) = &panel.detail else {
                panic!("run 2 must be loaded");
            };
            assert_eq!(detail.prior_runs, 1, "run 1 is run 2's only earlier run");
            let wire = &detail.charts[0];
            assert_eq!(wire.title, "Wire cycles per frame vs budget");
            assert_eq!(wire.historic.len(), 1);
            assert_eq!(
                wire.historic[0].opacity,
                ggo_worldlib::charts::reports::historic::HISTORIC_OPACITY[0],
                "the nearest prior run is the brightest step"
            );
            // Run 1's frames are 0..=4 at 164_470 + n*100 and frame 0 is
            // ignored, so the overlay is run 1's frames 1..=4.
            assert_eq!(
                wire.historic[0].values,
                vec![164_570.0, 164_670.0, 164_770.0, 164_870.0]
            );
            // Run 2's own axis is 10..=12 -- three samples against the
            // overlay's four, which is exactly the index-alignment case:
            // the ghost is truncated by the renderer, not by the data.
            assert_eq!(wire.x, vec![10.0, 11.0, 12.0]);
        });
    }

    /// Switching the toggle on paints the ghosts; switching it off stops.
    /// Nothing is re-derived either way -- `guarded_listener` holds the
    /// no-build guard across the toggle handler, so a re-derivation here
    /// fails this test with the tripwire's message.
    #[gpui::test]
    async fn test_the_historic_toggle_paints_the_overlays_it_was_given(cx: &mut TestAppContext) {
        let (_dir, db_path, panel, cx) = drawn_detail_window(cx).await;
        seed_second_run(&db_path);
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 2,
                    started_at: "2026-08-03T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena-2".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();

        let ghosts = |panel: &ChartsPanel| {
            panel
                .scene_for(0, (360.0, 240.0))
                .unwrap()
                .primitives
                .iter()
                .filter(|p| {
                    matches!(
                        p,
                        chart_geom::Primitive::Polyline {
                            color: chart_geom::ChartColor::Historic(_),
                            ..
                        }
                    )
                })
                .count()
        };
        panel.update(cx, |panel, cx| {
            assert_eq!(ghosts(panel), 0, "off by default, as in ggo-ide");
            panel.toggle_historic(cx);
            assert_eq!(ghosts(panel), 1, "one prior run, one ghost");
            panel.toggle_historic(cx);
            assert_eq!(ghosts(panel), 0);
        });
    }

    /// A run with nothing behind it says so, in a sentence that names the
    /// STATE and claims no cause.
    ///
    /// Pinned character for character, which is R4's F3 lesson: the round-1
    /// guard there was a phrase denylist and the reviewer walked a
    /// semantically identical over-claim straight through it. The rule this
    /// string has to satisfy is on `NO_PRIOR_RUNS`; a replacement has to be
    /// defended there, not slipped past a filter here.
    #[gpui::test]
    async fn test_a_run_with_no_earlier_runs_names_that_state_and_no_cause(
        cx: &mut TestAppContext,
    ) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        panel.update(cx, |panel, _cx| {
            let Some(DetailState::Ready(detail)) = &panel.detail else {
                panic!("the fixture run must be loaded");
            };
            assert_eq!(detail.prior_runs, 0, "run 1 is the cart's first");
        });
        assert_eq!(
            NO_PRIOR_RUNS, "no prior runs of this cart to overlay",
            "this sentence states that the overlay set is empty and nothing else. \
             The signal behind it -- load_prior_runs returned no runs -- covers a \
             first run, runs ingested later with higher ids, and a run with no run \
             row at all, so any wording that explains WHY is a claim it cannot \
             support. See NO_PRIOR_RUNS' doc before changing this."
        );
        assert!(
            cx.debug_bounds(HISTORIC_TOGGLE_SELECTOR).is_some(),
            "the toggle row is painted even with nothing to overlay -- that is \
             where the reader learns there is nothing"
        );
    }

    /// The selection band a chart `ix` would paint right now, if any.
    fn drag_band(panel: &ChartsPanel, ix: usize) -> Option<chart_geom::Rect> {
        let bounds = panel.chart_bounds.borrow()[ix].unwrap();
        panel
            .scene_for(
                ix,
                (f32::from(bounds.size.width), f32::from(bounds.size.height)),
            )
            .unwrap()
            .primitives
            .iter()
            .find_map(|p| match p {
                chart_geom::Primitive::Quad {
                    rect,
                    color: chart_geom::ChartColor::Selection,
                } => Some(*rect),
                _ => None,
            })
    }

    /// The drag has to be VISIBLE while it is being aimed -- a zoom
    /// gesture with no feedback is a gesture a user cannot aim.
    ///
    /// **The cursor arrives on the chart before the press**, which is what
    /// a real cursor always does and what round 1's version of this test
    /// omitted. It matters because gpui reports a div with a pending
    /// mouse-down as NOT hovered (`div.rs:3038-3041`), so the first move of
    /// every drag fires `on_hover(false)`; without a prior move the
    /// listener's `was_hovered` is already `false`, the `false -> false`
    /// transition never invokes it, and a mechanism that destroys the drag
    /// on hover-end passes anyway. Round 1 shipped exactly that: a preview
    /// that never painted in the app, with a green test over it. See
    /// `clear_hover`.
    #[gpui::test]
    async fn test_an_in_flight_drag_paints_its_band(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, from, to) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 1.0), point_of(panel, ix, 3.0))
        });

        // Arrive, press, drag -- all through real events.
        cx.simulate_mouse_move(from, None, gpui::Modifiers::default());
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, gpui::Modifiers::default());

        panel.update(cx, |panel, _cx| {
            let rect = drag_band(panel, ix).expect("an in-flight drag paints its band");
            let bounds = panel.chart_bounds.borrow()[ix].unwrap();
            assert!(
                (rect.x - f32::from(from.x - bounds.origin.x)).abs() < 1.0,
                "the band starts where the press did"
            );
            assert!(
                (rect.right() - f32::from(to.x - bounds.origin.x)).abs() < 1.0,
                "and follows the cursor"
            );
            assert!(
                panel.hover.is_some(),
                "the readout survives the drag too -- the hover-end gpui \
                 reports at a press is an artifact, not a departure"
            );
        });

        // Releasing inside consumes the preview along with the gesture.
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert_eq!(drag_band(panel, ix), None);
            assert!(panel.zoom.contains_key(&ix), "and the drag still zoomed");
        });
    }

    /// A release somewhere else never becomes a click, so `resolve_gesture`
    /// never runs and the preview would otherwise hang on screen over a
    /// gesture that has already ended.
    #[gpui::test]
    async fn test_a_release_outside_the_chart_abandons_the_drag(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, from, to) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 1.0), point_of(panel, ix, 3.0))
        });
        cx.simulate_mouse_move(from, None, gpui::Modifiers::default());
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| {
            assert!(drag_band(panel, ix).is_some(), "the drag has a band");
        });

        // Far above the chart -- the detail header, not a canvas.
        cx.simulate_mouse_up(
            gpui::point(from.x, px(4.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        panel.update(cx, |panel, _cx| {
            assert_eq!(drag_band(panel, ix), None, "the band goes with the release");
            assert!(panel.zoom.is_empty(), "and nothing was zoomed");
        });
    }

    /// The checkbox itself, not just the method behind it: a click inside
    /// the painted toggle row switches the overlay on.
    #[gpui::test]
    async fn test_clicking_the_historic_checkbox_switches_the_overlay_on(cx: &mut TestAppContext) {
        let (_dir, db_path, panel, cx) = drawn_detail_window(cx).await;
        seed_second_run(&db_path);
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 2,
                    started_at: "2026-08-03T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("arena-2".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        let row = cx
            .debug_bounds(HISTORIC_TOGGLE_SELECTOR)
            .expect("the toggle row is painted");
        // The checkbox is the row's leading child.
        cx.simulate_click(
            gpui::point(row.origin.x + px(6.), row.origin.y + row.size.height / 2.),
            gpui::Modifiers::default(),
        );
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.historic_enabled,
                "clicking the checkbox switches the overlay on"
            );
        });
    }

    /// Zooming is not limited to the four frame-selectable charts:
    /// `line.rs` gives every line chart a zoom and only some of them an
    /// `on_select`, and so does this. (The click on this chart still
    /// selects nothing -- `test_a_click_on_an_unselectable_chart_selects_
    /// nothing` is the other half.)
    #[gpui::test]
    async fn test_an_unselectable_line_chart_still_zooms(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, from, to) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Wire cycles per frame vs budget");
            assert!(!panel.chart_specs()[ix].selectable);
            (ix, point_of(panel, ix, 1.0), point_of(panel, ix, 3.0))
        });
        simulate_drag(cx, from, to);
        panel.update(cx, |panel, _cx| {
            assert!(panel.zoom.contains_key(&ix), "a line chart zooms");
            assert!(panel.frame_inspect.is_none());
        });
    }

    /// The scene cache does its job: a hover move rebuilds the chart under
    /// the cursor and reuses every other chart's scene.
    ///
    /// The counter is `chart_geom::scene_builds`, and it is what makes the
    /// memoization assertable at all -- both paths return the same scene by
    /// construction, so nothing about the OUTPUT can distinguish them.
    /// Without the cache this is 11 builds instead of 1 (`CachedScene`'s
    /// doc carries the measurement that motivated it).
    #[gpui::test]
    async fn test_a_hover_move_rebuilds_only_the_chart_under_the_cursor(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let charts = panel.update(cx, |panel, _cx| panel.chart_specs().len());
        assert!(charts > 1, "the fixture must have several charts");

        let at = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 2.0)
        });
        cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        let before = chart_geom::scene_builds();
        cx.simulate_mouse_move(
            gpui::point(at.x + px(8.), at.y),
            None,
            gpui::Modifiers::default(),
        );
        let built = chart_geom::scene_builds() - before;
        assert_eq!(
            built, 1,
            "a hover move must rebuild only the hovered chart, not all {charts}"
        );

        // And what the cache hands back is what a fresh build would.
        panel.update(cx, |panel, _cx| {
            for (ix, cached) in panel.scenes.borrow().iter().enumerate() {
                let Some(cached) = cached else { continue };
                assert_eq!(
                    cached.scene,
                    build_chart_scene(&cached.spec, cached.size, &cached.view),
                    "chart {ix}'s cached scene must equal a fresh build of it"
                );
            }
        });
    }

    // ----------------------- W3: the wiring the audit found untested --
    // Every test above that exercises selection, Back, dismissal or
    // Re-run reaches the METHOD directly; these reach each one through
    // the painted element it is wired to, with a real click.

    /// A device run selected and painted, the way [`drawn_detail_window`]
    /// does for a perf run: through `refresh_history`'s real clone and
    /// `select_device_run`'s real off-thread load.
    async fn drawn_device_detail_window(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        gpui::Entity<ChartsPanel>,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let diag_path = dir.path().join("diag.db");
        seed_device_run(&diag_path, "dev-1", "2026-08-02T00:00:00Z", "PASS");

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path);
            panel.diag_db_path_override = Some(diag_path);
            panel
        });
        panel.update(cx, |panel, cx| panel.refresh_history(cx));
        cx.executor().run_until_parked();
        let summary = panel.update(cx, |panel, _cx| history_of(panel).runs[0].clone());
        panel.update(cx, |panel, cx| panel.select_device_run(summary, cx));
        cx.executor().run_until_parked();
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(800.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        (dir, panel, cx)
    }

    /// A real click on a picker row reaches `select_run` for THAT row's
    /// run -- with two runs listed, so a click wired to the wrong row (or
    /// to the whole list) cannot pass by coincidence.
    #[gpui::test]
    async fn test_clicking_a_picker_row_selects_that_run(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);
        seed_second_run(&db_path);

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path);
            panel
        });
        panel.update(cx, |panel, cx| panel.refresh_runs(cx));
        cx.executor().run_until_parked();
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(800.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        // Newest first, so row 0 is run 2 (2026-08-03) and row 1 is run 1
        // -- the literal here is `run_row_selector(1)`, spelled out
        // because `debug_bounds` takes `&'static str`.
        assert_eq!(run_row_selector(1), "ggo-charts-run-1");
        let row = cx
            .debug_bounds("ggo-charts-run-1")
            .expect("the picker paints one selector-bearing row per run");
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            match &panel.selected {
                Some(Selection::Perf(run)) => {
                    assert_eq!(run.id, 1, "row 1 is the older run, run 1")
                }
                _ => panic!("the row click must select a perf run"),
            }
            assert_eq!(
                panel.chart_specs()[0].x,
                vec![1.0, 2.0, 3.0, 4.0],
                "and the detail that loaded is run 1's, not run 2's (10..=12)"
            );
        });
    }

    /// The same, one section down: a real click on a history-rail row
    /// reaches `select_device_run` for that row's device run.
    #[gpui::test]
    async fn test_clicking_a_history_rail_row_selects_that_device_run(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let diag_path = dir.path().join("diag.db");
        seed_device_run(&diag_path, "run-older", "2026-08-01T00:00:00Z", "PASS");
        seed_device_run(&diag_path, "run-newer", "2026-08-02T00:00:00Z", "FAIL");

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(None, cx);
            panel.db_path_override = Some(db_path);
            panel.diag_db_path_override = Some(diag_path);
            panel
        });
        panel.update(cx, |panel, cx| panel.refresh_history(cx));
        cx.executor().run_until_parked();
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(800.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        assert_eq!(device_run_row_selector(1), "ggo-charts-device-run-1");
        let row = cx
            .debug_bounds("ggo-charts-device-run-1")
            .expect("the rail paints one selector-bearing row per device run");
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            match &panel.selected {
                Some(Selection::Device(run)) => {
                    assert_eq!(run.id, "run-older", "row 1 is the older device run")
                }
                _ => panic!("the row click must select a device run"),
            }
            match &panel.device_log {
                Some(DeviceLogState::Ready(lines)) => {
                    assert_eq!(**lines, vec!["==> compile", "RESULT: PASS"]);
                }
                _ => panic!("and its pipeline log must have loaded"),
            }
        });
    }

    /// The Back button on a perf run's detail, clicked for real: back to
    /// the picker, through the same `clear_selection` the method tests
    /// drive directly.
    #[gpui::test]
    async fn test_clicking_back_returns_to_the_picker(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let back = cx
            .debug_bounds(BACK_BUTTON_SELECTOR)
            .expect("the detail header paints its Back button");
        cx.simulate_click(back.center(), gpui::Modifiers::default());

        panel.update(cx, |panel, _cx| {
            assert!(panel.selected.is_none(), "the click clears the selection");
            assert!(panel.detail.is_none());
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(800.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.debug_bounds(HISTORY_RAIL_SELECTOR).is_some(),
            "what paints now is the picker"
        );
        assert!(
            cx.debug_bounds(BACK_BUTTON_SELECTOR).is_none(),
            "and the detail header is gone with the run"
        );
    }

    /// ...and the Back button on a device run's detail, which is a
    /// separate render path (`render_device_detail`) wiring the same
    /// `clear_selection`.
    #[gpui::test]
    async fn test_clicking_back_on_a_device_run_returns_to_the_picker(cx: &mut TestAppContext) {
        let (_dir, panel, cx) = drawn_device_detail_window(cx).await;
        let back = cx
            .debug_bounds(DEVICE_BACK_SELECTOR)
            .expect("the device header paints its Back button");
        cx.simulate_click(back.center(), gpui::Modifiers::default());

        panel.update(cx, |panel, _cx| {
            assert!(panel.selected.is_none());
            assert!(panel.device_log.is_none());
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(800.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(
            cx.debug_bounds(HISTORY_RAIL_SELECTOR).is_some(),
            "what paints now is the picker"
        );
        assert!(cx.debug_bounds(DEVICE_BACK_SELECTOR).is_none());
    }

    /// The inspect pane's Close button -- the one dismissal
    /// `test_clicking_the_selected_frame_again_clears_it` does not cover,
    /// clicked for real.
    #[gpui::test]
    async fn test_clicking_the_inspect_close_button_dismisses_the_pane(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let at = panel.update(cx, |panel, _cx| {
            point_of(panel, chart_index(panel, "Cache misses per frame"), 3.0)
        });
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, _cx| assert!(panel.frame_inspect.is_some()));

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        let close = cx
            .debug_bounds(INSPECT_CLOSE_SELECTOR)
            .expect("the pane paints its Close button");
        cx.simulate_click(close.center(), gpui::Modifiers::default());

        panel.update(cx, |panel, _cx| {
            assert!(
                panel.frame_inspect.is_none(),
                "the Close button dismisses the pane"
            );
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(6000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        assert!(cx.debug_bounds(FRAME_INSPECT_SELECTOR).is_none());
    }

    /// The Re-run entry, clicked for real: the three tests above drive
    /// `rerun_selected` directly, so this is the one that pins the
    /// button's `on_click` to it.
    ///
    /// The panel is its own window root here (the [`drawn_detail_window`]
    /// shape) rather than living in the workspace window `rerun_panel`
    /// builds: dispatching a click into a window whose real root is the
    /// workspace redraws THAT root first, wiping the frame the test drew.
    /// The workspace the Re-run needs still exists -- in its own window,
    /// handed to the panel as the same `WeakEntity` `init` hands over.
    #[gpui::test]
    async fn test_clicking_rerun_routes_to_the_cart_runner(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        seed_run_with_samples(&db_path, 4);

        cx.update(|cx| {
            AppState::test(cx);
            ggo_common::register_cart_runner(cx, recording_cart_runner);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = ChartsPanel::new(Some(workspace.downgrade()), cx);
            panel.db_path_override = Some(db_path);
            panel
        });
        panel.update(cx, |panel, cx| {
            panel.select_run(
                RunListing {
                    id: 1,
                    started_at: "2026-08-01T00:00:00Z".to_string(),
                    cart_name: "demo".to_string(),
                    label: Some("carts/green.cart".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(DEFAULT_WIDTH, px(2000.)),
            |_window, _cx| panel.clone().into_any_element(),
        );

        let button = cx
            .debug_bounds(RERUN_BUTTON_SELECTOR)
            .expect("the detail header paints the Re-run entry");
        cx.simulate_click(button.center(), gpui::Modifiers::default());

        assert_eq!(
            cx.update(|_window, cx| cx.default_global::<Reran>().0.clone()),
            vec!["carts/green.cart".to_string()],
            "the CLICK reaches rerun_selected and hands the cart path on"
        );
        panel.update(cx, |panel, _cx| {
            assert_eq!(panel.rerun_note, None, "a claimed Re-run reports nothing");
        });
    }

    /// A device run has no cart in the perf-run sense, so its header
    /// offers no Re-run entry at all -- and the method behind the entry
    /// refuses a device selection silently rather than picking one of
    /// the perf-side refusal notes.
    #[gpui::test]
    async fn test_rerun_is_absent_from_a_device_run(cx: &mut TestAppContext) {
        let (_dir, panel, cx) = drawn_device_detail_window(cx).await;
        assert!(
            cx.debug_bounds(RERUN_BUTTON_SELECTOR).is_none(),
            "the device header paints no Re-run entry"
        );

        panel.update_in(cx, |panel, window, cx| panel.rerun_selected(window, cx));
        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(panel.selected, Some(Selection::Device(_))),
                "the selection is untouched"
            );
            assert_eq!(
                panel.rerun_note, None,
                "no note either: NO_RERUN_PATH and NO_CART_RUNNER are both \
                 claims about a PERF run, and a device selection supports \
                 neither"
            );
        });
    }

    /// `resolve_gesture`'s doc: a keyboard-dispatched click has no cursor
    /// and must select nothing rather than the hitbox's corner. The
    /// fixture opens the pane on frame 3 first, so a handler that mapped
    /// the keyboard click onto any position at all would show up as the
    /// pane toggling off (same frame), moving (another frame), or a zoom
    /// -- an untouched panel is only reachable by refusing the event.
    #[gpui::test]
    async fn test_a_keyboard_click_selects_nothing(cx: &mut TestAppContext) {
        let (_dir, _db_path, panel, cx) = drawn_detail_window(cx).await;
        let (ix, at) = panel.update(cx, |panel, _cx| {
            let ix = chart_index(panel, "Cache misses per frame");
            (ix, point_of(panel, ix, 3.0))
        });
        cx.simulate_click(at, gpui::Modifiers::default());
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.frame_inspect.as_ref().map(|s| s.frame), Some(3));

            let event = gpui::ClickEvent::Keyboard(gpui::KeyboardClickEvent::default());
            panel.resolve_gesture(ix, true, true, &event, cx);

            assert_eq!(
                panel.frame_inspect.as_ref().map(|s| s.frame),
                Some(3),
                "a keyboard click selects nothing, toggles nothing"
            );
            assert!(panel.zoom.is_empty(), "and zooms nothing");
        });
    }

    /// The three error states, reached through a db that exists but is
    /// not a database, and read via the same named methods the renderer
    /// prints -- `runs_error_message` and friends exist so these
    /// sentences are readable at all (`LogKind::empty_state`'s lesson).
    #[gpui::test]
    async fn test_a_corrupt_db_lands_every_load_in_its_error_state(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        std::fs::write(&db_path, b"this is not a database").unwrap();

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(db_path);
                panel
            })
        });

        panel.update(cx, |panel, cx| panel.refresh_runs(cx));
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            let LoadState::Error(e) = &panel.state else {
                panic!("a corrupt db must land the runs list in Error");
            };
            let message = ChartsPanel::runs_error_message(e);
            assert!(message.contains("Failed to load runs"), "{message}");
            assert!(
                !e.is_empty(),
                "the underlying cause rides along rather than being swallowed"
            );
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
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            let Some(DetailState::Error(e)) = &panel.detail else {
                panic!("a corrupt db must land the run detail in Error");
            };
            let message = ChartsPanel::samples_error_message(e);
            assert!(message.contains("Failed to load samples"), "{message}");
        });

        panel.update(cx, |panel, cx| {
            panel.select_device_run(
                RunSummary {
                    id: "dev-1".to_string(),
                    started_at: "2026-08-02T00:00:00Z".to_string(),
                    state: "done".to_string(),
                    verdict: Some("pass".to_string()),
                },
                cx,
            );
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            let Some(DeviceLogState::Error(e)) = &panel.device_log else {
                panic!("a corrupt db must land the device log in Error");
            };
            let message = ChartsPanel::device_log_error_message(e);
            assert!(message.contains("Failed to load log"), "{message}");
        });
    }

    /// Two rapid refreshes with the db repointed between them: the first
    /// (stale) result must never stomp the second. Two mechanisms defend
    /// this -- replacing `_load_task` cancels the first load, and the
    /// generation guard drops its result if it lands anyway -- and the
    /// iterations run the executor's schedules over both.
    #[gpui::test(iterations = 10)]
    async fn test_a_stale_runs_refresh_does_not_stomp_the_fresh_one(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let first_db = dir.path().join("first.db");
        let second_db = dir.path().join("second.db");
        seed_run_with_samples(&first_db, 4);
        seed_run_with_samples(&second_db, 4);
        seed_second_run(&second_db);

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(first_db);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_runs(cx);
            // Repointed and refreshed again with the first load still in
            // flight -- the rapid double-activation case.
            panel.db_path_override = Some(second_db);
            panel.refresh_runs(cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| match &panel.state {
            LoadState::Ready(runs) => {
                assert_eq!(
                    runs.len(),
                    2,
                    "the second db's two runs, not the first db's one"
                );
                assert_eq!(runs[0].id, 2);
            }
            _ => panic!("expected Ready with the second db's runs"),
        });
    }

    /// The same race on the history rail's own generation counter.
    /// Separate (ide, diag) pairs per refresh, because `history::load`
    /// clones diag rows INTO the ide db -- a shared target would let the
    /// stale clone's rows leak into the fresh listing and mask a stomp.
    #[gpui::test(iterations = 10)]
    async fn test_a_stale_history_refresh_does_not_stomp_the_fresh_one(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let first_ide = dir.path().join("first_ide.db");
        let first_diag = dir.path().join("first_diag.db");
        let second_ide = dir.path().join("second_ide.db");
        let second_diag = dir.path().join("second_diag.db");
        seed_device_run(&first_diag, "run-a", "2026-08-01T00:00:00Z", "PASS");
        seed_device_run(&second_diag, "run-b", "2026-08-02T00:00:00Z", "FAIL");

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ChartsPanel::new(None, cx);
                panel.db_path_override = Some(first_ide);
                panel.diag_db_path_override = Some(first_diag);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_history(cx);
            panel.db_path_override = Some(second_ide);
            panel.diag_db_path_override = Some(second_diag);
            panel.refresh_history(cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let history = history_of(panel);
            let ids: Vec<&str> = history.runs.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["run-b"],
                "the second pair's run, not the stale first's"
            );
        });
    }

    /// R3's F1 said the tripwire covered `select_run`'s update closure
    /// and `Render::render` and nothing else, and named event listeners
    /// as the gap R4's click-to-inspect would walk straight through.
    /// `guarded_listener` is that gap closed, and this is the drill that
    /// proves it: a listener that derives fails, rather than quietly
    /// re-running a 327 ms pass on every click.
    #[gpui::test]
    #[should_panic(expected = "built on the UI thread")]
    async fn test_a_listener_that_derives_trips_the_guard(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        let (panel, cx) = cx.add_window_view(|_window, cx| ChartsPanel::new(None, cx));
        let listener = panel.update(cx, |_panel, cx| {
            ChartsPanel::guarded_listener(cx, |_this, _event: &bool, _window, _cx| {
                let _ = detail::build(&loader::RunSamples::default(), &[]);
            })
        });
        cx.update_window(cx.window_handle(), |_, window, cx| {
            listener(&true, window, cx);
        })
        .unwrap();
    }
}
