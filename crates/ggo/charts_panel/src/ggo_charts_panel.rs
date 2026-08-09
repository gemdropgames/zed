//! GGO Charts panel (F3 Task C1): right-dock panel listing perf runs read
//! straight out of `~/.ggo/ggo_ide.db` -- the native replacement for the
//! Tauri IDE's Reports page's run picker (`RunPage.tsx`/`ReportsPage.tsx`),
//! reading the SAME database file rather than a copy (see `loader`'s doc).
//!
//! This task is the SKELETON: the picker lists every run (id/date/cart),
//! off-thread, with a load-generation guard against a stale result -- the
//! same shape `ggo_world_panel`/`ggo_metasprite_panel` use for their own
//! off-thread loads. C2 adds the actual chart rendering (line/histogram/
//! stacked) once a run is selected; this task's `Ready` state just proves
//! the list loaded, nothing is selectable yet.
//!
//! Structural mirror of `ggo_world_panel`/`ggo_metasprite_panel`: `Panel`
//! impl, `ToggleFocus`, `observe_new` registration into every new
//! workspace, and a `KeymapEventChannel` observer scaffold (no panel
//! keybinds exist yet -- see `bind_panel_keys`'s doc for why the observer
//! is still wired up now rather than added later).

mod loader;

use std::path::PathBuf;

use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels, Render,
    Styled, Task, Window, actions, div, px,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

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
/// it yet (see `bind_panel_keys`), but it exists now so C2's chart
/// zoom/pan/select keys have a context to land in without touching this
/// module's `init`/`Render` wiring.
const KEY_CONTEXT: &str = "GgoChartsPanel";

/// Fixed default width until the panel grows real settings persistence
/// (same call `ggo_world_panel`/`ggo_metasprite_panel` made at their own
/// skeleton stage).
const DEFAULT_WIDTH: Pixels = px(360.);

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as `ggo_world_panel::init`/`ggo_metasprite_panel::init`:
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

/// No panel-specific keybinds exist yet -- the runs list has nothing to
/// navigate with a keyboard yet (C2's chart interactivity is where real
/// bindings land). Kept as its own fn (rather than inlined into `init`)
/// so it matches `ggo_world_panel`/`ggo_metasprite_panel`'s shape
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

pub struct ChartsPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    /// Test hook: bypass `~/.ggo` resolution and point straight at a
    /// fixture db (`ggo_world_panel::root_override`'s analog).
    db_path_override: Option<PathBuf>,
    state: LoadState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
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
        }
    }

    /// Kick off the off-thread run listing. Runs on every panel
    /// activation (`set_active`), same trigger `ggo_world_panel::
    /// refresh_worlds` uses -- cheap enough (one `SELECT` against a
    /// small local db) to just re-run rather than cache indefinitely, so
    /// a run ingested by `ggo-emu`/`ggo-server` while the panel sits open
    /// shows up the next time it's focused.
    fn refresh_runs(&mut self, cx: &mut Context<Self>) {
        let Some(db_path) = self
            .db_path_override
            .clone()
            .or_else(loader::default_db_path)
        else {
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
                    // `ggo_world_panel::select_world` uses.
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
                    h_flex()
                        .id(("ggo-charts-run", ix))
                        .w_full()
                        .justify_between()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .child(Label::new(run.display_title()))
                        .child(Label::new(run.started_at.clone()).color(Color::Muted))
                }))
                .into_any_element(),
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
        // Same call as `ggo_world_panel`/`ggo_metasprite_panel`: no
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
        // `ggo_world_panel` took 8, `ggo_metasprite_panel` took 9 (grep
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
    /// with `refresh_worlds`/`select_world`.
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
}
