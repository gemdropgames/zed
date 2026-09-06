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
use crate::{
    CanvasMode, DEFAULT_WIDTH, EMPTY_MESSAGE, GGO_WORLD_PANEL_KEY, ToggleFocus, WorldPanel,
};

/// The empty dock's `debug_selector`: the message a dock with no world
/// tab shows, which is otherwise indistinguishable from a blank panel.
const EMPTY_SELECTOR: &str = "ggo-world-dock-empty";

/// Which renderer a freshly opened world tab comes up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// However the user last left a world tab -- what a click in the
    /// explorer, the Emerald menu or the world menu means.
    Sticky,
    /// Design, whatever the user last chose. For opens nobody is looking
    /// at: an agent surveying a project's worlds must not boot an
    /// emulator per world it reads.
    Design,
}

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
    /// The Live choices a new [`OpenMode::Sticky`] tab inherits, pushed
    /// here by whichever panel the user last changed them on. Held by the
    /// DOCK (spec) rather than read back off the active panel, so closing
    /// the last world tab does not reset them.
    canvas_mode: CanvasMode,
    live_sys_mask: u64,
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
            canvas_mode: CanvasMode::Live,
            live_sys_mask: 0,
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
            let moved = active != self.active;
            self.active = active;
            // Only on a real move, and deferred. Only, because
            // `refresh_worlds` notifies the panel, the canvas tab turns
            // that into a `ChangeItemTitle`, and the workspace turns THAT
            // back into the `ActiveItemChanged` this runs behind -- an
            // unconditional refresh here never stops. Deferred, because
            // `refresh_worlds` READS the workspace, which is mid-update
            // when that event is emitted.
            if moved && let Some(panel) = self.active() {
                cx.defer(move |cx| {
                    panel.update(cx, |panel, cx| panel.refresh_worlds(cx));
                });
            }
            cx.notify();
        }
    }

    /// Record the Live choices a new [`OpenMode::Sticky`] tab inherits.
    /// Called by whichever panel the user changed them on.
    pub(crate) fn note_sticky(&mut self, canvas_mode: CanvasMode, live_sys_mask: u64) {
        self.canvas_mode = canvas_mode;
        self.live_sys_mask = live_sys_mask;
    }

    /// [`Self::open_world_in`] in the mode the user last chose.
    pub fn open_world(
        &mut self,
        rel: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<WorldPanel>> {
        self.open_world_in(rel, OpenMode::Sticky, window, cx)
    }

    /// Activate the tab already showing `rel`, or open a new one in
    /// `mode`. Returns the tab's panel. Safe to call while the workspace
    /// is leased.
    ///
    /// `mode` only decides how a NEW tab comes up: a world that already
    /// has one keeps whatever it is showing, because an agent's survey
    /// must not close down the live view the user is working in.
    pub fn open_world_in(
        &mut self,
        rel: &str,
        mode: OpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<WorldPanel>> {
        let workspace = self.workspace.upgrade()?;
        let existing = self
            .panels
            .iter()
            .find(|(open_rel, _)| open_rel == rel)
            .and_then(|(_, panel)| panel.upgrade());
        let canvas_mode = match mode {
            OpenMode::Sticky => self.canvas_mode,
            OpenMode::Design => CanvasMode::Design,
        };
        // The mask rides along either way: it says which of the cart's
        // systems the user wants running, which only matters once a tab
        // is in Live and is theirs whenever it gets there.
        let live_sys_mask = self.live_sys_mask;
        let panel = match existing {
            Some(panel) => panel,
            None => {
                let weak_workspace = self.workspace.clone();
                let weak_dock = cx.weak_entity();
                #[cfg(test)]
                let root_override = self.test_root_override.clone();
                let panel = cx.new(|cx| {
                    let mut panel = WorldPanel::new(Some(weak_workspace), cx);
                    panel.canvas_mode = canvas_mode;
                    panel.live_sys_mask = live_sys_mask;
                    panel.set_dock(weak_dock);
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
                .debug_selector(|| EMPTY_SELECTOR.into())
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
    /// The dock's OWN handle, deliberately: this is the root the
    /// containment checks use, and the world tab shares its panel's
    /// handle. Returning the panel's here would make `ToggleFocus` read
    /// "the dock is already focused" whenever the canvas tab has focus,
    /// and then refuse to open the dock (or, with
    /// `close_panel_on_toggle`, close it). Focus still LANDS in the panel
    /// -- see [`Panel::activation_focus_handle`] below.
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
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

    /// Activating the dock focuses the open world's panel, whose render
    /// tracks this handle inside the dock's own -- so the world key
    /// bindings (`GgoWorldPanel`) dispatch, which focusing the dock's
    /// bare root would not do.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active() {
            Some(panel) => panel.read(cx).focus_handle.clone(),
            None => self.focus_handle.clone(),
        }
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
    use workspace::item::Item as _;
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
            // The second click on an open world splits its `.toml` out as
            // a text tab; without `editor::init` there is no project-item
            // builder for buffers and that open fails silently.
            editor::init(cx);
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

        let tabs = workspace.read_with(cx, |ws, cx| {
            ws.items_of_type::<WorldCanvasItem>(cx)
                .map(|item| item.read(cx).tab_content_text(0, cx).to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            tabs,
            ["World: test", "World: other"],
            "a center tab per world, titled by it"
        );
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
        assert_eq!(
            workspace.read_with(cx, |ws, cx| ws.items_of_type::<WorldCanvasItem>(cx).count()),
            2,
            "and no third tab"
        );
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

    /// The dock follows the ACTIVE tab, not the last one opened: switching
    /// tabs in the pane is what the `ActiveItemChanged` subscription is
    /// for, and nothing else in these tests exercises it (`open_world`
    /// assigns `active` itself).
    #[gpui::test]
    async fn the_dock_follows_whichever_world_tab_is_active(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        for rel in ["worlds/test.toml", "worlds/other.toml"] {
            workspace.update_in(cx, |ws, window, cx| {
                ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                    dock.open_world(rel, window, cx);
                })
            });
            cx.run_until_parked();
        }
        let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

        for (index, rel) in ["worlds/test.toml", "worlds/other.toml"]
            .into_iter()
            .enumerate()
        {
            pane.update_in(cx, |pane, window, cx| {
                pane.activate_item(index, true, true, window, cx)
            });
            cx.run_until_parked();
            let active = dock.read_with(cx, |dock, _| dock.active().expect("an active world"));
            assert_eq!(
                active.read_with(cx, |panel, _| panel.open_rel_path_now().map(str::to_string)),
                Some(rel.to_string()),
                "activating tab {index} must move the dock to {rel}"
            );
        }
    }

    /// The canvas tab shares its panel's focus handle, so the dock's own
    /// handle is what says whether the DOCK is focused. With the tab
    /// focused and the dock closed, `ToggleFocus` must open it -- reading
    /// the panel's handle here made that a no-op.
    #[gpui::test]
    async fn toggle_focus_opens_the_dock_while_a_world_tab_holds_focus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));

        // The tab is focused and the dock closed: what a user gets after
        // hiding the dock to give the canvas the room.
        workspace.update_in(cx, |ws, window, cx| ws.close_panel::<WorldDock>(window, cx));
        cx.run_until_parked();
        workspace.update_in(cx, |ws, window, cx| {
            ws.active_pane()
                .update(cx, |pane, cx| pane.focus_active_item(window, cx));
        });
        cx.run_until_parked();
        workspace.read_with(cx, |ws, cx| {
            assert!(!ws.right_dock().read(cx).is_open(), "the dock is closed");
        });

        cx.update(|window, cx| {
            assert!(
                panel.read(cx).focus_handle.is_focused(window),
                "the canvas tab holds the panel's focus handle"
            );
        });
        // The body of the `ToggleFocus` action, called rather than
        // dispatched: the canvas tab shares the panel's focus handle and
        // never tracks it in its own render, so with the dock closed the
        // focused handle has no dispatch node for an action to travel up.
        let focused_the_dock = workspace.update_in(cx, |ws, window, cx| {
            ws.toggle_panel_focus::<WorldDock>(window, cx)
        });
        assert!(
            focused_the_dock,
            "the dock must read as unfocused while the tab holds the panel's handle"
        );
        cx.run_until_parked();
        workspace.read_with(cx, |ws, cx| {
            assert!(
                ws.right_dock().read(cx).is_open(),
                "ToggleFocus must open the dock even from the canvas tab"
            );
        });
        cx.update(|window, cx| {
            assert!(
                panel.read(cx).focus_handle.is_focused(window),
                "and focus must land in the panel, where the world bindings live"
            );
        });
    }

    /// What `remote_open`'s "leave the open world alone" rule became: a
    /// second open of a world that already has a tab activates that tab
    /// and does NOT reload it, so an agent's `world_open` cannot drop the
    /// user's unsaved edits, undo stack or camera. A world whose tab is
    /// dirty is no longer a reason to refuse OTHER worlds either -- they
    /// get their own tabs -- which is what the second half asserts.
    #[gpui::test]
    async fn re_opening_a_dirty_world_activates_its_tab_without_reloading(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        let open = |rel: &'static str, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |ws, window, cx| {
                ggo_common::open_in_panel(ws, window, cx, move |dock: &mut WorldDock, window, cx| {
                    dock.open_world(rel, window, cx);
                })
            });
            cx.run_until_parked();
        };
        open("worlds/test.toml", cx);
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
        crate::tests::dirty_the_world(&panel, cx);

        open("worlds/test.toml", cx);
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.active().expect("still open")),
            panel,
            "the same tab, not a fresh panel"
        );
        assert!(
            panel.read_with(cx, |panel, _| panel.test_is_dirty()),
            "re-opening the open world must not reload it"
        );

        open("worlds/other.toml", cx);
        assert!(
            panel.read_with(cx, |panel, _| panel.test_is_dirty()),
            "and another world opens in its own tab, leaving the dirty one be"
        );
        assert_eq!(dock.read_with(cx, |dock, _| dock.open_panels().len()), 2);
    }

    /// A new tab is `Loading` the moment `open_world` returns -- the load
    /// itself is deferred -- so an agent's `world_read` right behind its
    /// `world_open` waits for THAT world instead of answering "nothing is
    /// open" or reading the world that was in front before.
    #[gpui::test]
    async fn a_new_tab_reads_as_loading_before_its_deferred_load_lands(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        let panel = workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            });
            ws.panel::<WorldDock>(cx)
                .and_then(|dock| dock.read(cx).active())
                .expect("open_world assigns the active panel synchronously")
        });
        let error = panel
            .read_with(cx, |panel, _| panel.remote_read())
            .expect_err("nothing has loaded yet");
        assert!(error.contains(crate::WORLD_STILL_LOADING), "{error}");

        cx.run_until_parked();
        let read = panel
            .read_with(cx, |panel, _| panel.remote_read())
            .expect("the deferred load lands");
        assert_eq!(read["rel_path"], "worlds/test.toml");
        assert!(dock.read_with(cx, |dock, _| dock.active().is_some()));
    }

    #[gpui::test]
    async fn the_dock_renders_the_empty_message_without_a_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<WorldDock>(window, cx)
        });
        cx.run_until_parked();
        assert!(dock.read_with(cx, |dock, _| dock.active().is_none()));
        assert!(
            cx.debug_bounds(EMPTY_SELECTOR).is_some(),
            "the empty dock says what to do rather than rendering blank"
        );
    }

    /// The Live choices are the DOCK's (spec), not the active panel's:
    /// closing the last world tab takes every panel with it, and the next
    /// world the user opens must still come up the way they left it.
    #[gpui::test]
    async fn the_sticky_mode_survives_closing_the_last_world_tab(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        let open = |rel: &'static str, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |ws, window, cx| {
                ggo_common::open_in_panel(ws, window, cx, move |dock: &mut WorldDock, window, cx| {
                    dock.open_world(rel, window, cx);
                })
            });
            cx.run_until_parked();
        };
        open("worlds/test.toml", cx);
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(crate::CanvasMode::Design, window, cx)
        });
        cx.run_until_parked();

        // A second tab inherits it while the first is still open.
        open("worlds/other.toml", cx);
        let second = dock.read_with(cx, |dock, _| dock.active().expect("a second tab"));
        assert_eq!(
            second.read_with(cx, |panel, _| panel.canvas_mode()),
            crate::CanvasMode::Design,
            "a new tab opens the way the last one was showing"
        );

        // And it survives every panel going away -- including the strong
        // handles this test is holding, which would otherwise keep the
        // panels alive past their tabs.
        drop(panel);
        drop(second);
        let ids = workspace.read_with(cx, |ws, cx| {
            ws.items_of_type::<WorldCanvasItem>(cx)
                .map(|item| item.entity_id())
                .collect::<Vec<_>>()
        });
        let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());
        for id in ids {
            pane.update_in(cx, |pane, window, cx| {
                pane.close_item_by_id(id, workspace::SaveIntent::Skip, window, cx)
            })
            .await
            .expect("close");
        }
        cx.run_until_parked();
        assert!(dock.read_with(cx, |dock, _| dock.open_panels().is_empty()));

        open("worlds/test.toml", cx);
        let reopened = dock.read_with(cx, |dock, _| dock.active().expect("a fresh tab"));
        assert_eq!(
            reopened.read_with(cx, |panel, _| panel.canvas_mode()),
            crate::CanvasMode::Design,
            "the dock kept the user's choice with no panel left to hold it"
        );
    }

    /// The MCP tools open worlds nobody is looking at: in Live each of
    /// those would boot a headless emulator run for a picture that is
    /// never drawn.
    #[gpui::test]
    async fn an_agent_open_comes_up_in_design_and_leaves_an_open_tab_alone(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world_in("worlds/test.toml", OpenMode::Design, window, cx);
            })
        });
        cx.run_until_parked();
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.canvas_mode()),
            crate::CanvasMode::Design
        );
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.canvas_mode),
            crate::CanvasMode::Live,
            "and the user's own sticky choice is not overwritten by it"
        );

        // A world that already has a tab keeps whatever it is showing.
        // Assigned rather than switched through `set_canvas_mode`: there
        // is no viewer booter registered in these tests, so entering Live
        // would fall straight back to Design.
        panel.update(cx, |panel, cx| {
            panel.canvas_mode = crate::CanvasMode::Live;
            cx.notify();
        });
        cx.run_until_parked();
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world_in("worlds/test.toml", OpenMode::Design, window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.active().expect("still open")),
            panel,
            "the same tab"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.canvas_mode()),
            crate::CanvasMode::Live,
            "an agent's read must not close down the user's live view"
        );
    }
}
