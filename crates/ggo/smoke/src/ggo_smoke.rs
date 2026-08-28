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

    /// A hand-built two-tile tileset -- tile 0 transparent, tile 1 solid
    /// slot 1 -- written through worldlib and opened through the fork's
    /// path-open interceptor, exactly as a click in the project panel
    /// would. Decoupled from the import quantizer so the float journeys
    /// assert against known pixels.
    async fn open_fixture_tileset(
        workspace: &Entity<Workspace>,
        root: &Path,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<ggo_tileset_panel::TilesetPanel> {
        let tile_pixels = ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
        let mut indices = vec![0u8; 2 * tile_pixels];
        for value in &mut indices[tile_pixels..] {
            *value = 1;
        }
        let mut palette = [0u16; 16];
        palette[1] = 0xF800;
        palette[2] = 0x07E0;
        ggo_worldlib::sprites::io::save_tileset(root, "assets/art/fx.til", &indices, 2, &palette)
            .unwrap();

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
                    path: path::rel_path::rel_path("assets/art/fx.til").into_arc(),
                },
                window,
                cx,
            )
        });
        assert!(claimed, "the tileset interceptor claimed the fixture");
        cx.run_until_parked();
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_tileset_panel::TilesetEditorItem>(cx)
                .next()
                .expect("tileset tab")
                .read(cx)
                .test_panel()
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert!(panel.test_is_ready()));
        panel
    }

    /// Screen point of tile 1's pixel `(x, y)` on the fixture sheet.
    fn tile1_at(
        panel: &Entity<ggo_tileset_panel::TilesetPanel>,
        cx: &mut gpui::VisualTestContext,
        x: f32,
        y: f32,
    ) -> gpui::Point<gpui::Pixels> {
        panel.read_with(cx, |panel, _| {
            let bounds = panel.test_sheet_bounds().expect("sheet painted");
            let zoom = panel.test_zoom() as f32;
            gpui::point(
                bounds.origin.x + gpui::px((16.0 + x + 0.5) * zoom),
                bounds.origin.y + gpui::px((y + 0.5) * zoom),
            )
        })
    }

    /// Drag a 2x2 marquee over tile 1's top-left with the Select tool,
    /// through the real keymap (`m`) and real mouse events.
    fn marquee_tile1(
        panel: &Entity<ggo_tileset_panel::TilesetPanel>,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.simulate_keystrokes("m");
        let from = tile1_at(panel, cx, 0.0, 0.0);
        let to = tile1_at(panel, cx, 1.0, 1.0);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
    }

    /// Float lifecycle, cancel branch: arrow keys lift the marquee into a
    /// float, escape drops it, and the document was never touched.
    #[gpui::test]
    async fn smoke_float_escape_cancels_without_touching_the_document(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        marquee_tile1(&panel, cx);
        cx.simulate_keystrokes("right right right");
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_floating(), "the nudges lifted a float");
            assert!(!panel.test_is_dirty(), "without touching the document");
        });

        cx.simulate_keystrokes("escape");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "escape dropped it");
            assert!(!panel.test_is_dirty());
            assert_eq!(panel.test_pixel(1, 0, 0), Some(1), "art untouched");
        });
    }

    /// Commit branch: nudge, click away to place, and the whole move is
    /// ONE undo step through ctrl-z.
    #[gpui::test]
    async fn smoke_float_commit_by_click_away_is_one_undo_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        marquee_tile1(&panel, cx);
        cx.simulate_keystrokes("right right right right");
        let away = tile1_at(&panel, cx, 12.0, 12.0);
        cx.simulate_mouse_down(away, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(away, gpui::MouseButton::Left, Default::default());

        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "the click placed it");
            assert_eq!(panel.test_pixel(1, 0, 0), Some(0), "source vacated");
            assert_eq!(panel.test_pixel(1, 4, 0), Some(1), "landed 4 right");
        });

        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(1, 0, 0), Some(1), "ONE undo restored");
            assert!(!panel.test_is_dirty());
        });
    }

    /// Copy branch: alt-drag inside the marquee, place -- the source
    /// survives, and one undo removes only the copy.
    #[gpui::test]
    async fn smoke_float_alt_drag_copies(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        marquee_tile1(&panel, cx);
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        // Drag INTO tile 0's transparent ground, where a landed copy is
        // distinguishable -- tile 1's own interior is already solid 1.
        let zoom = panel.read_with(cx, |panel, _| panel.test_zoom()) as f32;
        let from = tile1_at(&panel, cx, 0.0, 0.0);
        let to = gpui::point(from.x - gpui::px(8.0 * zoom), from.y);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, alt);
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());

        let away = tile1_at(&panel, cx, 12.0, 12.0);
        cx.simulate_mouse_down(away, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(away, gpui::MouseButton::Left, Default::default());

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(1, 0, 0), Some(1), "source survived");
            assert_eq!(panel.test_pixel(0, 8, 0), Some(1), "copy landed in tile 0");
        });
        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(0, 8, 0), Some(0), "undo took the copy");
            assert_eq!(panel.test_pixel(1, 0, 0), Some(1), "and left the source");
        });
    }

    /// Swap branch: paint a marker in tile 0, shift-drag tile 1's corner
    /// onto it, place -- an exact exchange, one undo restores both sides.
    #[gpui::test]
    async fn smoke_float_shift_drag_swaps(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        // A slot-2 marker at tile 0 (0,0): `.` steps 1 -> 2 on the keymap.
        cx.simulate_keystrokes("b .");
        let marker = {
            let base = tile1_at(&panel, cx, 0.0, 0.0);
            gpui::point(
                base.x - gpui::px(16.0 * panel.read_with(cx, |p, _| p.test_zoom()) as f32),
                base.y,
            )
        };
        cx.simulate_mouse_down(marker, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(marker, gpui::MouseButton::Left, Default::default());
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(0, 0, 0), Some(2), "marker painted");
        });

        marquee_tile1(&panel, cx);
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let from = tile1_at(&panel, cx, 0.0, 0.0);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, shift);
        cx.simulate_mouse_move(marker, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(marker, gpui::MouseButton::Left, Default::default());
        let away = tile1_at(&panel, cx, 12.0, 12.0);
        cx.simulate_mouse_down(away, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(away, gpui::MouseButton::Left, Default::default());

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(0, 0, 0), Some(1), "tile 1's art arrived");
            assert_eq!(panel.test_pixel(1, 0, 0), Some(2), "the marker went back");
        });
        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_pixel(0, 0, 0),
                Some(2),
                "one undo undid the swap"
            );
            assert_eq!(panel.test_pixel(1, 0, 0), Some(1));
        });
    }

    /// Undo-while-floating branch (the review's finding, by keystrokes):
    /// ctrl-z with a float pending cancels the float and rewinds nothing.
    #[gpui::test]
    async fn smoke_float_undo_cancels_the_pending_float(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        // A real prior op that a mis-routed undo would visibly rewind.
        cx.simulate_keystrokes("b .");
        let dot = tile1_at(&panel, cx, 14.0, 14.0);
        cx.simulate_mouse_down(dot, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(dot, gpui::MouseButton::Left, Default::default());

        marquee_tile1(&panel, cx);
        cx.simulate_keystrokes("right");
        panel.read_with(cx, |panel, _| assert!(panel.test_is_floating()));

        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "undo consumed the float");
            assert_eq!(
                panel.test_pixel(1, 14, 14),
                Some(2),
                "and rewound NOTHING underneath it"
            );
        });
    }

    /// Delete branch: delete with a lifted float blanks its origin in one
    /// step; duplicate appends a copy of the marquee'd tile.
    #[gpui::test]
    async fn smoke_float_delete_blanks_and_duplicate_appends(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        marquee_tile1(&panel, cx);
        cx.simulate_keystrokes("right delete");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating());
            assert_eq!(panel.test_pixel(1, 0, 0), Some(0), "origin blanked");
        });
        cx.simulate_keystrokes("ctrl-z");

        marquee_tile1(&panel, cx);
        cx.simulate_keystrokes("ctrl-d");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_tile_count(), 3, "duplicate appended a tile");
            assert_eq!(panel.test_pixel(2, 0, 0), Some(1), "with tile 1's art");
        });
    }

    /// One `ProjectPath` in the primary worktree.
    fn project_path(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        rel: &str,
    ) -> project::ProjectPath {
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
        project::ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    /// Offer `rel` to the fork's path-open interceptors, the project
    /// panel's exact funnel call.
    fn offer_open(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        rel: &str,
    ) -> bool {
        let path = project_path(workspace, cx, rel);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&path, window, cx)
        })
    }

    /// Boot with EVERY panel registered, for the routing journeys.
    async fn boot_all<'a>(
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
            ggo_map_panel::init(cx);
            ggo_world_panel::init(cx);
            ggo_audio_panel::init(cx);
            ggo_emu_panel::init(cx);
            ggo_emerald_panel::init(cx);
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

    /// The routing spine, every registered claimer at once: each asset
    /// kind lands in ITS panel's tab, and a plain text file is claimed by
    /// none of them -- the fall-through that keeps normal editing working
    /// with the whole fork loaded.
    #[gpui::test]
    async fn smoke_every_asset_kind_routes_to_its_panel_and_text_falls_through(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        let root = dir.path();

        // Fixtures for every claimer, written through worldlib where a
        // real decoder sits behind the tab.
        let tile_pixels = ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
        ggo_worldlib::sprites::io::save_tileset(
            root,
            "assets/art/fx.til",
            &vec![1u8; tile_pixels],
            1,
            &[0u16; 16],
        )
        .unwrap();
        std::fs::create_dir_all(root.join("assets/maps")).unwrap();
        ggo_worldlib::sprites::io::save_new_map(root, "assets/maps/lvl.map", 8, 8).unwrap();
        std::fs::create_dir_all(root.join("assets/audio")).unwrap();
        std::fs::write(root.join("assets/audio/hit.wav"), b"RIFF").unwrap();
        std::fs::create_dir_all(root.join("worlds")).unwrap();
        std::fs::write(root.join("worlds/overworld.toml"), "").unwrap();
        std::fs::write(root.join("game.cart"), b"").unwrap();

        assert!(offer_open(&workspace, cx, "assets/art/fx.til"), "tileset");
        assert!(offer_open(&workspace, cx, "assets/maps/lvl.map"), "map");
        assert!(offer_open(&workspace, cx, "assets/audio/hit.wav"), "audio");
        assert!(offer_open(&workspace, cx, "worlds/overworld.toml"), "world");
        assert!(offer_open(&workspace, cx, "game.cart"), "cart");
        assert!(
            !offer_open(&workspace, cx, "assets/notes.txt"),
            "text falls through to the plain editor with the whole fork loaded"
        );
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<ggo_tileset_panel::TilesetEditorItem>(cx)
                    .count(),
                1
            );
            assert_eq!(
                workspace
                    .items_of_type::<ggo_map_panel::MapEditorItem>(cx)
                    .count(),
                1
            );
            assert_eq!(
                workspace
                    .items_of_type::<ggo_audio_panel::AudioItem>(cx)
                    .count(),
                1
            );
            assert_eq!(
                workspace
                    .items_of_type::<ggo_world_panel::WorldCanvasItem>(cx)
                    .count(),
                1
            );
            assert_eq!(
                workspace
                    .items_of_type::<ggo_emu_panel::EmulatorItem>(cx)
                    .count(),
                1
            );
        });
    }

    /// The sprite pipeline closes its loop: the trio the import wizard
    /// wrote opens in the sprite panel when its `.spr` is clicked.
    #[gpui::test]
    async fn smoke_imported_sprite_opens_in_the_sprite_panel(cx: &mut TestAppContext) {
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
        cx.run_until_parked();

        assert!(
            offer_open(&workspace, cx, &format!("assets/{asset_rel}")),
            "the sprite interceptor claims the written .spr"
        );
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<ggo_sprite_panel::SpriteEditorItem>(cx)
                    .count(),
                1,
                "and the sprite tab exists"
            );
        });
    }

    /// New Map, the authoring path: the blank map worldlib writes opens in
    /// the map panel, and worldlib reopens what was created.
    #[gpui::test]
    async fn smoke_new_map_opens_and_round_trips(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        std::fs::create_dir_all(dir.path().join("assets/maps")).unwrap();
        ggo_worldlib::sprites::io::save_new_map(dir.path(), "assets/maps/lvl.map", 8, 8).unwrap();

        assert!(offer_open(&workspace, cx, "assets/maps/lvl.map"));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<ggo_map_panel::MapEditorItem>(cx)
                    .count(),
                1,
                "the map tab exists"
            );
        });

        let reopened = ggo_worldlib::sprites::io::open_map(dir.path(), "assets/maps/lvl.map")
            .expect("worldlib reopens the new map");
        assert_eq!((reopened.w, reopened.h), (8, 8));
    }

    /// Install the emerald dock panel with a stubbed `emd` runner that
    /// records every request and "scaffolds" by writing the project dir
    /// itself, then answers as the stub decides.
    fn install_emerald_stub(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        ok: bool,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>> {
        use std::sync::{Arc, Mutex};
        let calls: Arc<Mutex<Vec<ggo_common::ProcRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let runner: ggo_emerald_panel::TestEmdRunner =
            Arc::new(move |request: ggo_common::ProcRequest| {
                if ok
                    && request.args.first().map(String::as_str) == Some("new")
                    && let Some(name) = request.args.get(1)
                {
                    let dest = request.cwd.join(name);
                    std::fs::create_dir_all(&dest).unwrap();
                    std::fs::write(dest.join("emerald.toml"), "").unwrap();
                }
                recorded.lock().unwrap().push(request);
                let outcome = ggo_worldlib::emerald::EmdRunOutcome {
                    ok,
                    output: if ok {
                        "created".into()
                    } else {
                        "boom: emd said no".into()
                    },
                    result: None,
                };
                Box::pin(async move { outcome })
            });
        // init() already installed the dock panel; swap the seam on the
        // one the action handler will actually reach for.
        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<ggo_emerald_panel::EmeraldPanel>(cx)
                .expect("init installed the emerald panel");
            panel.update(cx, |panel, _| panel.test_set_runner(runner));
        });
        calls
    }

    /// NewProject, the real surface end to end: the action fires the
    /// native new-path dialog (simulated), the handler builds `emd new
    /// <name>` in the chosen parent, and the stubbed scaffolder's output
    /// exists on disk afterwards.
    #[gpui::test]
    async fn smoke_new_project_scaffolds_through_the_stubbed_runner(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        let calls = install_emerald_stub(&workspace, cx, true);

        let dest = dir.path().join("newgame");
        let chosen = dest.clone();
        // The dialog opens when the action runs; the simulation answers
        // the pending prompt.
        cx.dispatch_action(ggo_emerald_panel::NewProject);
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one emd invocation");
        assert_eq!(
            calls[0].args,
            vec!["new".to_string(), "newgame".to_string()],
            "emd new <name>"
        );
        assert_eq!(calls[0].cwd, dir.path(), "run in the chosen parent");
        assert!(
            dest.join("emerald.toml").is_file(),
            "the scaffold exists where the dialog pointed"
        );
    }

    /// The failure branch: the scaffolder says no, nothing is created,
    /// and the workspace survives to show the toast.
    #[gpui::test]
    async fn smoke_new_project_failure_leaves_nothing_behind(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        let calls = install_emerald_stub(&workspace, cx, false);

        let dest = dir.path().join("doomed");
        let chosen = dest.clone();
        // The dialog opens when the action runs; the simulation answers
        // the pending prompt.
        cx.dispatch_action(ggo_emerald_panel::NewProject);
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();

        assert_eq!(calls.lock().unwrap().len(), 1, "emd was asked");
        assert!(!dest.exists(), "and nothing was left behind");
        workspace.read_with(cx, |_, _| ());
    }

    /// Alt's precedence rule, end to end: alt only starts a copy-drag
    /// INSIDE a Select marquee. Anywhere else it keeps its sampling
    /// meaning, and the two must not blur into each other -- an alt-click
    /// on distant art that lifted a float instead of sampling would
    /// silently arm a paste nobody asked for.
    #[gpui::test]
    async fn smoke_alt_click_outside_the_marquee_samples_instead_of_lifting(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        let zoom = panel.read_with(cx, |panel, _| panel.test_zoom()) as f32;
        let tile0_at = |cx: &mut gpui::VisualTestContext, x: f32| {
            let base = tile1_at(&panel, cx, x, 0.0);
            gpui::point(base.x - gpui::px(16.0 * zoom), base.y)
        };
        let click = |cx: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>| {
            cx.simulate_mouse_down(at, gpui::MouseButton::Left, Default::default());
            cx.simulate_mouse_up(at, gpui::MouseButton::Left, Default::default());
        };

        // A slot-2 marker in tile 0 to sample from later.
        cx.simulate_keystrokes("b .");
        let marker = tile0_at(cx, 0.0);
        click(cx, marker);
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(0, 0, 0), Some(2), "marker painted");
        });

        // Step back to slot 1 and prove it, so the sample below has
        // something to change.
        cx.simulate_keystrokes(",");
        let calibration = tile0_at(cx, 8.0);
        click(cx, calibration);
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(0, 8, 0), Some(1), "the slot is back to 1");
        });

        marquee_tile1(&panel, cx);
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        cx.simulate_mouse_down(marker, gpui::MouseButton::Left, alt);
        cx.simulate_mouse_up(marker, gpui::MouseButton::Left, Default::default());
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "no copy-drag started out here");
            assert_eq!(
                panel.test_pixel(0, 1, 0),
                Some(0),
                "and nothing was painted"
            );
        });

        // The sample landed: the pencil now writes the marker's slot.
        cx.simulate_keystrokes("b");
        let proof = tile0_at(cx, 12.0);
        click(cx, proof);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_pixel(0, 12, 0),
                Some(2),
                "the alt-click sampled the marker's slot instead of lifting"
            );
        });
    }

    /// A modifier click inside the marquee that never becomes a drag must
    /// not arm the NEXT gesture. The mouse-up resets the pending mode, so
    /// the following keyboard nudge lifts a plain Move -- a leaked Swap or
    /// Copy would show up here as art the commit failed to vacate.
    #[gpui::test]
    async fn smoke_modifier_click_without_a_drag_commits_as_a_move(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        marquee_tile1(&panel, cx);
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let inside = tile1_at(&panel, cx, 0.0, 0.0);
        cx.simulate_mouse_down(inside, gpui::MouseButton::Left, shift);
        cx.simulate_mouse_up(inside, gpui::MouseButton::Left, Default::default());
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "no movement, no float");
        });

        cx.simulate_keystrokes("right right right right");
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_floating(), "the nudges lifted a float");
        });
        let away = tile1_at(&panel, cx, 12.0, 12.0);
        cx.simulate_mouse_down(away, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(away, gpui::MouseButton::Left, Default::default());

        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "the click away placed it");
            assert_eq!(
                panel.test_pixel(1, 0, 0),
                Some(0),
                "a Move vacated the origin: the stale Swap intent did not leak"
            );
            assert_eq!(panel.test_pixel(1, 4, 0), Some(1), "landed 4 right");
        });
    }

    /// The two cancel routes off a modifier float, which differ from a
    /// Move's: escape drops a Swap without exchanging anything, and delete
    /// on a Copy is a cancel rather than a source-blank, because neither
    /// float ever owed the document a write.
    #[gpui::test]
    async fn smoke_escape_cancels_a_swap_and_delete_cancels_a_copy(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let panel = open_fixture_tileset(&workspace, dir.path(), cx).await;

        let zoom = panel.read_with(cx, |panel, _| panel.test_zoom()) as f32;
        cx.simulate_keystrokes("b .");
        let marker = {
            let base = tile1_at(&panel, cx, 0.0, 0.0);
            gpui::point(base.x - gpui::px(16.0 * zoom), base.y)
        };
        cx.simulate_mouse_down(marker, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(marker, gpui::MouseButton::Left, Default::default());
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_pixel(0, 0, 0), Some(2), "marker painted");
        });

        marquee_tile1(&panel, cx);
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let from = tile1_at(&panel, cx, 0.0, 0.0);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, shift);
        cx.simulate_mouse_move(marker, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(marker, gpui::MouseButton::Left, Default::default());
        cx.simulate_keystrokes("escape");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "escape dropped the swap");
            assert_eq!(panel.test_pixel(0, 0, 0), Some(2), "the marker stayed");
            assert_eq!(panel.test_pixel(1, 0, 0), Some(1), "and so did tile 1");
        });

        // Escape also drops the marquee, so the copy drag needs a new one.
        marquee_tile1(&panel, cx);
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        let from = tile1_at(&panel, cx, 0.0, 0.0);
        let to = gpui::point(from.x - gpui::px(8.0 * zoom), from.y);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, alt);
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
        cx.simulate_keystrokes("delete");

        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_floating(), "delete consumed the copy");
            assert_eq!(
                panel.test_pixel(1, 0, 0),
                Some(1),
                "deleting a copy is a cancel, not a source-blank"
            );
            assert_eq!(panel.test_pixel(0, 8, 0), Some(0), "and nothing stamped");
        });
    }

    /// A two-frame sprite imported through the wizard, opened in the
    /// sprite panel: a 32x16 PNG of two SOLID halves (red, blue) framed
    /// at one tile wide, so the trio lands with two distinct pool tiles
    /// (nothing to dedup) and two one-cell frames -- frame 0 showing
    /// tile 0, frame 1 showing tile 1. Known pixels the edit journeys
    /// below can assert against without depending on the quantizer.
    async fn open_imported_duo_sprite(
        workspace: &Entity<Workspace>,
        root: &Path,
        cx: &mut gpui::VisualTestContext,
    ) -> (Entity<ggo_sprite_panel::SpritePanel>, String) {
        // Row-major: each of the 16 rows is 16 red pixels then 16 blue
        // ones, which is the two solid halves side by side.
        let row: Vec<u8> = [255u8, 0, 0, 255]
            .repeat(16)
            .into_iter()
            .chain([0u8, 0, 255, 255].repeat(16))
            .collect();
        let rgba = row.repeat(16);
        let mut encoded = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut encoded),
            &rgba,
            32,
            16,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        let png = root.join("assets/art/duo.png");
        std::fs::write(&png, &encoded).unwrap();

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
        assert!(asset_rel.ends_with(".spr"), "the wizard wrote a sprite");
        cx.run_until_parked();

        assert!(
            offer_open(workspace, cx, &format!("assets/{asset_rel}")),
            "the sprite interceptor claims the written .spr"
        );
        cx.run_until_parked();
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_sprite_panel::SpriteEditorItem>(cx)
                .next()
                .expect("sprite tab")
                .read(cx)
                .test_panel()
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_ready(), "the sprite panel loaded the trio");
            assert_eq!(
                panel.test_frame_cell(0, 0),
                Some(0),
                "frame 0 shows the red tile"
            );
            assert_eq!(
                panel.test_frame_cell(1, 0),
                Some(1),
                "frame 1 shows the blue tile"
            );
        });
        (panel, asset_rel)
    }

    /// Screen point at the center of picker sheet cell `col` of the first
    /// row -- the picker lays 24px squares out left-to-right.
    fn picker_cell_center(
        panel: &Entity<ggo_sprite_panel::SpritePanel>,
        cx: &mut gpui::VisualTestContext,
        col: usize,
    ) -> gpui::Point<gpui::Pixels> {
        panel.read_with(cx, |panel, _| {
            let bounds = panel.test_picker_bounds().expect("picker painted");
            gpui::point(
                bounds.origin.x + gpui::px(col as f32 * 24.0 + 12.0),
                bounds.origin.y + gpui::px(12.0),
            )
        })
    }

    /// Screen point at the center of the frame preview. The fixture's
    /// frames are one tile, so any point inside the preview is cell 0.
    fn preview_center(
        panel: &Entity<ggo_sprite_panel::SpritePanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> gpui::Point<gpui::Pixels> {
        panel.read_with(cx, |panel, _| {
            let bounds = panel.test_preview_bounds().expect("preview painted");
            bounds.center()
        })
    }

    /// A picker click, the way a user makes one: press and release on the
    /// same cell (a press-drag-release would be a marquee instead).
    fn click(cx: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>) {
        cx.simulate_mouse_down(at, gpui::MouseButton::Left, Default::default());
        cx.simulate_mouse_up(at, gpui::MouseButton::Left, Default::default());
    }

    /// The sprite editing spine, end to end through the real keymap: pick
    /// a pool tile out of the picker, stamp it on the preview, walk the
    /// edit back and forth with undo/redo, then ctrl-s and reopen the
    /// trio from disk. Every panel feature here is covered in isolation
    /// by the crate's own tests -- what this journey adds is that the
    /// picker's selection, the preview's hit mapping, the undo stack, and
    /// the save path all agree about the SAME cell.
    #[gpui::test]
    async fn smoke_sprite_assign_undo_and_save_round_trip(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let (panel, asset_rel) = open_imported_duo_sprite(&workspace, dir.path(), cx).await;

        let cell1 = picker_cell_center(&panel, cx, 1);
        click(cx, cell1);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_selected_tile(),
                Some(1),
                "clicking the picker's second cell selects the blue tile"
            );
        });

        let preview = preview_center(&panel, cx);
        click(cx, preview);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_frame_cell(0, 0),
                Some(1),
                "the preview click stamped the selection on frame 0"
            );
            assert!(panel.test_is_dirty(), "and the document went dirty");
        });

        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_frame_cell(0, 0), Some(0), "undo puts red back");
        });
        cx.simulate_keystrokes("ctrl-shift-z");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_frame_cell(0, 0), Some(1), "redo re-stamps blue");
        });

        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_dirty(), "ctrl-s cleared the dirty dot");
        });

        // The trio is framed against `assets/`, not the worktree root.
        let opened = ggo_worldlib::sprites::io::open_sprite(&dir.path().join("assets"), &asset_rel)
            .expect("worldlib reopens the saved trio");
        assert_eq!(
            opened
                .state
                .frames
                .first()
                .and_then(|frame| frame.map.first()),
            Some(&1),
            "the stamped tile reached the file"
        );
    }

    /// The two "a click does nothing" branches of the same panel, which
    /// only a whole-journey test can tell apart from a broken click: with
    /// the picker deselected by escape, a preview click must leave the
    /// document alone (and CLEAN, so no empty undo step was pushed), and
    /// space must still reach the transport rather than the deselected
    /// picker.
    #[gpui::test]
    async fn smoke_sprite_deselect_and_playback_branches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot(cx, dir.path()).await;
        let (panel, _) = open_imported_duo_sprite(&workspace, dir.path(), cx).await;

        let cell1 = picker_cell_center(&panel, cx, 1);
        click(cx, cell1);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_selected_tile(), Some(1), "blue is selected");
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_selected_tile(),
                None,
                "escape drops the picker selection"
            );
        });

        let preview = preview_center(&panel, cx);
        click(cx, preview);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_frame_cell(0, 0),
                Some(0),
                "a deselected picker stamps nothing"
            );
            assert!(!panel.test_is_dirty(), "and pushes no op onto the stack");
        });

        cx.simulate_keystrokes("space");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_playing(), "space starts the transport");
        });
        cx.simulate_keystrokes("space");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_playing(), "and space again stops it");
        });
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
