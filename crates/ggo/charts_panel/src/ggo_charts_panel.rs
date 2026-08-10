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
// `pub`: `ggo_emu_panel`'s ingest round-trip test reads its own writes back
// through THESE query functions (as a dev-dependency), which is what proves
// the two panels agree on `ggo_ide.db`'s schema rather than each having its
// own idea of it.
pub mod loader;
mod report;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    Action, App, Bounds, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseMoveEvent, Pixels, Render, Styled, Task, Window, actions, div, px,
};
use ui::Tooltip;
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use chart_geom::{ChartSpec, build_chart_scene};
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

        let panel = cx.new(|cx| ChartsPanel::new(cx));
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
/// run kicks off a second background pass for its samples.
enum DetailState {
    Loading,
    /// Charts already assembled from the loaded samples (`chart_set`), so
    /// the assembly happens once per selection rather than once per
    /// render. Empty when the run had no plottable frames.
    ///
    /// `Rc`, not a bare `ChartSpec`: `render_chart`'s prepaint closure is
    /// `'static` and so has to OWN whatever it reads, and a spec holds one
    /// `Vec<f32>` per series over the whole run -- deep-cloning all
    /// thirteen charts' worth on every render (i.e. on every hover
    /// mouse-move) is ~8 MB of memcpy per frame at ingest's 100,000-frame
    /// cap. Sharing the `Rc` makes that a refcount bump.
    Ready {
        charts: Vec<Rc<ChartSpec>>,
        /// How many frames the default ignore filter dropped, so the
        /// header can say so -- see `chart_set::ignored_count`.
        ignored: usize,
        /// The non-chart half of the run detail: KPI tiles, the header's
        /// run-config line, and the two diagnostic tables. Assembled once
        /// per selection alongside `charts`, for the same reason -- and
        /// over the same ignore-filtered frames, which is what keeps a
        /// tile and the chart under it talking about one frame set.
        report: report::RunReport,
    },
    Error(String),
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
    /// Test hook: bypass `~/.ggo` resolution and point straight at a
    /// fixture db (`ggo_world_panel::root_override`'s analog).
    db_path_override: Option<PathBuf>,
    state: LoadState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    /// The run whose charts are showing, `None` for the picker view.
    selected: Option<RunListing>,
    detail: Option<DetailState>,
    /// Separate from `load_generation`: a run selection and a runs-list
    /// refresh are independent loads, and a stale one of either must not
    /// clobber the other's result.
    detail_generation: u64,
    _detail_task: Option<Task<()>>,
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
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            db_path_override: None,
            state: LoadState::Empty,
            load_generation: 0,
            _load_task: None,
            selected: None,
            detail: None,
            detail_generation: 0,
            _detail_task: None,
            hover: None,
            chart_bounds: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn db_path(&self) -> Option<PathBuf> {
        self.db_path_override
            .clone()
            .or_else(ggo_common::default_db_path)
    }

    /// Select a run and kick off its sample load -- same off-thread
    /// shape and same load-generation staleness guard as
    /// [`Self::refresh_runs`], on its own generation counter.
    fn select_run(&mut self, run: RunListing, cx: &mut Context<Self>) {
        let Some(db_path) = self.db_path() else {
            self.detail = Some(DetailState::Error(
                "could not resolve a home directory".to_string(),
            ));
            cx.notify();
            return;
        };
        let run_id = run.id;
        self.selected = Some(run);
        self.hover = None;
        self.chart_bounds.borrow_mut().clear();
        self.detail_generation += 1;
        let generation = self.detail_generation;
        self.detail = Some(DetailState::Loading);
        cx.notify();

        let load = cx.background_spawn(async move { loader::load_run_samples(&db_path, run_id) });
        self._detail_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.detail_generation != generation {
                    return;
                }
                this.detail = Some(match result {
                    Ok(samples) => DetailState::Ready {
                        ignored: chart_set::ignored_count(&samples.frames),
                        report: report::build(&samples),
                        charts: chart_set::build_charts(&samples)
                            .into_iter()
                            .map(Rc::new)
                            .collect(),
                    },
                    Err(e) => DetailState::Error(e),
                });
                cx.notify();
            })
            .ok();
        }));
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
        // Bump the generation so an in-flight sample load for the run
        // being dismissed can't land afterwards and re-show its charts.
        self.detail_generation += 1;
        self.selected = None;
        self.detail = None;
        self.hover = None;
        self.chart_bounds.borrow_mut().clear();
        cx.notify();
    }

    /// The currently-assembled chart specs, or `&[]` in any other state.
    /// Test-only: `render_detail` matches on `self.detail` directly (it
    /// needs the other arms' messages anyway), so this exists purely so a
    /// test can assert the chart set and build scenes without a window.
    #[cfg(test)]
    fn chart_specs(&self) -> &[Rc<ChartSpec>] {
        match &self.detail {
            Some(DetailState::Ready { charts, .. }) => charts,
            _ => &[],
        }
    }

    /// The assembled non-chart report, or `None` in any other state.
    /// Test-only, for the same reason [`Self::chart_specs`] is: the render
    /// path matches on `self.detail` directly.
    #[cfg(test)]
    fn report(&self) -> Option<&report::RunReport> {
        match &self.detail {
            Some(DetailState::Ready { report, .. }) => Some(report),
            _ => None,
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

    fn render_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.selected.is_some() {
            return self.render_detail(cx);
        }
        match &self.state {
            LoadState::Empty => self.render_message("Select the panel to load runs", cx),
            LoadState::Loading => self.render_message("Loading runs…", cx),
            LoadState::Error(e) => self.render_message(format!("Failed to load runs: {e}"), cx),
            LoadState::Ready(runs) if runs.is_empty() => {
                self.render_message("No perf runs recorded yet", cx)
            }
            LoadState::Ready(runs) => v_flex()
                .id("ggo-charts-runs-list")
                .size_full()
                .overflow_y_scroll()
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
        }
    }

    /// The selected run's view: a back row, then one titled canvas per
    /// chart in [`chart_set::build_charts`]'s order.
    fn render_detail(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let title = self
            .selected
            .as_ref()
            .map(RunListing::display_title)
            .unwrap_or_default();

        let body = match &self.detail {
            None | Some(DetailState::Loading) => self.render_message("Loading samples…", cx),
            Some(DetailState::Error(e)) => {
                self.render_message(format!("Failed to load samples: {e}"), cx)
            }
            Some(DetailState::Ready { charts, report, .. }) => {
                self.chart_bounds.borrow_mut().resize(charts.len(), None);
                // ggo-ide's `detail_view` order, top to bottom: the two
                // diagnostic tables, then the KPI row, then the charts.
                // The tables come FIRST there and here for a reason worth
                // not "tidying" away: the run most in need of them is a
                // cart that panicked before it ever reached vsync_wait,
                // which has no frames, no KPIs and no charts at all --
                // burying the panic under the empty-charts message would
                // hide the only thing that run has to say.
                v_flex()
                    .id("ggo-charts-list")
                    .size_full()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_3()
                    .child(self.render_failures_table(&report.diagnostics, cx))
                    .child(self.render_panics_table(&report.diagnostics, cx))
                    .children(if charts.is_empty() {
                        // An explicit message, never a blank canvas: a run
                        // with no frames and a run whose only frame is the
                        // ignored frame 0 both land here. No KPI tiles
                        // either -- every one of them would read 0 (or
                        // 100% hit rate) for a run that measured nothing,
                        // and ggo-ide skips `kpi_row` in this case too.
                        vec![
                            Label::new(
                                "No frames recorded for this run (cart never reached vsync_wait).",
                            )
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
            Some(DetailState::Ready { report, .. }) => report.config_line.clone(),
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
                            .child(Label::new(title).size(LabelSize::Small))
                            .children(self.ignored_caption()),
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

    /// The KPI tile row above the plots -- ggo-ide's `kpi_row`, wrapped
    /// instead of `chunks(KPI_TILES_PER_ROW)`'d: that page picks a fixed
    /// tiles-per-row because it owns its window width, while this panel is
    /// a user-resizable dock, so the row reflows.
    fn render_kpi_row(&self, tiles: &[report::KpiTile], cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .flex_wrap()
            .gap_2()
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
        Self::table("Panics", diagnostics.empty_state(rows.len()), cx)
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
        empty_state: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        v_flex()
            .w_full()
            .gap_1()
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
        let Some(DetailState::Ready { ignored, .. }) = &self.detail else {
            return None;
        };
        let n = *ignored;
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
        spec: &Rc<ChartSpec>,
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
                this.update(cx, |this, cx| this.refresh_runs(cx)).ok();
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
                let mut panel = ChartsPanel::new(cx);
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
                let mut panel = ChartsPanel::new(cx);
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
                matches!(&panel.detail, Some(DetailState::Ready { ignored, .. }) if *ignored == 1),
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
                matches!(&panel.detail, Some(DetailState::Ready { charts, .. }) if charts.is_empty()),
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
                let mut panel = ChartsPanel::new(cx);
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
    /// recorded clean UART says "none recorded"; a run with no UART at all
    /// says it predates UART capture, because "nothing failed" is a claim
    /// its data cannot support.
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

        // `seed_run_with_samples` writes no UART at all.
        let (_dir, silent) = ready_detail_panel(cx, 4).await;
        silent.update(cx, |panel, _cx| {
            let diagnostics = &panel.report().unwrap().diagnostics;
            assert_eq!(*diagnostics, report::Diagnostics::NoUart);
            assert_eq!(diagnostics.empty_state(0), Some(report::NO_UART));
        });

        assert_ne!(report::NONE_RECORDED, report::NO_UART);
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
                let mut panel = ChartsPanel::new(cx);
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
}
