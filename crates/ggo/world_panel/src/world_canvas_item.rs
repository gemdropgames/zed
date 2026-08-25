//! The world CANVAS as a center-pane tab: a workspace [`Item`] over the
//! dock [`WorldPanel`]'s state (spec
//! `docs/superpowers/specs/2026-08-20-ggo-center-editors-design.md`).
//! The panel stays the document owner -- load/save/inspector/entity
//! editing all live in the dock; this item only gives the viewport the
//! center pane's space. Closing the tab closes nothing but the view.

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task,
    WeakEntity, Window,
};
use project::Project;
use ui::prelude::*;
use workspace::item::{Item, ItemEvent, SaveOptions};

use crate::{ViewerState, WorldPanel};

pub enum WorldCanvasEvent {
    UpdateTab,
}

pub struct WorldCanvasItem {
    panel: WeakEntity<WorldPanel>,
    /// Fallback focus for the dead-panel case (workspace tore the dock
    /// down); with a live panel the item shares its focus handle so the
    /// panel's key bindings work from the canvas tab.
    focus_handle: FocusHandle,
}

impl WorldCanvasItem {
    pub fn new(panel: WeakEntity<WorldPanel>, cx: &mut Context<Self>) -> Self {
        if let Some(panel) = panel.upgrade() {
            cx.observe(&panel, |_, _, cx| cx.emit(WorldCanvasEvent::UpdateTab))
                .detach();
        }
        Self {
            panel,
            focus_handle: cx.focus_handle(),
        }
    }

}

impl EventEmitter<WorldCanvasEvent> for WorldCanvasItem {}

impl Focusable for WorldCanvasItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.panel.upgrade() {
            Some(panel) => panel.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for WorldCanvasItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let canvas: Option<AnyElement> = self.panel.upgrade().and_then(|panel| {
            panel.update(cx, |panel, cx| {
                // The image cache's atlas release runs here, on the one
                // render path that paints those images.
                panel.retire_images(window);
                match &panel.state {
                    ViewerState::Ready(_) => Some(panel.render_canvas(cx)),
                    _ => None,
                }
            })
        });
        match canvas {
            Some(canvas) => div().size_full().child(canvas).into_any_element(),
            None => div()
                .size_full()
                .flex()
                .justify_center()
                .items_center()
                .child(Label::new("No world open").color(Color::Muted))
                .into_any_element(),
        }
    }
}

impl Item for WorldCanvasItem {
    type Event = WorldCanvasEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            WorldCanvasEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        let stem = self.panel.upgrade().and_then(|panel| {
            panel.read(cx).open_rel_path_now().map(|rel| {
                std::path::Path::new(rel)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| rel.to_string())
            })
        });
        match stem {
            Some(stem) => format!("World: {stem}").into(),
            None => "World".into(),
        }
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.panel
            .upgrade()
            .is_some_and(|panel| panel.read(cx).dirty_world_name().is_some())
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
        // An `Ok(())` here would tell the close flow the edits were
        // written when nothing holds them anymore -- a data-loss lie.
        let Some(panel) = self.panel.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!(
                "cannot save the world: its panel no longer exists"
            )));
        };
        let result = panel.update(cx, |panel, cx| {
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
    async fn test_canvas_item_renders_panel_canvas_and_mirrors_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = crate::tests::ready_panel_in_window(cx, dir.path()).await;

        let item = cx.update(|_, cx| cx.new(|cx| WorldCanvasItem::new(panel.downgrade(), cx)));
        item.read_with(cx, |item, cx| {
            assert_eq!(item.tab_content_text(0, cx).as_ref(), "World: test");
            assert!(!item.is_dirty(cx), "clean panel, clean tab");
        });

        crate::tests::dirty_the_world(&panel, cx);
        item.read_with(cx, |item, cx| {
            assert!(item.is_dirty(cx), "panel dirt surfaces on the tab");
        });

        // Render the item in its own window: the canvas paints from the
        // panel's state and records its bounds (the thing the dock render
        // no longer does).
        let (item, cx) = cx.add_window_view(|_, cx| WorldCanvasItem::new(panel.downgrade(), cx));
        cx.run_until_parked();
        let _ = item;
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                open.view.borrow().last_bounds.is_some(),
                "the item's render painted the canvas"
            );
        });
    }

    /// `Item::save` routes through the panel's own save path: success
    /// writes the file and clears dirty, a failed write surfaces as `Err`
    /// and keeps the document dirty, and a DEAD panel (the workspace tore
    /// the dock down while the tab survived) must also `Err` -- an `Ok`
    /// there would report a save that wrote nothing, silently dropping the
    /// edits on close.
    #[gpui::test]
    async fn test_item_save_success_and_failure_and_dead_panel(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = crate::tests::ready_panel_in_window(cx, dir.path()).await;
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let item = cx.update(|_, cx| cx.new(|cx| WorldCanvasItem::new(panel.downgrade(), cx)));
        crate::tests::dirty_the_world(&panel, cx);
        let save = cx.update(|window, cx| {
            item.update(cx, |item, cx| {
                item.save(SaveOptions::default(), project.clone(), window, cx)
            })
        });
        save.await.expect("a healthy save must succeed");
        let on_disk =
            ggo_worldlib::world_file::read_world(dir.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            serde_json::json!([50, 60]),
            "save must write the panel's edit"
        );
        item.read_with(cx, |item, cx| {
            assert!(!item.is_dirty(cx), "save clears dirty");
        });

        // Break the write: the panel saves under the OPEN document's root,
        // so repointing that root at a regular file makes the parent-dir
        // creation fail deterministically. (Dirtied with a DIFFERENT pos
        // than `dirty_the_world`'s -- a same-pos move is a no-op the store
        // won't record.)
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                ggo_worldlib::world_doc::WorldOp::MoveEntity {
                    entity: 0,
                    pos: [70.0, 80.0],
                    gesture: None,
                },
                cx,
            );
            assert!(panel.dirty_world_name().is_some(), "op should dirty the doc");
        });
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.root = blocker;
        });
        let save = cx.update(|window, cx| {
            item.update(cx, |item, cx| {
                item.save(SaveOptions::default(), project.clone(), window, cx)
            })
        });
        assert!(save.await.is_err(), "a failed write must surface as Err");
        item.read_with(cx, |item, cx| {
            assert!(item.is_dirty(cx), "a failed save must keep the tab dirty");
        });

        // A dead panel: drop the only strong handle and save the orphaned
        // tab.
        let doomed = cx.update(|_, cx| cx.new(|cx| WorldPanel::new(None, cx)));
        let dead_item =
            cx.update(|_, cx| cx.new(|cx| WorldCanvasItem::new(doomed.downgrade(), cx)));
        drop(doomed);
        cx.run_until_parked();
        let save = cx.update(|window, cx| {
            dead_item.update(cx, |item, cx| {
                item.save(SaveOptions::default(), project.clone(), window, cx)
            })
        });
        save.await
            .expect_err("a dead panel must not report a save that wrote nothing");
    }

    /// An item over a panel with NO open world still renders (the
    /// "No world open" fallback, not a panic) and titles itself with the
    /// bare "World".
    #[gpui::test]
    async fn test_item_render_falls_back_when_no_world_is_open(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
        });
        let panel = cx.update(|cx| cx.new(|cx| WorldPanel::new(None, cx)));
        let (item, cx) = cx.add_window_view(|_, cx| WorldCanvasItem::new(panel.downgrade(), cx));
        cx.run_until_parked();
        item.read_with(cx, |item, cx| {
            assert_eq!(item.tab_content_text(0, cx).as_ref(), "World");
            assert!(!item.is_dirty(cx), "an empty panel has nothing to save");
        });
    }
}
