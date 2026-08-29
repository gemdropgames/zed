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

    use ggo_map_panel::PaintSession;
    use ggo_worldlib::world_file;
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
        assert!(offer_open(&workspace, cx, "assets/audio/hit.wav"), "audio");
        assert!(offer_open(&workspace, cx, "worlds/overworld.toml"), "world");
        assert!(offer_open(&workspace, cx, "game.cart"), "cart");
        assert!(
            !offer_open(&workspace, cx, "assets/notes.txt"),
            "text falls through to the plain editor with the whole fork loaded"
        );
        // `.map` is deliberately UNCLAIMED since the standalone map editor
        // was retired (spec 2026-08-29): a level is painted inside the
        // world that references it, so a `.map` clicked in the project
        // panel falls through to default handling like any other file.
        // Asserted at the funnel, with every fork interceptor registered,
        // because a stray `.map` claimer is exactly what would resurrect
        // the second editing surface.
        assert!(
            !offer_open(&workspace, cx, "assets/maps/lvl.map"),
            "no fork interceptor claims a .map any more"
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

    // ------------------------------------------------ world edit journeys

    /// Entity 0's `Transform.pos` in the world fixture below. World
    /// PIXELS as `f64`, which is the world document's own coordinate type
    /// (`Transform.pos` is a TOML float array, Q16.16-snapped on write).
    const WORLD_START: [f64; 2] = [4.0, 4.0];

    /// Plain arrow-key nudge step, world px --
    /// `ggo_worldlib::drag_ops::NUDGE_STEP_PX`.
    const WORLD_NUDGE_PX: f64 = 1.0;
    /// Shift-arrow nudge step, world px --
    /// `ggo_worldlib::sprites::tileset_doc::TILE_PX`, which
    /// `drag_ops::nudge_delta` uses for the Shift case.
    const WORLD_NUDGE_TILE: f64 = 16.0;

    /// A ONE-entity world at `worlds/test.toml`, written through
    /// worldlib's own `write_world` so the panel's loader reads exactly
    /// what the format's round-trip tests produce.
    ///
    /// Deliberately one entity, no instances and no backgrounds: the
    /// journeys below assert on entity 0's position, `ctrl-a` selects
    /// entities AND instances (so an instance would change the selected
    /// count), and `DeleteSelected` puts a "Remove N instances?" confirm
    /// in front of the delete branch when the set holds any.
    fn write_world_fixture(root: &Path) {
        let doc = ggo_worldlib::world_file::WorldFile {
            entities: vec![ggo_worldlib::world_file::WorldEntity {
                components: serde_json::json!({
                    "Transform": { "pos": WORLD_START, "z": 0.0 },
                    "Text": { "content": "hi", "max_width": 16.0, "max_height": 12.0 },
                })
                .as_object()
                .expect("the fixture components are a json object")
                .clone(),
            }],
            instances: vec![],
            backgrounds: vec![],
        };
        ggo_worldlib::world_file::write_world(root, "worlds/test.toml", &doc)
            .expect("worldlib writes the world fixture");
    }

    /// Write the fixture and open it the way a user does: the project
    /// panel's click funnel offers `worlds/test.toml` to the fork's
    /// interceptors, the world interceptor claims it, loads it into the
    /// dock panel and opens the center-pane canvas tab FOCUSED -- the tab
    /// shares the panel's focus handle, which is what puts the
    /// `GgoWorldPanel` keymap context under the following keystrokes.
    async fn open_fixture_world(
        workspace: &Entity<Workspace>,
        root: &Path,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<ggo_world_panel::WorldPanel> {
        write_world_fixture(root);
        assert!(
            offer_open(workspace, cx, "worlds/test.toml"),
            "the world interceptor claimed the fixture"
        );
        cx.run_until_parked();
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_world_panel::WorldCanvasItem>(cx)
                .next()
                .expect("the interceptor opened the world canvas tab")
                .read(cx)
                .test_panel()
                .expect("the tab's panel is alive")
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_ready(), "the world loaded");
            assert_eq!(panel.test_entity_count(), 1, "the fixture's one entity");
            assert_eq!(
                panel.test_entity_position(0),
                Some(WORLD_START),
                "at the position the fixture wrote"
            );
            assert!(!panel.test_is_dirty(), "a freshly loaded world is clean");
        });
        panel
    }

    /// The world editing spine through the REAL keymap: select all, nudge
    /// by pixels and by a tile, walk the run back and forth with
    /// undo/redo, ctrl-s, and reread the toml through worldlib.
    ///
    /// Undo granularity is the panel's own, not one-per-keypress: a RUN of
    /// nudges shares a gesture id, so the store amends its top entry and
    /// ONE ctrl-z takes the whole `right right right shift-right` run back
    /// to where it started (`WorldPanel::nudge_impl`'s documented
    /// deviation from ggo-ide). The run is sealed by undo/redo, a click,
    /// or a new selection.
    #[gpui::test]
    async fn smoke_world_select_all_nudge_and_save_round_trip(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        let panel = open_fixture_world(&workspace, dir.path(), cx).await;

        cx.simulate_keystrokes("ctrl-a");
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.test_selected_count() >= 1,
                "ctrl-a selected the world's entities and instances"
            );
        });

        cx.simulate_keystrokes("right right right shift-right");
        let nudged = [
            WORLD_START[0] + 3.0 * WORLD_NUDGE_PX + WORLD_NUDGE_TILE,
            WORLD_START[1],
        ];
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_entity_position(0),
                Some(nudged),
                "three 1px nudges and one 16px tile nudge on x, nothing on y"
            );
            assert!(panel.test_is_dirty(), "and the document went dirty");
        });

        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_entity_position(0),
                Some(WORLD_START),
                "the four nudges coalesced into ONE undo entry"
            );
        });
        cx.simulate_keystrokes("ctrl-shift-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_entity_position(0),
                Some(nudged),
                "and redo replays the whole run"
            );
        });

        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_dirty(), "ctrl-s cleared the dirty flag");
        });

        let on_disk = ggo_worldlib::world_file::read_world(dir.path(), "worlds/test.toml")
            .expect("worldlib reopens what ctrl-s wrote");
        assert_eq!(on_disk.entities.len(), 1, "still one entity on disk");
        let pos = on_disk.entities[0]
            .components
            .get("Transform")
            .and_then(|t| t.get("pos"))
            .and_then(|pos| pos.as_array())
            .expect("the saved entity kept its Transform.pos");
        assert_eq!(
            (pos[0].as_f64(), pos[1].as_f64()),
            (Some(nudged[0]), Some(nudged[1])),
            "the file holds the position the panel shows"
        );
    }

    /// The three "the keystroke did something else" branches of the same
    /// panel, which only a whole-journey test tells apart from a dead
    /// binding: escape really drops the selection (so the next arrow pans
    /// the CAMERA instead of moving the entity -- the panel's documented
    /// no-selection behaviour -- and the document stays clean), ctrl-d
    /// appends a copy that one undo removes, and delete empties the world
    /// that one undo brings back intact.
    #[gpui::test]
    async fn smoke_world_escape_delete_and_duplicate_branches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        let panel = open_fixture_world(&workspace, dir.path(), cx).await;

        cx.simulate_keystrokes("ctrl-a");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_selected_count(), 1, "the one entity");
        });
        cx.simulate_keystrokes("escape");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_selected_count(), 0, "escape cleared it");
        });

        cx.simulate_keystrokes("right");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_entity_position(0),
                Some(WORLD_START),
                "an arrow with nothing selected pans the camera, not the entity"
            );
            assert!(
                !panel.test_is_dirty(),
                "and touches the document not at all"
            );
        });

        cx.simulate_keystrokes("ctrl-a ctrl-d");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_entity_count(), 2, "ctrl-d appended a copy");
        });
        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_entity_count(), 1, "one undo took the copy back");
        });

        cx.simulate_keystrokes("ctrl-a delete");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_entity_count(), 0, "delete emptied the world");
            assert_eq!(panel.test_selected_count(), 0, "and left nothing selected");
        });
        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_entity_count(), 1, "one undo brought it back");
            assert_eq!(
                panel.test_entity_position(0),
                Some(WORLD_START),
                "where it was, not at some default"
            );
        });
    }

    // ----------------------------------------- world-hosted map journeys

    /// The fixture background map's side, in cells.
    const BG_MAP_DIM: u16 = 8;

    /// The packed cell a canvas click paints with NOTHING selected in the
    /// tileset strip. The strip's anchor/far both start at `(0, 0)`, so
    /// `palette_sel_rect` + `build_stamp` give a 1x1 stamp of tile 0 with
    /// palSub 0 and no flips -- `pack_cell(0, 0, false, false)`, i.e. `0`.
    /// Blank is `CELL_BLANK` (`0x03FF`), so a painted cell and an unpainted
    /// one are never confusable.
    fn map_painted_cell() -> u16 {
        ggo_worldlib::sprites::map_doc::pack_cell(0, 0, false, false)
    }

    /// A blank cell: worldlib's own sentinel, NOT zero.
    fn map_blank_cell() -> u16 {
        ggo_worldlib::sprites::map_doc::CELL_BLANK
    }

    /// The paint fixtures' two-tile tileset at `assets/art/mapfx.til`
    /// (tile 0 transparent, tile 1 solid slot 1) -- the same hand-built
    /// sheet the float journeys use, and the ONLY `.til` under the asset
    /// root, so the layers rail's picker has exactly one entry to offer.
    fn write_paint_tileset(root: &Path) {
        let tile_pixels = ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
        let mut indices = vec![0u8; 2 * tile_pixels];
        for value in &mut indices[tile_pixels..] {
            *value = 1;
        }
        let mut palette = [0u16; 16];
        palette[1] = 0xF800;
        ggo_worldlib::sprites::io::save_tileset(
            root,
            "assets/art/mapfx.til",
            &indices,
            2,
            &palette,
        )
        .expect("worldlib writes the fixture tileset");
    }

    /// A world under `assets/worlds/`, with whatever `[[background]]`
    /// slots the journey needs, written through worldlib's own
    /// `write_world`.
    ///
    /// Under `assets/` on purpose, unlike [`write_world_fixture`]'s
    /// project-root world: the panel derives its ASSET ROOT by splitting
    /// the clicked path at its `worlds/` segment, so this fixture's root
    /// is `<project>/assets` and every path inside the documents
    /// (`maps/...`, `art/mapfx.til`) is one segment shorter than its place
    /// on disk. A world at the project root would make the two frames
    /// identical and the asset-root-relative assertions below vacuous.
    fn write_paint_world(root: &Path, stem: &str, backgrounds: Vec<world_file::Background>) {
        write_paint_tileset(root);
        world_file::write_world(
            root,
            &format!("assets/worlds/{stem}.toml"),
            &world_file::WorldFile {
                entities: vec![],
                instances: vec![],
                backgrounds,
            },
        )
        .expect("worldlib writes the world fixture");
    }

    /// The paint round-trip's world: `assets/worlds/edit.toml`, its `bg0`
    /// slot linked to an 8x8 all-blank map at `assets/maps/edit.bg0.map`
    /// that is born BOUND to the fixture tileset.
    ///
    /// Bound at write time on purpose: binding is a picker flow of its own
    /// (and the third journey below drives it), and what these two are
    /// about is the EDIT session that follows one. The `til_path` inside
    /// the map is ASSET-ROOT-relative (`art/mapfx.til`, no `assets/`
    /// segment) -- the F4 `ggo-sprfix` contract, which the save half of
    /// journey one re-reads off disk.
    fn write_paint_fixture(root: &Path) {
        write_paint_world(
            root,
            "edit",
            vec![world_file::Background {
                layer: 0,
                map: "maps/edit.bg0.map".into(),
            }],
        );
        std::fs::create_dir_all(root.join("assets/maps")).unwrap();
        ggo_worldlib::sprites::io::save_map(
            root,
            "assets/maps/edit.bg0.map",
            &ggo_worldlib::sprites::map_doc::MapState {
                w: BG_MAP_DIM,
                h: BG_MAP_DIM,
                cells: vec![map_blank_cell(); BG_MAP_DIM as usize * BG_MAP_DIM as usize],
                til_path: "art/mapfx.til".to_string(),
                pal_path: "art/mapfx.pal".to_string(),
                dirty: false,
            },
        )
        .expect("worldlib writes the fixture background map");
    }

    /// Open `rel` the way a user does -- the project panel's click funnel
    /// -- and hand back the loaded panel behind the canvas tab.
    async fn open_world_tab(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        rel: &str,
    ) -> Entity<ggo_world_panel::WorldPanel> {
        assert!(
            offer_open(workspace, cx, rel),
            "the world interceptor claimed {rel}"
        );
        cx.run_until_parked();
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_world_panel::WorldCanvasItem>(cx)
                .next()
                .expect("the interceptor opened the world canvas tab")
                .read(cx)
                .test_panel()
                .expect("the tab's panel is alive")
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_ready(), "the world loaded");
            assert!(!panel.test_is_dirty(), "a freshly loaded world is clean");
        });
        panel
    }

    /// Enter (or leave, when it is already on) paint mode on background
    /// layer `layer` the way a user does: a click on that slot's button in
    /// the layers rail. The button's label is the map's stem, so the
    /// journeys resolve it by the rail's own `debug_selector` rather than
    /// re-spelling the fixture's file name.
    fn click_bg_slot(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let button = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} painted in the layers rail"));
        click(cx, button.center());
        cx.run_until_parked();
    }

    /// Screen point at the centre of background cell `(x, y)`. A
    /// background slot's paint anchor is the world origin
    /// (`paint_target_rel`), so a cell centre is plain tile arithmetic in
    /// WORLD px -- put on screen through the panel's live camera, which is
    /// neither at the canvas origin nor at zoom 1 by default.
    fn bg_cell_center(
        panel: &Entity<ggo_world_panel::WorldPanel>,
        cx: &mut gpui::VisualTestContext,
        x: i32,
        y: i32,
    ) -> gpui::Point<gpui::Pixels> {
        let tile = ggo_worldlib::sprites::tileset_doc::TILE_PX as f64;
        panel.read_with(cx, |panel, _| {
            panel
                .test_canvas_point([(x as f64 + 0.5) * tile, (y as f64 + 0.5) * tile])
                .expect("the canvas painted")
        })
    }

    /// Click the centre of background cell `(x, y)`.
    fn click_bg_cell(
        panel: &Entity<ggo_world_panel::WorldPanel>,
        cx: &mut gpui::VisualTestContext,
        x: i32,
        y: i32,
    ) {
        let at = bg_cell_center(panel, cx, x, y);
        click(cx, at);
        cx.run_until_parked();
    }

    /// Cell `(x, y)` of the paint session editing `rel`.
    fn bg_cell(
        panel: &Entity<ggo_world_panel::WorldPanel>,
        cx: &mut gpui::VisualTestContext,
        rel: &str,
        x: usize,
        y: usize,
    ) -> u16 {
        panel.read_with(cx, |panel, _| {
            let state = panel
                .test_paint_session(rel)
                .expect("the world loaded a session for the painted map")
                .store
                .state();
            state.cells[y * state.w as usize + x]
        })
    }

    /// Switch to the Select tool the way a user does -- a click on the
    /// paint column's tool rail. The map tools are rail-only (ggo-ide has
    /// no letter hotkeys for them either), and an `IconButton`'s debug
    /// selector is its ICON name, which `MapTool::Select`
    /// (`IconName::Maximize`) shares with the PANE's zoom button. The
    /// guard pins the hit inside the world panel's paint column, so a
    /// wrong-button click fails here instead of silently doing something
    /// else.
    fn pick_select_tool(cx: &mut gpui::VisualTestContext) {
        let column = cx
            .debug_bounds("ggo-world-paint")
            .expect("the paint column painted -- the panel is in paint mode");
        let button = cx
            .debug_bounds("ICON-Maximize")
            .expect("the Select tool button painted");
        assert!(
            column.contains(&button.center()),
            "the resolved button is the paint column's Select tool, not the pane's zoom button"
        );
        click(cx, button.center());
        cx.run_until_parked();
    }

    /// The map editing spine, now hosted by the WORLD editor and driven
    /// through the real funnel: click the world open, click its `bg0` slot
    /// in the layers rail to put that map under the brush, click a canvas
    /// cell to stamp the default brush into it, walk the edit back and
    /// forth with undo/redo, ctrl-s, and reread the `.map` through
    /// worldlib.
    ///
    /// The stamp is the interesting part of the fixture: nothing is
    /// selected in the tileset strip, and the session still paints -- the
    /// strip's anchor starts at tile 0, so "no selection" is a 1x1 stamp
    /// of tile 0 rather than a dead brush.
    ///
    /// The two dirty flags are separate on purpose: painting a background
    /// dirties the SESSION, never the world document (no `WorldOp` runs),
    /// and one ctrl-s has to clean both.
    #[gpui::test]
    async fn smoke_world_paint_undo_and_save_round_trip(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        write_paint_fixture(dir.path());
        let panel = open_world_tab(&workspace, cx, "assets/worlds/edit.toml").await;

        const MAP_REL: &str = "maps/edit.bg0.map";
        click_bg_slot(cx, "ggo-world-bg-paint-0");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some(MAP_REL),
                "the rail click put the bg0 map under the brush"
            );
            let session = panel
                .test_paint_session(MAP_REL)
                .expect("and loaded its session");
            assert_eq!(
                session.store.state().til_path,
                "art/mapfx.til",
                "already bound to the fixture tileset, asset-root-relative"
            );
            assert!(
                session.tileset.is_some(),
                "and that binding resolved to real art -- an unbound session paints nothing"
            );
            assert!(!session.dirty(), "a freshly loaded map is clean");
        });

        click_bg_cell(&panel, cx, 1, 1);
        panel.read_with(cx, |panel, _| {
            assert!(
                panel
                    .test_paint_session(MAP_REL)
                    .is_some_and(PaintSession::dirty),
                "the click dirtied the map session"
            );
            assert!(
                !panel.test_is_dirty(),
                "and left the world document alone -- painting runs no WorldOp"
            );
        });
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 1, 1),
            map_painted_cell(),
            "the click stamped the default brush -- tile 0, palSub 0, no flips"
        );
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 0, 0),
            map_blank_cell(),
            "and touched no other cell"
        );

        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 1, 1),
            map_blank_cell(),
            "one undo took the whole click-gesture back"
        );
        cx.simulate_keystrokes("ctrl-shift-z");
        cx.run_until_parked();
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 1, 1),
            map_painted_cell(),
            "and redo replays it"
        );

        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel
                    .test_paint_session(MAP_REL)
                    .is_some_and(PaintSession::dirty),
                "ctrl-s cleaned the map session"
            );
            assert!(!panel.test_is_dirty(), "and the world store with it");
        });

        let on_disk = ggo_worldlib::sprites::io::open_map(&dir.path().join("assets"), MAP_REL)
            .expect("worldlib reopens what ctrl-s wrote");
        assert_eq!(
            (on_disk.w, on_disk.h),
            (BG_MAP_DIM, BG_MAP_DIM),
            "same size"
        );
        assert_eq!(
            on_disk.til_path, "art/mapfx.til",
            "the save kept the asset-root-relative binding"
        );
        let cell_index = |x: usize, y: usize| y * BG_MAP_DIM as usize + x;
        assert_eq!(
            on_disk.cells[cell_index(1, 1)],
            map_painted_cell(),
            "the file holds the cell the panel shows"
        );
        assert_eq!(
            on_disk.cells[0],
            map_blank_cell(),
            "and left every other cell blank"
        );
    }

    /// The selection branches of the world-hosted brush, which only a
    /// whole journey tells apart from a dead binding: a Select-tool drag
    /// on the WORLD canvas really settles a cell selection, escape drops
    /// the selection before it drops the MODE (two levels, one key), and a
    /// re-selected delete blanks exactly the selected cells as one undo
    /// step.
    #[gpui::test]
    async fn smoke_world_paint_select_delete_and_escape_branches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        write_paint_fixture(dir.path());
        let panel = open_world_tab(&workspace, cx, "assets/worlds/edit.toml").await;

        const MAP_REL: &str = "maps/edit.bg0.map";
        click_bg_slot(cx, "ggo-world-bg-paint-0");
        click_bg_cell(&panel, cx, 1, 1);
        click_bg_cell(&panel, cx, 2, 1);
        assert_eq!(bg_cell(&panel, cx, MAP_REL, 1, 1), map_painted_cell());
        assert_eq!(bg_cell(&panel, cx, MAP_REL, 2, 1), map_painted_cell());

        pick_select_tool(cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel
                    .test_paint_session(MAP_REL)
                    .expect("the session is live")
                    .tool,
                ggo_map_panel::MapTool::Select,
                "the rail's button switched the tool"
            );
        });

        let select_both = |cx: &mut gpui::VisualTestContext| {
            let from = bg_cell_center(&panel, cx, 1, 1);
            let to = bg_cell_center(&panel, cx, 2, 1);
            cx.simulate_mouse_down(from, gpui::MouseButton::Left, Default::default());
            cx.simulate_mouse_move(to, gpui::MouseButton::Left, Default::default());
            cx.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
            cx.run_until_parked();
        };
        let selection = |cx: &mut gpui::VisualTestContext| {
            panel.read_with(cx, |panel, _| {
                panel
                    .test_paint_session(MAP_REL)
                    .expect("the session is live")
                    .selection
            })
        };
        select_both(cx);
        assert_eq!(
            selection(cx),
            Some((1, 1, 2, 1)),
            "the drag settled a two-cell selection"
        );

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(selection(cx), None, "escape cleared the selection");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some(MAP_REL),
                "and did NOT leave paint mode while there was a selection to clear"
            );
        });

        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 1, 1),
            map_painted_cell(),
            "delete with nothing selected is a no-op"
        );
        assert_eq!(bg_cell(&panel, cx, MAP_REL, 2, 1), map_painted_cell());

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_paint_mode_rel(),
                None,
                "the SECOND escape left paint mode for entity editing"
            );
        });

        click_bg_slot(cx, "ggo-world-bg-paint-0");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some(MAP_REL),
                "the rail put the same map back under the brush"
            );
        });
        select_both(cx);
        assert_eq!(
            selection(cx),
            Some((1, 1, 2, 1)),
            "the Select tool survived the round trip through entity mode"
        );

        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 1, 1),
            map_blank_cell(),
            "delete blanked the selected cells"
        );
        assert_eq!(bg_cell(&panel, cx, MAP_REL, 2, 1), map_blank_cell());
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 3, 1),
            map_blank_cell(),
            "and nothing outside them was ever painted"
        );

        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 1, 1),
            map_painted_cell(),
            "one undo restored both cells -- the delete is ONE rect fill"
        );
        assert_eq!(bg_cell(&panel, cx, MAP_REL, 2, 1), map_painted_cell());
    }

    /// The layers rail's add flow, end to end: a world with no
    /// backgrounds, an empty slot's picker, a tileset pick that has to
    /// GENERATE the `.map` before it can link it, and a paint + ctrl-s
    /// that lands in both files at once.
    ///
    /// The generated map's name is the world's own stem plus the slot
    /// (`background_map_rel`), so the link the document gains and the file
    /// the pick wrote have to agree by construction, not by luck.
    #[gpui::test]
    async fn smoke_world_add_background_layer_journey(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        write_paint_world(dir.path(), "blank", vec![]);
        let panel = open_world_tab(&workspace, cx, "assets/worlds/blank.toml").await;
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.test_backgrounds().is_empty(),
                "the fixture world links no background at all"
            );
        });

        const MAP_REL: &str = "maps/blank.bg1.map";
        assert!(
            !dir.path().join("assets").join(MAP_REL).exists(),
            "and nothing has generated its bg1 map yet"
        );

        click_bg_slot(cx, "ggo-world-bg-slot-1");
        let entry = cx
            .debug_bounds("MENU_ITEM-art/mapfx.til")
            .expect("the empty slot's picker offered the asset root's one tileset");
        click(cx, entry.center());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_backgrounds(),
                vec![world_file::Background {
                    layer: 1,
                    map: MAP_REL.to_string(),
                }],
                "the pick linked slot 1 to the map named after the world's stem"
            );
            assert!(
                panel.test_is_dirty(),
                "the link is an undoable edit of the world document"
            );
        });
        let generated = ggo_worldlib::sprites::io::open_map(&dir.path().join("assets"), MAP_REL)
            .expect("the pick generated the map it linked");
        assert_eq!(
            generated.til_path, "art/mapfx.til",
            "born bound to the picked tileset, asset-root-relative"
        );
        assert!(
            generated.cells.iter().all(|&cell| cell == map_blank_cell()),
            "and born blank"
        );

        click_bg_slot(cx, "ggo-world-bg-paint-1");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some(MAP_REL),
                "the freshly linked slot is paintable straight away"
            );
        });
        click_bg_cell(&panel, cx, 2, 3);
        assert_eq!(
            bg_cell(&panel, cx, MAP_REL, 2, 3),
            map_painted_cell(),
            "and paints with the tileset the pick bound"
        );

        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_dirty(), "ctrl-s cleaned the world document");
            assert!(
                !panel
                    .test_paint_session(MAP_REL)
                    .is_some_and(PaintSession::dirty),
                "and the map session"
            );
        });

        let world = world_file::read_world(&dir.path().join("assets"), "worlds/blank.toml")
            .expect("worldlib reopens the saved world");
        assert_eq!(
            world.backgrounds,
            vec![world_file::Background {
                layer: 1,
                map: MAP_REL.to_string(),
            }],
            "the file carries the [[background]] link"
        );
        let saved = ggo_worldlib::sprites::io::open_map(&dir.path().join("assets"), MAP_REL)
            .expect("worldlib reopens the saved map");
        assert_eq!(
            saved.cells[3 * saved.w as usize + 2],
            map_painted_cell(),
            "and the map bytes hold the paint"
        );
    }

    // ------------------------------------------------------------- audio

    /// A real one-second PCM16 mono RIFF file at `root/rel` -- the shape
    /// `ggo_audio::decode`'s wav path reads, hand-rolled because nothing
    /// in the fork ships an audio fixture (the same reason
    /// `ggo_audio_panel`'s own tests carry this writer).
    ///
    /// The clip is a triangle wave at a DELIBERATELY tiny amplitude
    /// (+-64 of 32767, about -54 dBFS): the preview really does open the
    /// emulator pane's cpal output on a developer's box, and a full-scale
    /// tone would make every test run audible. The samples still vary, so
    /// the decode, the waveform buckets and the ADPCM bake all have real
    /// data to chew on.
    ///
    /// A full second is load-bearing: it keeps the preview thread alive
    /// far longer than the journey takes, so "playing" is observable
    /// without racing the clip's end.
    fn write_beep_wav(root: &Path, rel: &str) {
        const RATE: u32 = 32_000;
        const PERIOD: usize = 40;
        const PEAK: i32 = 64;
        let samples: Vec<i16> = (0..RATE as usize)
            .map(|i| {
                let half = (PERIOD / 2) as i32;
                let p = (i % PERIOD) as i32;
                let v = if p < half {
                    (p * 2 * PEAK / half) - PEAK
                } else {
                    PEAK - ((p - half) * 2 * PEAK / half)
                };
                v as i16
            })
            .collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk length
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&RATE.to_le_bytes());
        out.extend_from_slice(&(RATE * 2).to_le_bytes()); // bytes per second
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("rel has a parent")).unwrap();
        std::fs::write(path, out).unwrap();
    }

    /// The audio tab's transport through the REAL keymap: click a `.wav`
    /// in the explorer, let it decode and bake, then drive `space` and
    /// `l` in the `GgoAudioPanel` context.
    ///
    /// Nothing here asserts on sound. The preview writes into the
    /// emulator pane's cpal ring, which is deliberately no-device-safe
    /// (`preview.rs`: "`None` (no device) plays silently"), so on a
    /// headless box the audio goes nowhere and there is nothing to read
    /// back. What IS observable, and what this journey exists for, is
    /// that the two bindings reach the two handlers with a real decoded
    /// file behind them: `space` is a toggle rather than a restart, and
    /// `l` flips the loop flag the next preview runs with.
    #[gpui::test]
    async fn smoke_audio_open_play_and_loop_toggle(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        write_beep_wav(dir.path(), "assets/audio/beep.wav");

        assert!(
            offer_open(&workspace, cx, "assets/audio/beep.wav"),
            "the audio interceptor claims the .wav"
        );
        cx.run_until_parked();
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_audio_panel::AudioItem>(cx)
                .next()
                .expect("the interceptor opened the audio tab")
                .read(cx)
                .test_panel()
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_ready(), "the wav decoded");
            assert!(
                panel.test_is_baked(),
                "and the ADPCM bake the Baked-mode transport plays has landed"
            );
            assert!(!panel.test_is_playing(), "nothing plays until asked");
            assert!(!panel.test_is_looping(), "and a file opens one-shot");
        });

        cx.simulate_keystrokes("space");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_error(),
                None,
                "the bake had landed, so PlayStop is not the \"still baking\" no-op"
            );
            assert!(
                panel.test_is_playing(),
                "space resolved to ggo_audio::PlayStop and started a preview"
            );
        });

        cx.simulate_keystrokes("space");
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel.test_is_playing(),
                "a second space STOPS the preview -- `play_stop` branches on \
                 the live preview, it does not restart from the top"
            );
        });

        cx.simulate_keystrokes("l");
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.test_is_looping(),
                "l resolved to ggo_audio::ToggleLoop"
            );
            assert!(
                !panel.test_is_playing(),
                "toggling with nothing playing does not start playback"
            );
        });
        cx.simulate_keystrokes("l");
        panel.read_with(cx, |panel, _| {
            assert!(!panel.test_is_looping(), "and toggles back");
        });
    }

    // ---------------------------------------------------------- emulator

    /// Drive the executor until `ready`, or fail at `what`.
    ///
    /// One task per turn rather than `run_until_parked`, and a wall-clock
    /// deadline rather than a tick budget: the emulator is a REAL OS
    /// thread pacing itself to 60 Hz, so no amount of `advance_clock`
    /// moves it, and while a cart runs the executor may never go idle at
    /// all. This is `ggo_emu_panel`'s own `await_first_frame` helper,
    /// generalised over the condition -- including its
    /// `std::thread::sleep` when the executor has nothing to run, which
    /// is the only way to let the emulator thread make progress.
    fn pump_until(
        panel: &Entity<ggo_emu_panel::EmuPanel>,
        cx: &mut gpui::VisualTestContext,
        what: &str,
        ready: impl Fn(&ggo_emu_panel::EmuPanel) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !panel.read_with(cx, |panel, _| ready(panel)) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            if !cx.background_executor.tick() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// Pump until the pane's frame counter has been unchanged for a
    /// quarter second, and return it.
    ///
    /// Pause lands as a flag the emulator thread reads at a frame
    /// boundary, so frames already in flight still arrive after it: the
    /// count settles rather than stopping dead. Waiting for silence
    /// instead of a fixed window is exactly what `drive.rs`'s own
    /// `pause_parks_step_advances_one_frame_and_resume_continues` does
    /// ("a loaded box can delay the in-flight frame"), sleep and all.
    fn settled_frame_count(
        panel: &Entity<ggo_emu_panel::EmuPanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> u32 {
        let quiet = std::time::Duration::from_millis(250);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut last = panel.read_with(cx, |panel, _| panel.test_frame());
        let mut quiet_since = std::time::Instant::now();
        while quiet_since.elapsed() < quiet {
            assert!(
                std::time::Instant::now() < deadline,
                "the frame counter never settled -- frames kept arriving"
            );
            if !cx.background_executor.tick() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let now = panel.read_with(cx, |panel, _| panel.test_frame());
            if now != last {
                last = now;
                quiet_since = std::time::Instant::now();
            }
        }
        last
    }

    /// Open `rel` through the fork's interceptor and hand back its
    /// emulator panel, with the perf ingest pointed at a database inside
    /// `root` -- without that redirect a run that reaches the end of
    /// `finish_run` writes a row into the developer's real
    /// `~/.ggo/ggo_ide.db`.
    fn open_cart(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        root: &Path,
        rel: &str,
    ) -> Entity<ggo_emu_panel::EmuPanel> {
        assert!(
            offer_open(workspace, cx, rel),
            "the cart interceptor claims {rel}"
        );
        cx.run_until_parked();
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<ggo_emu_panel::EmulatorItem>(cx)
                .next()
                .expect("the interceptor opened the emulator tab")
                .read(cx)
                .test_panel()
        });
        panel.update(cx, |panel, _| {
            panel.test_set_db_path(root.join("ggo_ide.db"));
        });
        cx.run_until_parked();
        panel
    }

    /// The whole transport, on a cart that really executes: open the
    /// `.cart` from the explorer, run it with `ctrl-alt-r`, prove the
    /// green backdrop it sets reaches the pane's framebuffer, then pause,
    /// step and stop through the REAL keymap.
    ///
    /// The fixture is `ggo_emu_panel`'s own hand-assembled RV32I cart:
    /// `set_palette(0, 0, 0x07E0)` then `present`/`vsync_wait` forever.
    /// Nothing else in the fork can prove the port end to end (header
    /// parse -> XIP map -> interpret -> ecall -> PPU compose -> RGB565 ->
    /// BGRA -> channel -> pane), and there is no committed `.cart` to
    /// open instead.
    #[gpui::test]
    async fn smoke_cart_run_pause_step_stop(cx: &mut TestAppContext) {
        // The emulator thread is real and self-paced, so the journey has
        // to be allowed to wait on wall-clock time.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        std::fs::write(
            dir.path().join("game.cart"),
            ggo_emu_panel::fixture::green_screen_cart(),
        )
        .unwrap();

        let panel = open_cart(&workspace, cx, dir.path(), "game.cart");
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.test_is_ready(),
                "the cart is selected and its root resolved"
            );
            assert!(
                !panel.test_is_running(),
                "clicking a cart SELECTS it -- opening a tab must not start a run"
            );
        });

        cx.simulate_keystrokes("ctrl-alt-r");
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.test_is_running(),
                "ctrl-alt-r resolved to ggo_emu::Run and started the cart"
            );
        });

        pump_until(
            &panel,
            cx,
            "the first frame off the emulator thread",
            |panel| panel.test_frame() > 0 && panel.test_frame_pixel(0, 0).is_some(),
        );
        panel.read_with(cx, |panel, _| {
            let green = Some([0x00, 0xFF, 0x00, 0xFF]);
            assert_eq!(
                panel.test_frame_pixel(0, 0),
                green,
                "the cart's own backdrop -- RGB565 0x07E0 expanded to full-range \
                 BGRA -- reached the pane, so the whole port really ran"
            );
            assert_eq!(
                panel.test_frame_pixel(160, 120),
                green,
                "and the centre too"
            );
            assert_eq!(
                panel.test_frame_pixel(320, 0),
                None,
                "and the probe is bounded to the 320x240 framebuffer rather \
                 than indexing off the end of it"
            );
            assert!(!panel.test_is_paused(), "a run starts unpaused");
        });

        cx.simulate_keystrokes("ctrl-alt-p");
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.test_is_paused(),
                "ctrl-alt-p resolved to ggo_emu::TogglePause"
            );
            assert!(panel.test_is_running(), "a paused run is still a run");
        });
        let paused_at = settled_frame_count(&panel, cx);
        assert!(paused_at > 0, "the cart had already produced frames");
        let still_paused = settled_frame_count(&panel, cx);
        assert_eq!(
            still_paused, paused_at,
            "a paused emulator publishes nothing: the counter is FROZEN"
        );

        // Step advances and RE-PARKS. Deliberately not `paused_at + 1`:
        // `drive`'s frame channel is bounded at one frame and the
        // emulator thread uses `try_send`, so a pane that is behind
        // simply misses frames -- `test_frame` is the emulator's frame
        // NUMBER, with gaps, not a count of frames delivered. A frame
        // presented in the moment between the pause keystroke and the
        // thread reaching its park can therefore be dropped, leaving the
        // pane one number behind the emulator, and the step then lands
        // two numbers on. The exact one-frame contract IS asserted, at
        // the layer where it is observable: `drive.rs`'s
        // `pause_parks_step_advances_one_frame_and_resume_continues`
        // drains the channel itself. What this journey owns is the
        // wiring -- the keystroke reaches `StepFrame`, the run advances,
        // and it parks again instead of free-running.
        cx.simulate_keystrokes("ctrl-alt-.");
        pump_until(&panel, cx, "the stepped frame", |panel| {
            panel.test_frame() > paused_at
        });
        let after_step = settled_frame_count(&panel, cx);
        assert!(
            after_step > paused_at,
            "ctrl-alt-. resolved to ggo_emu::StepFrame and advanced the run"
        );
        assert_eq!(
            settled_frame_count(&panel, cx),
            after_step,
            "and it PARKED again rather than resuming -- one step is not a resume"
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_paused(), "and the step left it paused");
        });

        cx.simulate_keystrokes("ctrl-alt-s");
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel.test_is_running(),
                "ctrl-alt-s resolved to ggo_emu::Stop and ended the run"
            );
            assert!(!panel.test_is_paused());
        });
        // Let the end-of-run task join the emulator thread and land its
        // perf data in the temp database, so neither outlives the test.
        pump_until(&panel, cx, "the run's exit reason", |panel| {
            panel.test_status().as_deref() == Some("stopped")
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel.test_status_is_error(),
                "the user asking a healthy run to stop is not a failure"
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<ggo_emu_panel::EmulatorItem>(cx)
                    .count(),
                1,
                "and the tab is still there, ready to run again"
            );
        });
    }

    /// A cart that cannot load must fail LOUDLY and leave the pane
    /// usable: the run ends on its own, the status row carries the
    /// loader's reason, no frame is ever shown, and the tab survives to
    /// run something else.
    #[gpui::test]
    async fn smoke_bad_cart_surfaces_an_error_and_survives(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = boot_all(cx, dir.path()).await;
        std::fs::write(dir.path().join("bad.cart"), b"not a cart, just bytes").unwrap();

        let panel = open_cart(&workspace, cx, dir.path(), "bad.cart");
        panel.read_with(cx, |panel, _| {
            assert!(panel.test_is_ready(), "a junk cart still opens a tab");
            assert!(!panel.test_is_running());
        });

        // No assertion that the run is briefly live: the loader fails on
        // the first thing it does, so the frame channel can already be
        // closed -- and the pump task already finished -- by the time
        // `simulate_keystrokes`' own pump returns. The observable
        // contract is what the failure LEFT BEHIND.
        cx.simulate_keystrokes("ctrl-alt-r");
        pump_until(&panel, cx, "the failed run to end itself", |panel| {
            !panel.test_is_running() && panel.test_status().is_some()
        });
        panel.read_with(cx, |panel, _| {
            let status = panel
                .test_status()
                .expect("the failure reaches the status row");
            assert!(
                status.starts_with("cart: "),
                "the loader's own reason is what the pane shows: {status:?}"
            );
            assert!(
                panel.test_status_is_error(),
                "and a cart that FAILED to load reads as an error, not as an \
                 ordinary exit -- the styling is what distinguishes it"
            );
            assert_eq!(panel.test_frame(), 0, "no frame was ever produced");
            assert_eq!(
                panel.test_frame_pixel(0, 0),
                None,
                "and nothing was painted into the pane"
            );
            assert!(
                panel.test_is_ready(),
                "the cart is still selected -- the pane is usable, not wedged"
            );
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<ggo_emu_panel::EmulatorItem>(cx)
                    .count(),
                1,
                "and the tab survived the failure"
            );
        });
    }
}
