//! The map editor as a center-pane tab: one [`MapEditorItem`] per `.map`,
//! wrapping a [`MapPanel`] the same way `ggo_tileset_panel`'s item wraps
//! its panel. The workspace's own tab machinery gives the dirty dot,
//! save, and the close-dirty prompt.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity,
    Window,
};
use project::Project;
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent, SaveOptions};

use crate::{MapPanel, ViewerState};

pub enum MapItemEvent {
    UpdateTab,
}

pub struct MapEditorItem {
    panel: Entity<MapPanel>,
    rel: String,
}

impl MapEditorItem {
    pub fn new(
        rel: String,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| MapPanel::new(Some(workspace), cx));
        Self::wrap(rel, panel, window, cx)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        rel: String,
        root: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| {
            let mut panel = MapPanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        Self::wrap(rel, panel, window, cx)
    }

    fn wrap(
        rel: String,
        panel: Entity<MapPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        panel.update(cx, |panel, cx| panel.open_rel_path(&rel, window, cx));
        cx.observe(&panel, |_, _, cx| cx.emit(MapItemEvent::UpdateTab))
            .detach();
        Self { panel, rel }
    }

    pub fn rel(&self) -> &str {
        &self.rel
    }

    pub fn panel(&self) -> &Entity<MapPanel> {
        &self.panel
    }
}

impl EventEmitter<MapItemEvent> for MapEditorItem {}

impl Focusable for MapEditorItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Render for MapEditorItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for MapEditorItem {
    type Event = MapItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            MapItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        std::path::Path::new(&self.rel)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "Map".to_string())
            .into()
    }

    /// A single document per tab: this is what makes the pane prompt
    /// before closing a dirty tab (`Pane::skip_save_on_close`).
    fn buffer_kind(&self, _cx: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.panel.read(cx).dirty()
    }

    fn can_save(&self, cx: &App) -> bool {
        self.is_dirty(cx)
    }

    /// "Don't Save" on the close prompt lands here (`Pane::save_item`,
    /// the `Ok(1)` arm): a singleton item that `can_save` MUST implement
    /// this, or the default `unimplemented!()` takes the process down.
    /// Discarding is a re-read from disk through the panel's own load
    /// path, which is generation-guarded and drops the dirty document.
    fn reload(
        &mut self,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let rel = self.rel.clone();
        self.panel
            .update(cx, |panel, cx| panel.reload_from_disk(&rel, cx));
        Task::ready(Ok(()))
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let result = self.panel.update(cx, |panel, cx| {
            panel.save_impl(cx);
            match &panel.state {
                ViewerState::Ready(open) => match &open.save_error {
                    Some(e) => Err(anyhow::anyhow!(e.clone())),
                    None => Ok(()),
                },
                _ => Ok(()),
            }
        });
        Task::ready(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::FakeFs;
    use workspace::AppState;

    #[gpui::test]
    async fn test_item_wraps_a_panel_mirrors_dirty_and_routes_save(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let dir = tempfile::tempdir().unwrap();
        crate::tests::write_project(dir.path());
        let root = dir.path().to_path_buf();
        let (item, cx) = cx.add_window_view(|window, cx| {
            MapEditorItem::new_for_test("assets/maps/level.map".into(), root, window, cx)
        });
        cx.run_until_parked();

        item.read_with(cx, |item, cx| {
            assert_eq!(item.rel(), "assets/maps/level.map");
            assert_eq!(item.tab_content_text(0, cx).as_ref(), "level");
            assert!(!item.is_dirty(cx), "freshly opened is clean");
        });

        item.update(cx, |item, cx| {
            item.panel()
                .clone()
                .update(cx, |panel, cx| panel.paint_at((0, 0), cx));
        });
        cx.run_until_parked();
        item.read_with(cx, |item, cx| {
            assert!(item.is_dirty(cx), "edits surface as item dirt")
        });

        let save = item.update_in(cx, |item, window, cx| {
            item.save(SaveOptions::default(), project.clone(), window, cx)
        });
        save.await
            .expect("saving into the fixture root must succeed");
        item.read_with(cx, |item, cx| {
            assert!(!item.is_dirty(cx), "a landed save is clean")
        });
    }
}
