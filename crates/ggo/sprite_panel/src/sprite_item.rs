//! One center-pane tab per `.spr`: a workspace [`Item`] wrapping its own
//! [`SpritePanel`] entity, so every open sprite keeps independent state
//! and undo. This replaces the dock registration (spec
//! `docs/superpowers/specs/2026-08-20-ggo-center-editors-design.md`) --
//! the panel type keeps ALL document logic; this file only adapts it to
//! the workspace's tab machinery (title, dirty dot, save routing).

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity,
    Window,
};
use project::Project;
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent, SaveOptions};

use crate::{SpritePanel, ViewerState};

pub enum SpriteItemEvent {
    UpdateTab,
}

pub struct SpriteEditorItem {
    panel: Entity<SpritePanel>,
    rel: String,
}

impl SpriteEditorItem {
    pub fn new(
        rel: String,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| SpritePanel::new(Some(workspace), cx));
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
            let mut panel = SpritePanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        Self::wrap(rel, panel, window, cx)
    }

    /// An item with no sprite loaded yet -- the host for the "New
    /// Sprite…" form, whose document only exists after the form commits.
    /// The rel-sync observer picks up the created sprite's rel once the
    /// panel loads it.
    pub fn new_empty(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let panel = cx.new(|cx| SpritePanel::new(Some(workspace), cx));
        Self::observe_panel(&panel, cx);
        Self {
            panel,
            rel: String::new(),
        }
    }

    fn wrap(
        rel: String,
        panel: Entity<SpritePanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        panel.update(cx, |panel, cx| panel.open_rel_path(&rel, window, cx));
        Self::observe_panel(&panel, cx);
        Self { panel, rel }
    }

    /// Any inner-panel change may flip the dirty dot or (rename, "New
    /// Sprite…" landing) repoint the document -- resync the tab identity
    /// and re-render the tab.
    fn observe_panel(panel: &Entity<SpritePanel>, cx: &mut Context<Self>) {
        cx.observe(panel, |this, panel, cx| {
            if let ViewerState::Ready(open) = &panel.read(cx).state {
                this.rel = open.source_rel.clone();
            }
            cx.emit(SpriteItemEvent::UpdateTab);
        })
        .detach();
    }

    pub fn rel(&self) -> &str {
        &self.rel
    }

    /// Smoke-test hook: `ggo_smoke` reaches the panel through the tab.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_panel(&self) -> Entity<SpritePanel> {
        self.panel.clone()
    }

    pub(crate) fn panel(&self) -> &Entity<SpritePanel> {
        &self.panel
    }
}

impl EventEmitter<SpriteItemEvent> for SpriteEditorItem {}

impl Focusable for SpriteEditorItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Render for SpriteEditorItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for SpriteEditorItem {
    type Event = SpriteItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            SpriteItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        std::path::Path::new(&self.rel)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "New Sprite".to_string())
            .into()
    }

    /// A single document per tab: this is what makes the pane prompt
    /// before closing a dirty tab (`Pane::skip_save_on_close`).
    fn buffer_kind(&self, _cx: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn is_dirty(&self, cx: &App) -> bool {
        matches!(&self.panel.read(cx).state, ViewerState::Ready(open) if open.store.dirty())
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
    use crate::test_fixtures::write_sprite_fixture;
    use ggo_worldlib::sprites::io::open_sprite;
    use ggo_worldlib::sprites::sprite_doc::DocOp;
    use gpui::TestAppContext;
    use project::FakeFs;
    use workspace::AppState;

    #[gpui::test]
    async fn test_item_wraps_a_panel_and_mirrors_dirty(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture(dir.path());
        let root = dir.path().to_path_buf();
        let (item, cx) = cx.add_window_view(|window, cx| {
            SpriteEditorItem::new_for_test("sprites/hero.spr".into(), root, window, cx)
        });
        cx.run_until_parked();

        item.read_with(cx, |item, cx| {
            assert_eq!(item.rel(), "sprites/hero.spr");
            assert_eq!(item.tab_content_text(0, cx).as_ref(), "hero");
            assert!(!item.is_dirty(cx), "freshly opened is clean");
        });
        item.update(cx, |item, cx| {
            item.panel()
                .clone()
                .update(cx, |panel, cx| panel.step_size(1, 0, cx));
        });
        cx.run_until_parked();
        item.read_with(cx, |item, cx| {
            assert!(item.is_dirty(cx), "panel edits surface as item dirt");
        });
    }

    /// [`Item::save`] must route through the panel's own save path both
    /// ways: Ok(()) only after the trio actually landed (and the dirty
    /// dot cleared), and a failed write surfaced as Err WITH the document
    /// kept dirty -- the workspace treats an Ok save as "safe to close
    /// the tab", so a swallowed failure here would discard unwritten
    /// edits.
    #[gpui::test]
    async fn test_item_save_routes_the_panel_save_and_reports_failures(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture(dir.path());
        let root = dir.path().to_path_buf();
        let (item, cx) = cx.add_window_view(|window, cx| {
            SpriteEditorItem::new_for_test("sprites/hero.spr".into(), root, window, cx)
        });
        cx.run_until_parked();

        item.update(cx, |item, cx| {
            item.panel().clone().update(cx, |panel, cx| {
                assert!(panel.apply_doc(DocOp::FrameDuration { at: 0, ms: 500 }, cx));
            });
        });
        let save = item.update_in(cx, |item, window, cx| {
            item.save(SaveOptions::default(), project.clone(), window, cx)
        });
        save.await
            .expect("saving into the fixture root must succeed");
        item.read_with(cx, |item, cx| {
            assert!(!item.is_dirty(cx), "a landed save clears the dirty dot");
        });
        assert_eq!(
            open_sprite(dir.path(), "sprites/hero.spr")
                .unwrap()
                .state
                .frames[0]
                .duration_ms,
            500,
            "the edit reached the trio on disk"
        );

        // Break the save target: a root pointing at a regular FILE makes
        // `save_sprite`'s create-parent-dirs step fail deterministically,
        // where a merely missing directory would just be created.
        let bad_root = dir.path().join("sprites/hero.spr");
        item.update(cx, |item, cx| {
            item.panel().clone().update(cx, |panel, cx| {
                assert!(panel.apply_doc(DocOp::FrameDuration { at: 0, ms: 700 }, cx));
                if let ViewerState::Ready(open) = &mut panel.state {
                    open.root = bad_root;
                }
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
            let panel = item.panel().read(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                open.save_error.is_some(),
                "and surface the failure on the panel"
            );
        });
    }
}
