//! The right-dock panel for worlds (spec
//! `docs/superpowers/specs/2026-09-06-live-world-view-v2-design.md`).
//! Thin on purpose: the document, the editing state and the viewer
//! emulator all live on the [`WorldPanel`] behind each center-pane
//! [`WorldCanvasItem`]; this dock only shows the active tab's, and is the
//! one place a world gets opened from.

use gpui::{
    Action, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Subscription, Task, WeakEntity, Window,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::world_canvas_item::WorldCanvasItem;
use crate::{DEFAULT_WIDTH, EMPTY_MESSAGE, GGO_WORLD_PANEL_KEY, ToggleFocus, WorldPanel};

pub struct WorldDock {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: Option<WeakEntity<WorldPanel>>,
    /// Every panel this dock has opened a tab for, in open order, each
    /// under the path it was opened for. Weak: the tab owns its panel, so
    /// a closed tab drops out of the list by itself.
    ///
    /// Kept here rather than read back off the workspace for two reasons:
    /// `open_world` runs while the workspace is LEASED (it is called from
    /// inside `open_in_panel`) and reading a leased entity panics, and the
    /// path a panel is LOADING is not yet its `open_rel_path_now`, so a
    /// second click arriving mid-load would otherwise open a second tab on
    /// the same world.
    panels: Vec<(String, WeakEntity<WorldPanel>)>,
    _active_item: Option<Subscription>,
    #[cfg(test)]
    test_root_override: Option<std::path::PathBuf>,
}

impl WorldDock {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let _active_item = workspace.upgrade().map(|workspace| {
            cx.subscribe(&workspace, |_this, _workspace, event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    // Deferred: the event is emitted from inside the
                    // workspace's own update, which `refresh_active` reads.
                    let this = cx.weak_entity();
                    cx.defer(move |cx| {
                        this.update(cx, |this, cx| this.refresh_active(cx)).ok();
                    });
                }
            })
        });
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            active: None,
            panels: Vec::new(),
            _active_item,
            #[cfg(test)]
            test_root_override: None,
        }
    }

    /// The panel behind the active world tab, if the active item is one.
    pub fn active(&self) -> Option<Entity<WorldPanel>> {
        self.active.as_ref().and_then(WeakEntity::upgrade)
    }

    /// Every open world panel (one per [`WorldCanvasItem`] in any pane),
    /// in the order the worlds were opened.
    pub fn open_panels(&self) -> Vec<Entity<WorldPanel>> {
        self.panels
            .iter()
            .filter_map(|(_, panel)| panel.upgrade())
            .collect()
    }

    fn refresh_active(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let active = workspace
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.downcast::<WorldCanvasItem>())
            .map(|item| item.read(cx).panel().downgrade());
        // A non-world tab keeps the last world in the dock: the user is
        // likely editing that world's toml or sprite beside it. A closed
        // tab is the exception -- its panel is gone, so `active()` is
        // already `None` and the dock has to let go of the dead handle.
        if active.is_some() || self.active().is_none() {
            self.active = active;
            cx.notify();
        }
    }

    /// Activate the tab already showing `rel`, or open a new one. Returns
    /// the tab's panel. Safe to call while the workspace is leased.
    pub fn open_world(
        &mut self,
        rel: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<WorldPanel>> {
        let workspace = self.workspace.upgrade()?;
        let existing = self
            .panels
            .iter()
            .find(|(open_rel, _)| open_rel == rel)
            .and_then(|(_, panel)| panel.upgrade());
        // The mode and mask are sticky across worlds: a new tab opens the
        // way the last one was showing.
        let (canvas_mode, live_sys_mask) = self
            .active()
            .map(|panel| {
                let panel = panel.read(cx);
                (panel.canvas_mode, panel.live_sys_mask)
            })
            .unwrap_or((crate::CanvasMode::Live, 0));
        let panel = match existing {
            Some(panel) => panel,
            None => {
                let weak_workspace = self.workspace.clone();
                #[cfg(test)]
                let root_override = self.test_root_override.clone();
                let panel = cx.new(|cx| {
                    let mut panel = WorldPanel::new(Some(weak_workspace), cx);
                    panel.canvas_mode = canvas_mode;
                    panel.live_sys_mask = live_sys_mask;
                    #[cfg(test)]
                    {
                        panel.root_override = root_override;
                    }
                    panel.mark_loading(rel, cx);
                    panel
                });
                self.panels.retain(|(_, panel)| panel.upgrade().is_some());
                self.panels.push((rel.to_string(), panel.downgrade()));
                panel
            }
        };
        self.active = Some(panel.downgrade());
        let rel = rel.to_string();
        let item_panel = panel.clone();
        // The workspace is leased by whoever called us (the explorer
        // interceptor, a menu entry, the MCP host): tabs and the load
        // wait for the lease to end.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                let existing = workspace
                    .items_of_type::<WorldCanvasItem>(cx)
                    .find(|item| item.read(cx).panel() == &item_panel);
                match existing {
                    Some(item) => {
                        workspace.activate_item(&item, true, true, window, cx);
                    }
                    None => {
                        let item = cx.new(|cx| WorldCanvasItem::new(item_panel.clone(), cx));
                        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
                    }
                }
            });
            item_panel.update(cx, |panel, cx| {
                if panel.open_rel_path_now() != Some(rel.as_str()) {
                    panel.open_rel_path(&rel, window, cx);
                }
            });
        });
        cx.notify();
        Some(panel)
    }

    /// Read the panels this dock opens from `root` instead of the
    /// workspace's first visible worktree, the way `WorldPanel`'s own
    /// `root_override` does.
    #[cfg(test)]
    pub(crate) fn test_root_override(&mut self, root: std::path::PathBuf) {
        self.test_root_override = Some(root);
    }
}

impl Render for WorldDock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.active() {
            Some(panel) => panel.into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new(EMPTY_MESSAGE).color(Color::Muted))
                .into_any_element(),
        };
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(body)
    }
}

impl Focusable for WorldDock {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The dock's own handle only stands in while no world is open:
        // the panel's is what the world key bindings are scoped to.
        match self.active() {
            Some(panel) => panel.read(cx).focus_handle.clone(),
            None => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<PanelEvent> for WorldDock {}

impl Panel for WorldDock {
    fn persistent_name() -> &'static str {
        "GGO World"
    }

    fn panel_key() -> &'static str {
        GGO_WORLD_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // No settings persistence yet (see task-5-brief.md); Bottom isn't a
        // sensible spot for a world/map editor sidebar, so only Left/Right.
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
        Some(IconName::Public)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO World")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    /// Nothing to guard here: every open world is a tab, and a dirty tab
    /// is prompted for by the workspace's own item close flow
    /// (`WorldCanvasItem::can_save`). Prompting here as well would ask
    /// twice for the same document.
    fn prepare_to_close(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Task<bool> {
        Task::ready(true)
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active && let Some(panel) = self.active() {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_worlds` needs to READ the
            // workspace to find the project root -- reading it re-entrantly
            // panics.
            cx.defer(move |cx| {
                panel.update(cx, |panel, cx| panel.refresh_worlds(cx));
            });
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    /// A workspace with the dock registered and a real fixture root the
    /// panels read through `root_override`.
    pub(crate) async fn dock_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        Entity<Workspace>,
        Entity<WorldDock>,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        crate::tests::write_fixture(root);
        // A second world beside the fixture's `worlds/test.toml`.
        std::fs::copy(
            root.join("worlds/test.toml"),
            root.join("worlds/other.toml"),
        )
        .unwrap();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({ "worlds": { "test.toml": "", "other.toml": "" } }),
        )
        .await;
        let project = Project::test(fs, ["/proj".as_ref()], cx).await;
        let (multi, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |mw, _| mw.workspace().clone());
        let dock = workspace.read_with(cx, |ws, cx| ws.panel::<WorldDock>(cx).expect("registered"));
        dock.update(cx, |dock, _| dock.test_root_override(root.to_path_buf()));
        (workspace, dock, cx)
    }

    #[gpui::test]
    async fn two_worlds_open_as_two_tabs_with_two_panels(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        let first = workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/other.toml", window, cx);
            })
        });
        cx.run_until_parked();

        let panels = dock.read_with(cx, |dock, _| dock.open_panels());
        assert_eq!(panels.len(), 2, "one panel per tab");
        let rels: Vec<_> = panels
            .iter()
            .map(|p| p.read_with(cx, |p, _| p.open_rel_path_now().map(str::to_string)))
            .collect();
        assert_eq!(
            rels,
            [
                Some("worlds/test.toml".into()),
                Some("worlds/other.toml".into())
            ]
        );
        assert!(
            panels
                .iter()
                .all(|p| p.read_with(cx, |p, _| p.test_is_ready()))
        );
        assert!(first, "the first open claimed the dock");

        // The dock follows the active tab: the second world is active now.
        let active = dock.read_with(cx, |dock, _| dock.active().expect("an active world"));
        assert_eq!(
            active.read_with(cx, |p, _| p.open_rel_path_now().map(str::to_string)),
            Some("worlds/other.toml".into())
        );

        // Re-opening the first world activates its tab instead of a third.
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(dock.read_with(cx, |dock, _| dock.open_panels().len()), 2);
        let active = dock.read_with(cx, |dock, _| dock.active().expect("an active world"));
        assert_eq!(
            active.read_with(cx, |p, _| p.open_rel_path_now().map(str::to_string)),
            Some("worlds/test.toml".into())
        );
    }

    #[gpui::test]
    async fn closing_a_tab_drops_its_panel_and_stops_its_viewer(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("open"));
        let endpoint = ggo_common::LinkEndpoint::new();
        panel.update(cx, |panel, _| {
            panel.test_set_live_endpoint(endpoint.clone())
        });
        let weak = panel.downgrade();
        drop(panel);

        // Only the id: a strong handle to the tab would keep the panel
        // alive by itself and prove nothing.
        let item_id = workspace.read_with(cx, |ws, cx| {
            ws.items_of_type::<crate::WorldCanvasItem>(cx)
                .next()
                .expect("tab")
                .entity_id()
        });
        let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(item_id, workspace::SaveIntent::Skip, window, cx)
        })
        .await
        .expect("close");
        cx.run_until_parked();

        assert!(weak.upgrade().is_none(), "the tab owned the panel");
        assert!(
            endpoint.stop_requested(),
            "the panel's release stopped its viewer"
        );
        assert!(dock.read_with(cx, |dock, _| dock.active().is_none()));
    }

    #[gpui::test]
    async fn the_dock_renders_the_empty_message_without_a_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        dock.update_in(cx, |dock, window, cx| {
            let _ = dock.render(window, cx);
            assert!(dock.active().is_none());
        });
    }
}
