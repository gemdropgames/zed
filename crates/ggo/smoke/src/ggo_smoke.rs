//! End-to-end smoke tests for the GGO panels: whole user journeys driven
//! through the same public entry points the app uses -- pane drops, the
//! workspace's fork interceptors, keystrokes through the real keymap --
//! asserting the observable outcomes (files written, tabs opened) rather
//! than panel internals.
//!
//! The per-feature regression tests live in each panel crate; this crate
//! exists for the class of bug they cannot see, where every feature is
//! correct in isolation and wrong in combination.

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace, Workspace};

    use gpui::Entity;

    /// A real-fs emerald project the panels' `std::fs` reads can see,
    /// mirrored into the `FakeFs` the worktree walks. One decodable
    /// red/blue PNG at `assets/art/hero.png` -- INSIDE the asset tree, so
    /// the wizard's default destination resolves against `assets/` the way
    /// it does for a project's own art.
    fn write_project(root: &Path) {
        std::fs::create_dir_all(root.join("assets/art")).unwrap();
        std::fs::write(root.join("emerald.toml"), "").unwrap();
        let mut encoded = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut encoded),
            &[255u8, 0, 0, 255, 0, 0, 255, 255].repeat(128), // 16x16 halves
            16,
            16,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        std::fs::write(root.join("assets/art/hero.png"), &encoded).unwrap();
        std::fs::write(root.join("assets/notes.txt"), "notes\n").unwrap();
    }

    /// The app as a user gets it: every panel's `init` registered, the
    /// default keymap bound, a real window around a real project.
    async fn boot<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
    ) -> (Entity<Workspace>, &'a mut gpui::VisualTestContext) {
        write_project(root);
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            ggo_import_panel::init(cx);
            ggo_tileset_panel::init(cx);
            ggo_sprite_panel::init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            serde_json::json!({
                "emerald.toml": "",
                "assets": { "art": { "hero.png": "" }, "notes.txt": "notes\n" },
            }),
        )
        .await;
        let project = Project::test(fs, [root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        (workspace, cx)
    }

    /// The full import pipeline, to the bytes on disk: drop the PNG, let
    /// the wizard claim it, commit, and reopen what it wrote through
    /// worldlib's own reader.
    #[gpui::test]
    async fn smoke_import_pipeline_writes_a_tileset_worldlib_can_reopen(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;

        let png = dir.path().join("assets/art/hero.png");
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_external_paths_drop(&gpui::ExternalPaths(vec![png].into()), window, cx)
        });
        cx.run_until_parked();

        let wizard = workspace.read_with(cx, |workspace, cx| {
            let item = workspace
                .items_of_type::<ggo_import_panel::ImportItem>(cx)
                .next()
                .expect("the wizard tab exists");
            item.read(cx).panel().clone()
        });
        cx.run_until_parked();
        let asset_rel = wizard.update(cx, |wizard, cx| {
            assert!(wizard.test_is_ready(), "the wizard decoded the drop");
            wizard.test_commit(cx).expect("the commit succeeds")
        });

        let til = dir.path().join("assets").join(&asset_rel);
        assert!(til.is_file(), "{til:?} was written");
        let opened =
            ggo_worldlib::sprites::io::open_tileset(dir.path(), &format!("assets/{asset_rel}"))
                .expect("worldlib reopens what the wizard wrote");
        assert_eq!(opened.tile_count, 1, "the 16x16 source is one tile");
    }

    /// The edit pipeline through the REAL keymap: open the imported
    /// tileset, paint with a mouse click, cycle the palette with `.`,
    /// paint again, save with ctrl-s, and reopen the file from disk.
    #[gpui::test]
    async fn smoke_keystroke_edit_and_save_round_trips_through_the_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;

        // Import first, through the same pipeline as the journey above.
        let png = dir.path().join("assets/art/hero.png");
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_external_paths_drop(&gpui::ExternalPaths(vec![png].into()), window, cx)
        });
        cx.run_until_parked();
        let wizard = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_import_panel::ImportItem>(cx)
                .next()
                .expect("wizard")
                .read(cx)
                .panel()
                .clone()
        });
        cx.run_until_parked();
        let asset_rel = wizard.update(cx, |wizard, cx| {
            wizard.test_commit(cx).expect("commit succeeds")
        });
        cx.run_until_parked();

        // Open the written .til the way the app does: the project panel's
        // click funnel offers the path to the fork's open interceptor,
        // which claims it and opens the tileset tab.
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project::ProjectPath {
                    worktree_id,
                    path: path::rel_path::rel_path(&format!("assets/{asset_rel}")).into_arc(),
                },
                window,
                cx,
            )
        });
        assert!(claimed, "the tileset interceptor claimed the .til");
        cx.run_until_parked();
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_tileset_panel::TilesetEditorItem>(cx)
                .next()
                .expect("the interceptor opened the tileset tab")
                .read(cx)
                .test_panel()
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_ready(), "the tileset loaded");
        });

        let Some((bounds, zoom)) = panel.read_with(cx, |panel, _| {
            panel
                .test_sheet_bounds()
                .map(|bounds| (bounds, panel.test_zoom() as f32))
        }) else {
            panic!("the sheet never painted, so its bounds were not recorded");
        };
        let at = |x: f32, y: f32| {
            gpui::point(
                bounds.origin.x + gpui::px((x + 0.5) * zoom),
                bounds.origin.y + gpui::px((y + 0.5) * zoom),
            )
        };

        // Click paints the default slot 1.
        cx.simulate_mouse_down(at(1.0, 1.0), gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(at(1.0, 1.0), gpui::MouseButton::Left, Default::default());
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(0, 1, 1), Some(1), "the click painted");
        });

        // `.` steps the palette through the REAL keymap; paint slot 2.
        cx.simulate_keystrokes(".");
        cx.simulate_mouse_down(at(3.0, 3.0), gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(at(3.0, 3.0), gpui::MouseButton::Left, Default::default());
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_pixel(0, 3, 3),
                Some(2),
                "NextSlot resolved through the keymap and the paint used it"
            );
        });

        // ctrl-s through the keymap; the document is clean and the file
        // holds both edits.
        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_dirty(), "the save landed");
        });
        let reopened =
            ggo_worldlib::sprites::io::open_tileset(dir.path(), &format!("assets/{asset_rel}"))
                .expect("reopen");
        let tile_pixels = ggo_worldlib::sprites::tileset_doc::TILE_PX;
        assert_eq!(reopened.indices[tile_pixels + 1], 1, "edit one survived");
        assert_eq!(
            reopened.indices[3 * tile_pixels + 3],
            2,
            "edit two survived"
        );
    }

    /// Import-as-sprite writes the `.spr` trio and worldlib reopens it.
    #[gpui::test]
    async fn smoke_sprite_import_writes_a_trio_worldlib_can_reopen(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;

        let png = dir.path().join("assets/art/hero.png");
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_external_paths_drop(&gpui::ExternalPaths(vec![png].into()), window, cx)
        });
        cx.run_until_parked();
        let wizard = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_import_panel::ImportItem>(cx)
                .next()
                .expect("wizard")
                .read(cx)
                .panel()
                .clone()
        });
        cx.run_until_parked();
        let asset_rel = wizard.update(cx, |wizard, cx| {
            wizard.test_set_as_sprite(true);
            wizard.test_set_frame_tiles((Some(1), None));
            wizard.test_commit(cx).expect("commit succeeds")
        });
        assert!(asset_rel.ends_with(".spr"), "a sprite commit: {asset_rel}");

        // Sprite sidecars are stored asset-root-relative, so the reopen is
        // framed against `assets/`, not the worktree root.
        let opened = ggo_worldlib::sprites::io::open_sprite(&dir.path().join("assets"), &asset_rel)
            .expect("worldlib reopens the trio");
        assert_eq!(opened.state.frames.len(), 1, "one 16x16 frame at 1 tile");
    }

    /// The journey that crashed the editor this morning: a `.png` dropped
    /// on a pane whose active item is an EDITOR. The drop reaches the
    /// fork's interceptor through `Pane::handle_external_paths_drop` with
    /// the pane leased, the wizard open defers, and the wizard tab exists
    /// afterwards -- no panic, editor still present.
    #[gpui::test]
    async fn smoke_dropping_a_png_on_an_editor_opens_the_import_wizard(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;

        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .expect("one worktree")
                .read(cx)
                .id()
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    project::ProjectPath {
                        worktree_id,
                        path: path::rel_path::rel_path("assets/notes.txt").into_arc(),
                    },
                    None,
                    true,
                    window,
                    cx,
                )
            })
            .await
            .expect("the editor opens");

        let png = dir.path().join("assets/art/hero.png");
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.handle_external_paths_drop(&gpui::ExternalPaths(vec![png].into()), window, cx)
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<ggo_import_panel::ImportItem>(cx)
                    .count(),
                1,
                "the drop was claimed and the wizard tab exists"
            );
        });
    }
}
