//! One center-pane tab per `.til`: a workspace [`Item`] wrapping its own
//! [`TilesetPanel`] entity, so every open tileset keeps independent state
//! and undo -- the tileset analog of `ggo_sprite_panel::sprite_item`. The
//! panel type keeps ALL document logic; this file only adapts it to the
//! workspace's tab machinery (title, dirty dot, save routing).

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity,
    Window,
};
use project::Project;
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent, SaveOptions};

use crate::{TilesetPanel, ViewerState};

pub enum TilesetItemEvent {
    UpdateTab,
}

pub struct TilesetEditorItem {
    panel: Entity<TilesetPanel>,
    rel: String,
}

impl TilesetEditorItem {
    pub fn new(
        rel: String,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| TilesetPanel::new(Some(workspace), cx));
        Self::wrap(rel, panel, window, cx)
    }

    /// [`Self::new`] against a bare filesystem root instead of a live
    /// workspace -- the panel tests' `root_override` hook.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        rel: String,
        root: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| {
            let mut panel = TilesetPanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        Self::wrap(rel, panel, window, cx)
    }

    fn wrap(
        rel: String,
        panel: Entity<TilesetPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        panel.update(cx, |panel, cx| panel.open_rel_path(&rel, window, cx));
        // Any inner-panel change may flip the dirty dot -- re-render the
        // tab.
        cx.observe(&panel, |_, _, cx| cx.emit(TilesetItemEvent::UpdateTab))
            .detach();
        Self { panel, rel }
    }

    pub fn rel(&self) -> &str {
        &self.rel
    }

    #[cfg(test)]
    pub(crate) fn panel(&self) -> &Entity<TilesetPanel> {
        &self.panel
    }
}

impl EventEmitter<TilesetItemEvent> for TilesetEditorItem {}

impl Focusable for TilesetEditorItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Render for TilesetEditorItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for TilesetEditorItem {
    type Event = TilesetItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            TilesetItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        std::path::Path::new(&self.rel)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "Tileset".to_string())
            .into()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.panel.read(cx).dirty()
    }

    fn can_save(&self, cx: &App) -> bool {
        self.is_dirty(cx)
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

    fn write_fixture(root: &std::path::Path) {
        use ggo_worldlib::sprites::io::save_tileset;
        use ggo_worldlib::sprites::palette565::PAL_SLOTS;
        use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
        let indices = vec![0u8; 2 * TILE_PIXELS];
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800;
        save_tileset(root, "tiles/world.til", &indices, 2, &palette).unwrap();
    }

    /// The item mirrors the panel's document state onto the tab (title,
    /// dirty dot) and [`Item::save`] routes through the panel's own save
    /// path both ways: Ok(()) only after the pair actually landed (and
    /// the dirty dot cleared), and a failed write surfaced as Err WITH
    /// the document kept dirty -- the workspace treats an Ok save as
    /// "safe to close the tab", so a swallowed failure here would discard
    /// unwritten edits.
    #[gpui::test]
    async fn test_item_wraps_a_panel_mirrors_dirty_and_routes_save(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let root = dir.path().to_path_buf();
        let (item, cx) = cx.add_window_view(|window, cx| {
            TilesetEditorItem::new_for_test("tiles/world.til".into(), root, window, cx)
        });
        cx.run_until_parked();

        item.read_with(cx, |item, cx| {
            assert_eq!(item.rel(), "tiles/world.til");
            assert_eq!(item.tab_content_text(0, cx).as_ref(), "world");
            assert!(!item.is_dirty(cx), "freshly opened is clean");
        });

        item.update(cx, |item, cx| {
            item.panel().clone().update(cx, |panel, cx| {
                panel.apply_paint_for_test(0, 0, 0, 1, cx);
            });
        });
        cx.run_until_parked();
        item.read_with(cx, |item, cx| {
            assert!(item.is_dirty(cx), "panel edits surface as item dirt");
        });

        let save = item.update_in(cx, |item, window, cx| {
            item.save(SaveOptions::default(), project.clone(), window, cx)
        });
        save.await
            .expect("saving into the fixture root must succeed");
        item.read_with(cx, |item, cx| {
            assert!(!item.is_dirty(cx), "a landed save clears the dirty dot");
        });

        // Break the save target: a root pointing at a regular FILE makes
        // the atomic write fail deterministically.
        let bad_root = dir.path().join("tiles/world.til");
        item.update(cx, |item, cx| {
            item.panel().clone().update(cx, |panel, cx| {
                panel.apply_paint_for_test(0, 1, 0, 1, cx);
                panel.project_root = Some(bad_root);
            });
        });
        let save = item.update_in(cx, |item, window, cx| {
            item.save(SaveOptions::default(), project, window, cx)
        });
        assert!(save.await.is_err(), "a failed write must surface as Err");
        item.read_with(cx, |item, cx| {
            assert!(
                item.is_dirty(cx),
                "a failed save must keep the document dirty"
            );
        });
    }
}
