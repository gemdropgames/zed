# GGO test-coverage review — 2026-08-21

Scope: all 10 `crates/ggo/*` crates (~54k lines, 733 tests) plus GGO
touchpoints in `workspace`, `project_panel`, and `gpui`. Five parallel
audits mapped every action, mouse handler, form, and pub fn to its
covering test. This file is the synthesis; per-crate raw tables live in
the audit transcripts.

## Verdict

Coverage is strong where it was built test-first: pure math modules are
at or near 100% (`sprite_panel::{tiles,edits,onion,playback}`,
`map_panel::geom`, `world_panel::inspector`, `emu_panel` unit modules,
`import_panel::geom`, `charts_panel::chart_geom` largely). The gaps are
systemic, not random, and cluster into six themes.

## Theme 1 — data-loss paths untested (worst cluster, all P0)

- `ggo_common::prepare_to_close_dirty`: a **Save that fails must cancel
  the close** — no test anywhere in the repo. A save callback returning
  `true` on a failed write silently discards the document.
  Same untested branch surfaced independently in world_panel and
  map_panel `prepare_to_close`.
- `Workspace::prepare_to_close` dock-panel poll loop (workspace.rs
  ~3497): never driven end-to-end with a dirty dock panel; only panel
  methods are called directly.
- `SpriteEditorItem::save` and `WorldCanvasItem::save`: **zero tests**
  on the workspace save/close route for the new center tabs. The
  WorldCanvasItem dead-panel branch returns `Ok(())` while dropping
  edits — silently.
- `save_impl` write-failure (`save_error` set, dirty retained): untested
  in sprite, world, AND map panels.
- No test anywhere answers a close prompt with **"Save"** in
  sprite_panel (`save_for_close` has zero refs).
- `sprite_panel::confirm_name_frame` — the UI path that writes the
  editor sidecar — untested (only programmatic `set_frame_name` is).

## Theme 2 — wiring tested by method call, never by gesture

Everywhere: buttons, actions, and keybindings are exercised by calling
the handler method directly; the click/keystroke/action-dispatch wiring
is not.

- Actions never dispatched: sprite `PlayPause`/`CommitField`; world
  `CommitField` (Enter — the primary commit gesture, P0),
  Undo/Redo/Save/Delete/Nudge across world+map; emu Run/Stop/ToggleMute;
  emerald `Submit`; charts none beyond ToggleFocus.
- No `simulate_keystrokes` for any binding except world tab/shift-tab.
- Clicks: charts run-list rows, Back/Close/Re-run buttons; emu Run/Stop/
  Mute; emerald all 14 `on_click` listeners; tileset zoom buttons.
- `dispatch_context` `editing`/`not_editing` stamps (guards space/enter
  from stealing editor input) untested in sprite panel.

## Theme 3 — rendered canvas interaction

Only sprite_panel has a real rendered-window mouse test
(`test_rendered_clicks_select_a_tile_and_repoint_the_cell`) — the
ready-made template. Missing equivalents:

- import_panel: the ENTIRE canvas layer (crop down/move/up, middle-drag
  pan, wheel zoom, window→image coord mapping under zoom/pan).
- world_panel: all six canvas listeners incl. the focus-take on click,
  `handle_pan_move`, `edit_drag_local`, wheel zoom wiring.
- map_panel: strip tile-pick listeners (`strip_cell_at` zero refs),
  middle pan, wheel zoom, mouse-up end-paint.
- sprite_panel: strip drag-reorder `on_drag`/`on_drop` (only
  `move_frame_to` is tested) and double-click → name form.

## Theme 4 — error and race paths

- charts loader: `list_runs` error (the ONLY producer of
  `LoadState::Error`), `{e:#}` stringification, `started_at` tie-break;
  `detail::load` error propagation. All P0.
- Stale-generation guards: tileset (P0), world, map, charts refresh —
  never raced.
- tileset `refresh_root` REAL worktree-discovery branch: never executed
  (every test uses `root_override`). P0.
- Interceptor declines (outside worktree, no panel docked, remote) —
  patchy; world interceptor's dead-panel `WeakEntity::new_invalid` path
  and `open_path` failure branch untested.
- emu `Session::drop` (panel closed mid-run): thread teardown never
  asserted. P0 (orphaned emulator process).
- emerald `NewProject` end-to-end (dialog → `emd new` → workspace swap
  → failure toast): only the arg-builder is tested. P0.

## Theme 5 — math edge cases (charts)

- `chart_geom::bins`: wide integer ranges (`nice_step`/`ceil` branch,
  last-bin clamp) and negative/mixed-sign values — untested. P0.
- `chart_set::build_charts` exact chart ORDER for a fully-gated run;
  `colored`'s fold-slot invariant. P0.
- `report::kpi_tiles` working-set `> 64` threshold boundary. P0.

## Theme 6 — inspector write ops (world)

- Bool checkbox → `SetField(Bool)`: the only bool edit path. P0.
- `Add component…` → `AddComponent` + `defaults_for`: menu path
  untested. P0.
- Remove-component trash → `RemoveComponent`: zero refs. P0.
- world `open_rel_path` dirty + Save / dirty + Don't-Save branches
  (only Cancel tested). P0.

## Enablers

- `ggo_common/Cargo.toml` needs dev-deps (`gpui`, `project`,
  `workspace` with test-support, `serde_json`) before its gpui-level
  gaps are testable. No cycle risk.
- No workspace-crate-local tests exist for the interceptor/contributor
  registries — all coverage is cross-crate; a workspace refactor sees
  green until ggo crates build.
- gpui `nearest()` sampling opt-in: zero coverage builder→shader
  (regression to bilinear blur would ship silently). P2 but cheap to
  pin at the scene-emission level.

## Counts

- P0: ~28 distinct gaps (clusters above).
- P1: ~60 (wiring/gesture layer dominates).
- P2: large tail (render-only, Panel trait accessors, paint helpers) —
  mostly not worth writing.

## Status update (same day, post-fill)

ALL THREE WAVES ARE DONE. GGO totals: 850 tests (was 733 at audit
time), every crate clippy-clean, zed builds.

- Waves 1+2 (+37): every P0 in Themes 1, 4, 5, 6 plus emu Session-drop
  and emerald NewProject. Production fixes: `WorldCanvasItem::save` on
  a dead panel now returns Err instead of a silent Ok; emerald's
  NewProject flow gained runner/open seams.
- Wave 3 (+57): keystroke dispatch matrices (incl. the `> Editor`
  precedence pins in sprite/world/map and sprite's editing-blocks-space
  stamp), rendered strip drag-reorder (gpui drag IS drivable
  headlessly: down, move past 2px, second move onto the repainted
  target, up), import's full canvas gesture stack, world/map canvas
  pan/wheel/strip listeners (closures extracted to named methods),
  charts row/Back/Close/Re-run click wiring + error-state renders +
  stale-refresh races (iterations=10), emu transport keys/buttons/pad
  listeners + ingest-failure surfacing (new `ingest_finished_run`
  seam), emerald form cancel/kind/field flows incl. rendered clicks.

Remaining: the P2 tail only (render-only accessors, paint helpers,
gpui `nearest()` shader pin) — logged above, deliberately unwritten.

## Recommended fill order

1. **Wave 1 (P0 data loss, ~12 tests):** failed-save-cancels-close in
   ggo_common (+ world/map twins); `Workspace::prepare_to_close`
   end-to-end; both Items' `save` branches incl. dead-panel;
   `save_impl` failure ×3; sprite "Save" prompt answer;
   `confirm_name_frame`.
2. **Wave 2 (P0 correctness, ~12 tests):** charts bins/order/threshold/
   loader errors; tileset refresh_root + stale-generation; world
   inspector AddComponent/RemoveComponent/bool; world open_rel_path
   Save/Don't-Save; emu Session::drop; emerald NewProject.
3. **Wave 3 (P1 gesture layer):** action-dispatch + keystroke tests per
   panel; rendered-canvas tests for import/world/map on the sprite
   template; click wiring for charts/emu/emerald buttons.
