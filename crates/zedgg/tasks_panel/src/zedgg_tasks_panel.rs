mod board;
mod panel;
mod task_view;

pub use board::{TaskBoard, open_board};
pub use panel::{Delete, NewTask, OpenBoard, TasksPanel, ToggleFocus, init};
pub use task_view::{TaskView, open_task};

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

    #[gpui::test]
    async fn test_title_edit_saves_with_description(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = task_workspace(cx).await;
        let id = { tasks::create_task(&open(dir.path()).unwrap(), "old").unwrap() };
        workspace.update_in(cx, |_, window, cx| {
            open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
        });
        cx.run_until_parked();
        let view = workspace.read_with(cx, |w, cx| w.items_of_type::<TaskView>(cx).next().unwrap());

        view.update_in(cx, |view, window, cx| {
            view.set_title_for_save("new title".into(), window, cx)
        });
        view.read_with(cx, |view, cx| assert!(view.is_dirty(cx), "title change dirties"));
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        let c = open(dir.path()).unwrap();
        assert_eq!(tasks::get_task(&c, id).unwrap().unwrap().title, "new title");
        view.read_with(cx, |view, cx| {
            assert!(!view.is_dirty(cx));
            assert_eq!(view.tab_content_text(0, cx), "new title");
        });
    }

    #[gpui::test]
    async fn test_state_change_and_tag_assign_from_view(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = task_workspace(cx).await;
        let id = { tasks::create_task(&open(dir.path()).unwrap(), "t").unwrap() };
        workspace.update_in(cx, |_, window, cx| {
            open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
        });
        cx.run_until_parked();
        let view = workspace.read_with(cx, |w, cx| w.items_of_type::<TaskView>(cx).next().unwrap());

        view.update_in(cx, |view, window, cx| {
            view.set_state(tasks::TaskState::InProgress, window, cx)
        });
        cx.run_until_parked();
        let c = open(dir.path()).unwrap();
        assert_eq!(
            tasks::get_task(&c, id).unwrap().unwrap().state,
            tasks::TaskState::InProgress
        );

        view.update_in(cx, |view, window, cx| view.add_tag("art".into(), window, cx));
        cx.run_until_parked();
        let c = open(dir.path()).unwrap();
        let tags = tasks::list_tags(&c).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "art");
        assert_eq!(
            tasks::get_task(&c, id).unwrap().unwrap().tag_ids,
            [tags[0].id]
        );
        view.read_with(cx, |view, _| assert_eq!(view.tag_names(), ["art"]));
    }

    /// Regression test: `set_state` (like `add_tag`/`remove_tag`) refreshes
    /// title/state/tags from the DB in the background, but must not
    /// clobber an unsaved description edit sitting in the buffer -- or
    /// silently mark it clean -- while doing so.
    #[gpui::test]
    async fn test_set_state_does_not_clobber_unsaved_description_edit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = task_workspace(cx).await;
        let id = { tasks::create_task(&open(dir.path()).unwrap(), "t").unwrap() };
        workspace.update_in(cx, |_, window, cx| {
            open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
        });
        cx.run_until_parked();
        let view = workspace.read_with(cx, |w, cx| w.items_of_type::<TaskView>(cx).next().unwrap());

        view.update_in(cx, |view, window, cx| {
            view.editor()
                .update(cx, |editor, cx| editor.set_text("unsaved notes", window, cx));
        });
        view.read_with(cx, |view, cx| assert!(view.is_dirty(cx)));

        view.update_in(cx, |view, window, cx| {
            view.set_state(tasks::TaskState::InProgress, window, cx)
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert!(
                view.is_dirty(cx),
                "unsaved description edit must survive a state refresh"
            );
            assert_eq!(view.buffer().read(cx).text(), "unsaved notes");
        });
        let c = open(dir.path()).unwrap();
        assert_eq!(
            tasks::get_task(&c, id).unwrap().unwrap().state,
            tasks::TaskState::InProgress,
            "the state change itself still lands"
        );
        assert_eq!(
            tasks::load_description(&c, id).unwrap(),
            "",
            "the unsaved edit must not have been written either"
        );
    }

    /// A workspace with the panel registered and pointed at `root` (a real
    /// temp dir) via `root_override`, so DB reads/writes hit disk while the
    /// project itself is a `FakeFs`.
    pub(super) async fn tasks_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        gpui::Entity<Workspace>,
        gpui::Entity<TasksPanel>,
        &'a mut gpui::VisualTestContext,
    ) {
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
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<TasksPanel>(cx)
                .expect("TasksPanel should have been added by init()")
        });
        panel.update(cx, |panel, cx| {
            panel.root_override = Some(root.to_path_buf());
            panel.refresh_root(cx);
        });
        cx.run_until_parked();
        (workspace, panel, cx)
    }

    #[gpui::test]
    async fn test_panel_lists_tasks_grouped_and_click_opens_tab(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, cx) = tasks_workspace(cx, dir.path()).await;
        let (a, b) = {
            let c = open(dir.path()).unwrap();
            let a = tasks::create_task(&c, "draw tiles").unwrap();
            let b = tasks::create_task(&c, "fix jump").unwrap();
            tasks::move_task_between(&c, b, tasks::TaskState::Review, None, None).unwrap();
            (a, b)
        };
        panel.update(cx, |panel, cx| panel.reload(cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let rows = panel.visible_row_labels();
            assert_eq!(
                rows,
                ["Backlog", "draw tiles", "In Progress", "Review", "fix jump", "Done"]
            );
        });

        panel.update_in(cx, |panel, window, cx| panel.click_task(a, window, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let view = workspace.items_of_type::<TaskView>(cx).next().expect("tab");
            assert_eq!(view.read(cx).task_id(), a);
        });
        let _ = b;
    }

    #[gpui::test]
    async fn test_toggle_focus_opens_left_dock(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, cx) = tasks_workspace(cx, dir.path()).await;
        workspace.update(cx, |workspace, cx| {
            assert!(!workspace.left_dock().read(cx).is_open());
        });
        cx.dispatch_action(ToggleFocus);
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.left_dock().read(cx).is_open());
        });
    }

    #[gpui::test]
    async fn test_board_groups_cards_and_click_opens_tab(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, cx) = tasks_workspace(cx, dir.path()).await;
        let (a, b) = {
            let c = open(dir.path()).unwrap();
            let a = tasks::create_task(&c, "draw tiles").unwrap();
            let b = tasks::create_task(&c, "fix jump").unwrap();
            tasks::move_task_between(&c, b, tasks::TaskState::InProgress, None, None).unwrap();
            (a, b)
        };
        workspace.update_in(cx, |workspace, window, cx| {
            open_board(workspace, dir.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        let board = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<TaskBoard>(cx).next().expect("board tab")
        });
        board.read_with(cx, |board, _| {
            assert_eq!(board.column(tasks::TaskState::Backlog), [a]);
            assert_eq!(board.column(tasks::TaskState::InProgress), [b]);
            assert_eq!(board.column(tasks::TaskState::Done), [] as [i64; 0]);
        });

        board.update_in(cx, |board, window, cx| board.open_card(a, window, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let view = workspace.items_of_type::<TaskView>(cx).next().expect("tab");
            assert_eq!(view.read(cx).task_id(), a);
        });

        // Second open_board re-activates, no duplicate.
        workspace.update_in(cx, |workspace, window, cx| {
            open_board(workspace, dir.path().to_path_buf(), window, cx);
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<TaskBoard>(cx).count(), 1);
        });
    }

    #[gpui::test]
    async fn test_drop_card_moves_state_and_orders_within_column(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, cx) = tasks_workspace(cx, dir.path()).await;
        let (a, b, d) = {
            let c = open(dir.path()).unwrap();
            let a = tasks::create_task(&c, "a").unwrap();
            let b = tasks::create_task(&c, "b").unwrap();
            let d = tasks::create_task(&c, "d").unwrap();
            (a, b, d)
        };
        workspace.update_in(cx, |workspace, window, cx| {
            open_board(workspace, dir.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();
        let board = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<TaskBoard>(cx).next().expect("board")
        });

        // d, b, a in Backlog (newest on top). Dropping the middle card (b)
        // on itself is a pick-up-and-release-in-place gesture: order must
        // not change.
        board.update_in(cx, |board, window, cx| {
            board.drop_card(b, tasks::TaskState::Backlog, Some(b), window, cx)
        });
        cx.run_until_parked();
        board.read_with(cx, |board, _| {
            assert_eq!(board.column(tasks::TaskState::Backlog), [d, b, a]);
        });

        // Drag a to Review (empty column).
        board.update_in(cx, |board, window, cx| {
            board.drop_card(a, tasks::TaskState::Review, None, window, cx)
        });
        cx.run_until_parked();
        board.read_with(cx, |board, _| {
            assert_eq!(board.column(tasks::TaskState::Review), [a]);
            assert_eq!(board.column(tasks::TaskState::Backlog), [d, b]);
        });

        // Drag d below-target: drop ON b => lands above b.
        board.update_in(cx, |board, window, cx| {
            board.drop_card(d, tasks::TaskState::Backlog, Some(b), window, cx)
        });
        cx.run_until_parked();
        board.read_with(cx, |board, _| {
            assert_eq!(board.column(tasks::TaskState::Backlog), [d, b]);
        });

        // Drop b at Review column end => after a.
        board.update_in(cx, |board, window, cx| {
            board.drop_card(b, tasks::TaskState::Review, None, window, cx)
        });
        cx.run_until_parked();
        board.read_with(cx, |board, _| {
            assert_eq!(board.column(tasks::TaskState::Review), [a, b]);
        });
    }
}
