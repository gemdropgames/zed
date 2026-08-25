# Asset pipeline — plan

Spec: `docs/superpowers/specs/2026-08-25-asset-pipeline-design.md`.
Branches: zed `asset-pipeline` (off `ggo`), ggo `asset-pipeline` (off
`main`). Each phase: tests first where the unit is pure, clippy, commit.

## Phase A — worldlib (ggo repo)
What: the Aseprite reader, the import record on the tileset sidecar,
palette permutations on `Preview`, `decode_source`.
Why: rules and parsing stay editor-agnostic and unit-tested.
1. `sprites/aseprite.rs` + byte-fixture tests; `flate2` dep.
2. `tileset_meta::ImportRecord`, `source_changed`; round-trip test.
3. `import::Preview::{sort_by_luma, swap, move_slot}`; RGBA-invariant tests.
4. `import::decode_source`, `DecodedFrame`; dispatch test. Commit, merge to
   ggo `main` at the end with the zed merge.

## Phase B — drop + keys (zed)
What: workspace external-drop interceptor; import panel claims image
drops; wizard actions + keys.
Why: the two cheapest UX gaps; the interceptor seam is reused by nothing
else yet but mirrors `intercept_path_open`.
5. `workspace`: registry + `Workspace::intercept_external_drop`; `pane.rs`
   calls it. 6. `ggo_import_panel`: register interceptor; actions/keys;
   tests (claim/reject, enter/escape).

## Phase C — Aseprite in the wizard + remember/re-import
7. Loader → `decode_source`; `OpenImport.frames`; multi-frame sprite
   commit; menu/`is_png_path` extension; `.ase` test.
8. Commit writes `ImportRecord`; `Reimport` restores; tileset panel
   banner + Re-import → import panel; tests for record, restore, banner.

## Phase D — thumbnails
9. `workspace::ThumbnailProvider` global; `project_panel` observe + swap
   icon (GGO-marked seam). 10. `ggo_common::ThumbnailCache` (decode,
   downscale, LRU, drop_image); registered from `ggo_import_panel::init`
   (the crate that already depends on worldlib + image). Tests: fake
   provider in project_panel; cache decode of a `.til`.

## Phase E — palette surgery UI
11. Swatch buttons, ◀ ▶, Sort, Reset; sprite-mode read-only hint; test
    swap → committed `.pal` order.

## Phase F — wrap
12. Sweep tests + clippy (import, tileset, project_panel, workspace,
    ggo_common); MIGRATION.md rows; tick task 6; review agent; fix; merge
    `asset-pipeline` → `ggo`, push `ggo` + `main`; ggo repo → `main`, push.
