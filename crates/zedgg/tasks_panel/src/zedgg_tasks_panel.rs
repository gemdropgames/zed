mod task_view;

use gpui::App;

pub use task_view::{TaskView, open_task};

pub fn init(_cx: &mut App) {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use language::{Language, LanguageConfig};
    use project::{FakeFs, Project};
    use std::sync::Arc;
    use workspace::item::Item as _;
    use workspace::{AppState, MultiWorkspace, Workspace};
    use zedgg_project_db::open;
    use zedgg_project_db::tasks;

    pub(super) async fn task_workspace(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<Workspace>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            markdown_preview::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(Arc::new(Language::new(
                LanguageConfig { name: "Markdown".into(), ..Default::default() },
                None,
            )));
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        (workspace, cx)
    }

    #[gpui::test]
    async fn test_open_task_edits_and_saves_description_and_title(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = task_workspace(cx).await;
        let id = {
            let c = open(dir.path()).unwrap();
            let id = tasks::create_task(&c, "Ship it").unwrap();
            tasks::save_description(&c, id, "# Steps\n").unwrap();
            id
        };
        workspace.update_in(cx, |_, window, cx| {
            open_task(
                cx.weak_entity(),
                dir.path().to_path_buf(),
                id,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let view = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<TaskView>(cx).next().expect("tab opened")
        });
        view.read_with(cx, |view, cx| {
            assert_eq!(view.task_id(), id);
            assert_eq!(view.tab_content_text(0, cx), "Ship it");
            assert!(!view.is_dirty(cx));
        });

        view.update_in(cx, |view, window, cx| {
            view.editor()
                .update(cx, |editor, cx| editor.set_text("# Steps\n- one\n", window, cx));
            assert!(view.is_dirty(cx));
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let c = open(dir.path()).unwrap();
        assert_eq!(tasks::load_description(&c, id).unwrap(), "# Steps\n- one\n");
        view.read_with(cx, |view, cx| assert!(!view.is_dirty(cx)));

        // Second open re-activates, no duplicate tab.
        workspace.update_in(cx, |_, window, cx| {
            open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<TaskView>(cx).count(), 1);
        });
    }
}
