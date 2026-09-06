# Cart-Owned Editing — Phase 2 (Zed: Live as a terminal) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In Live, the Zed world tab forwards pointer and keymap commands to the cart, paints the cart's selection, mirrors the cart's moves into the document (one undo entry per cart gesture), and offers Edit/Play plus a tool radio. Every host-side Live hit-test and drag is deleted. Journey tests drive the real `emerald-editor-runtime` in-process.

**Architecture:** `LiveView` gains cart-state mirrors (selection, marquee, gesture, removed) fed by `LinkMailbox` (emerald phase 1); `live_step` turns rows into gesture-coalesced `MoveEntity`/`MoveInstance` ops and removals into `RemoveEntity`; mouse handlers in Live only build a `Pointer`; keymap actions in Live only build a `Command`; the rail replaces the systems checkboxes with `Edit | Play` and a tool radio. A `CartHarness` test fixture runs the cart's real `World` + `Schedule` + `LinkState` against the panel's `LinkEndpoint`.

**Tech Stack:** Rust, GPUI, `emerald-editor-link` v3 (`pointer`, `command`, `set_mode`, `set_tool`, `groups`, `selection()`, `marquee()`, `take_gestures()`, `take_removed()`, `tool_names()`), `emerald-editor-runtime` (`install`, `LinkState`, `Mailbox`, `process`, `CartLink`) as a dev-dependency.

**Spec:** `docs/superpowers/specs/2026-09-06-cart-owned-editing-design.md` — "Editor", "Testing".

## Global Constraints

- Branch `cart-owned-editing` in `/home/clay/projects/zed` (exists, off `ggo`). Emerald checkout `/home/clay/projects/emerald` is on its own `cart-owned-editing` branch (phase 1, protocol v3) — Zed builds against it by path. Commit per task, no AI trailers.
- Gate per task: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`; final: the same for `ggo_common ggo_emu_panel ggo_emerald_panel ggo_emu_mcp ggo_smoke`. Known pre-existing failures (foreign ggo-hal edits): `ggo_emu_panel` `drive::tests::start_drives_frames_and_wait_ends_the_thread`, `tests::test_a_real_cart_drives_the_panel_end_to_end`; `ggo_smoke` `tests::smoke_cart_run_pause_step_stop`.
- Pointer device px: `device = (canvas_px − frame_origin) / scale`, from `live::geometry` (Phase 4 v2); clamp to `i16`. Buttons bit0 left, bit1 middle, bit2 right; modifiers bit0 shift, bit1 ctrl/cmd, bit2 alt; `snap` = the panel's snap toggle. At most one `Pointer` per tick (`live_step`), the last event wins; a release (buttons 0) is never dropped: if a release and a press land in one tick, send the release this tick and the press next.
- In Live no host-side hit-test/marquee/drag/nudge/delete/pan runs; Design mode is untouched.
- Mirror ops: a row whose transform differs from the document → `MoveEntity { entity, pos, gesture: Some(format!("cart-{id}")) }` (or `MoveInstance` for a group's primary member, delta = member's row − document member pos) using the cart's current gesture id (from `Gesture Begin`); outside a gesture (nudge from a user system) each row change is its own op. `Gesture End` closes coalescing. `EntityRemoved { index }` → `RemoveEntity` op (instance members: ignore, log). Selection from the cart replaces `open.selected` (index map → `Selection`).
- Undo/redo in Live: `MoveEntity`/`MoveInstance`/`MoveMany` inverse → `SetTransform` per affected cart index (no world resend); any other op → world resend as today.
- Fork hook rule (CLAUDE.md); comments why only; no `unwrap()` outside tests; no `let _ =` on fallible ops.

---

### Task 1: Mirrors in `LiveView`, cart state into the document

**Files:**
- Modify: `crates/ggo/world_panel/src/live.rs` (`LiveView`: delete `drag_origin`, `pending_transforms`, `pan_drag`, `sent_camera`/`input_camera`; add `cart_selection: Vec<u32>`, `marquee: Option<[f64;4]>`, `gesture: Option<u32>` (open gesture id), `pending_pointer: Option<PointerState>`, `pending_commands: Vec<EditCommand>`, `mode: EditorMode`, `tool: u8`, `tool_names: Vec<String>`); delete `hit`, `hit_row`, `rows_in_rect`, `hits_in_rect`, `drag_origins`, `contains`; keep `overlay_rows`, `CartRow`, `IndexMap`, geometry.
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` `live_step` (~line 1238): after `poll`, (1) selection: `mailbox.selection()` → `Vec<Selection>` via `index_map.selection_of` dedup → `open.selected` (only when changed; notify); (2) marquee → `live.marquee` (world px); (3) gestures: `take_gestures()` Begin → `live.gesture = Some(id)`, End → `None`; (4) rows: for each row whose `(x, y)` differs from the document position of its selection (entity: the entity's `Transform.pos`; instance member: the member's current pos = instance pos + member offset — use the primary member only, i.e. the first index of the group), apply `MoveEntity`/`MoveInstance` with `gesture: live.gesture.map(|id| format!("cart-{id}"))` through `apply_op` WITHOUT triggering `world_dirty` (add a `apply_mirror_op` that skips the Live resend); (5) `take_removed()` → `RemoveEntity` for direct entities (mirror op, no resend); (6) after a world load lands (`HelloAck`/`Loaded`), send `groups(&instance_ranges)` computed from `IndexMap` (contiguous runs per `Selection::Instance`).
- Test: `ggo_world_panel.rs` tests using the existing hand-rolled datagram helpers plus new builders `cart_selection(&endpoint, &[idx])`, `cart_gesture(&endpoint, kind, id)`, `cart_marquee(..)`, `cart_removed(..)`; `hello_ack(version, tool_names)` semantics change (names are tools).

**Interfaces:**
- Produces: `WorldPanel::apply_mirror_op(&mut self, op: WorldOp, cx)`; `live::instance_ranges(&IndexMap) -> Vec<(u32, u32)>`; `LiveView::{cart_selection, marquee, gesture}`.

- [ ] **Step 1: Tests.** (a) `cart_selection([1])` → `open.selected == [Selection::Entity(1)]` and the overlay row 1 is marked selected; (b) `cart_gesture(Begin, 7)`, `cart_rows([(0, 40, 50)])`, `cart_rows([(0, 60, 50)])`, `cart_gesture(End, 7)` → the document entity 0 is at (60, 50) and the undo stack has ONE entry whose undo returns it to the original; (c) rows moving an instance member (index inside a group) → one `MoveInstance` and the instance pos moved by the member's delta; (d) `cart_removed(2)` → entity 2 gone from the document, undo restores it; (e) after `hello_ack` + world sent, `host_sent` contains a `Groups` datagram (kind 0x0F) with the fixture's instance range; (f) a row change never triggers a world resend (no `BlobBegin` after the mirror op).
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Gate.** **Step 5: Commit** `ggo_world_panel: mirror the cart's selection, gestures and moves into the document`.

---

### Task 2: Live input is forwarded, not interpreted

**Files:**
- Modify: `ggo_world_panel.rs` `render_canvas` mouse handlers (~5985-6100), `canvas_primary_down_with`/`canvas_double_click`/`canvas_drag_to`/`canvas_primary_up`/`handle_pan_move` (Live branches removed; in Live the handlers call `live_pointer(local, buttons, modifiers)`), `live_pan_begin`/`live_pan_by` deleted, `wheel_zoom` Live branch stays (scale); keymap listeners (`NudgeLeft…`, `DeleteSelected`, `SelectAll`, `ClearSelection`, `Duplicate`) in Live → `live_command(EditCommand)`; `live_step` flushes `pending_pointer` (one per tick, release-priority rule) and `pending_commands` (all).
- Modify: `live.rs` `PointerState { device: (i16, i16), buttons: u8, modifiers: u8, snap: bool }` and `pub fn device_from_canvas(local: [f64;2], frame_rect: [f64;4], scale: u32) -> (i16, i16)`.

- [ ] **Step 1: Tests.** Unit: `device_from_canvas([100, 80], [20, 20, 640, 480], 2) == (40, 30)`; clamps beyond i16. Panel: mouse down at a canvas point in Live → next tick `host_sent` has a `Pointer` (0x0B) with the expected device px and buttons 1; move → `Pointer` buttons 1 new position; up → buttons 0; a down+up in one tick → the release is sent this tick and no press is lost (press next tick); `NudgeLeft` action in Live → `Command { NUDGE, -1, 0 }` (tile variant −16); `DeleteSelected` → `Command DELETE`; the document is NOT changed by any of these host-side (no op recorded); Design mode: the same clicks still hit-test locally as before (existing Design tests pass unchanged).
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Gate.** **Step 5: Commit** `ggo_world_panel: live gestures forward pointer and commands to the cart`.

---

### Task 3: Rail — Edit | Play and the tool radio; overlay paints the cart's marquee

**Files:**
- Modify: `ggo_world_panel.rs` `render_systems_rail` → `render_edit_rail` (Edit|Play toggle → `set_mode`, tool radio over `tool_names()` → `set_tool`; `live_sys_mask`/`set_live_system` deleted; sticky `live_sys_mask` on `WorldDock` replaced by sticky `live_tool: u8`), `render_canvas` Live branch passes `live.marquee` as the scene marquee, inspector disabled (read-only) in Play.
- Modify: `world_dock.rs` sticky field rename.

- [ ] **Step 1: Tests.** `debug_bounds` for `ggo-world-mode-edit-on` / `ggo-world-mode-play-off` and `ggo-world-tool-0-on`; clicking Play sends `SetMode 1`, the inspector shows disabled; clicking tool 1 sends `SetTool 1`; a `Marquee` datagram paints the marquee rect (assert via the scene's marquee through `test_live_marquee()`); `hello_ack(v, ["Select","paint"])` renders two radio entries.
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Gate.** **Step 5: Commit** `ggo_world_panel: Edit/Play switch and tool radio replace the systems rail`.

---

### Task 4: Undo/redo of moves replays `SetTransform`

**Files:**
- Modify: `ggo_world_panel.rs` `undo_impl`/`redo_impl` + `apply_op` Live path: when the (inverse) op is `MoveEntity`/`MoveInstance`/`MoveMany`, compute the affected cart indices via `IndexMap` and send `set_transform(index, raw x, raw y)` for each instead of marking `world_dirty`; every other op keeps the world resend.

- [ ] **Step 1: Tests.** After the mirrored drag from Task 1 (b), `undo` sends `SetTransform` (0x02) for index 0 with the original position and NO `BlobBegin`; `redo` sends the moved position; undo of a `RemoveEntity` still resends the world (`BlobBegin` appears).
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Gate.** **Step 5: Commit** `ggo_world_panel: undo replays transforms to the cart`.

---

### Task 5: `CartHarness` and the journey tests

**Files:**
- Create: `crates/ggo/world_panel/src/cart_harness.rs` (`#[cfg(test)]`): `CartHarness { world: World, schedule: Schedule, mailbox: Box<Mailbox>, link: EndpointCartLink }`; `EndpointCartLink(Arc<LinkEndpoint>)` implements `emerald_editor_runtime::link::CartLink`: `recv` pops one APP payload from `endpoint.take_outbound()` (decode the wire frames with `ggo_comm::MessageReader`, keep a `VecDeque` of pending payloads); `send` → `endpoint.push_inbound(payload)` + `tick()`. `CartHarness::new(endpoint, edit_systems: SystemTable, game_systems: SystemTable)` calls `install`; `frame()` runs `schedule.run(&mut world)` once and publishes a fake frame image into the endpoint (so the panel leaves the boot screen); `frames(n)`.
- Create: `crates/ggo/world_panel/src/cart_journeys.rs` (`#[cfg(test)]`): the journeys below, each `#[gpui::test]`, using `live_panel(cx, &dir)` + the harness bound to that endpoint, and the panel's REAL handlers (`canvas_primary_down_with`, `canvas_drag_to`, `canvas_primary_up`, the middle-button handlers, the keymap actions) plus `debug_bounds` on overlay selectors (add `debug_selector` per overlay row: `ggo-world-live-row-{index}-{selected|unselected}` — the paint closure cannot carry selectors; expose `test_live_overlay() -> Vec<(u32, [f64;4], bool)>` instead and assert on it).
- Modify: `ggo_world_panel.rs` tests module `mod cart_journeys;`, helpers as needed (`test_live_overlay`, `test_inspector_position_text()`).

- [ ] **Journeys (all must pass):**
  1. `click_selects_and_outlines_the_sprite`: fixture world with a `Transform` entity at (40, 50); click at its device position; after `frames(2)` the panel's selection is that entity and `test_live_overlay()` marks its rect selected.
  2. `drag_moves_the_entity_and_the_outline_follows`: press on it, move +30 px (device, scale 1), `frames(1)` per move step (3 steps); the overlay rect x advances each frame; inspector position text shows the new x; release → `frames(2)`; the document entity is at (70, 50); the undo stack has exactly one new entry.
  3. `undo_after_a_drag_moves_it_back_on_the_cart`: continue 2; `undo`; `frames(2)`; the cart's row (and overlay) is back at (40, 50).
  4. `shift_click_toggles_and_marquee_selects_two`.
  5. `middle_drag_pans_and_the_outline_stays_on_the_sprite`: overlay rect moves by the pan delta in canvas space exactly as the reported camera does.
  6. `nudge_delete_select_all_through_the_keymap`: `NudgeRight` moves by 1; `NudgeRightTile` by 16; `SelectAll` selects both; `DeleteSelected` removes them from the cart (`EntityCount` 0) and the document.
  7. `an_instance_drags_as_a_group_and_undoes_as_one`: fixture with an instance of two entities; drag one member → both rows move, the document `MoveInstance`d once; undo restores both.
  8. `play_mode_ignores_clicks_and_runs_the_game`: harness with a `game_systems` entry that moves entity 0 by +1 per frame; `SetMode Play` → after 5 frames the row moved 5, a click changed nothing; `Edit` → the game system stops, selection cleared.
  9. `a_user_edit_system_sees_the_click_and_can_consume_it`: `edit_systems` entry that on left just-pressed sets a static flag and `consumed = true`; click → flag set, selection stays empty.
  10. `switching_the_tool_makes_the_builtins_inert`: `SetTool 1` then click → no selection; `SetTool 0` → click selects.
  11. `closing_the_tab_stops_the_cart`: endpoint `stop_requested()` after the tab closes (dock harness).
- [ ] **Run** all; **Gate**; **Commit** `ggo_world_panel: cart journeys drive the real editor runtime`.

---

### Task 6: Whole-fork gate, review, merge both repos, reinstall

- [ ] **Step 1:** `for c in ggo_common ggo_world_panel ggo_emu_panel ggo_emerald_panel ggo_emu_mcp ggo_smoke; do ./script/clippy -p $c && cargo test -p $c --lib || exit 1; done` (known failures excepted).
- [ ] **Step 2:** Fresh opus review over `git diff ggo...cart-owned-editing` (zed) for practices + spec "Editor"/"Testing"; fix; re-gate.
- [ ] **Step 3:** Merge emerald `cart-owned-editing` → `main` (ff), push; merge zed → `ggo`, push `ggo` and `main`. Reinstall `emd` (`cargo install --path crates/cli --force` in emerald), `zedgg` (`script/install-zedgg`), `zedgg-emu-mcp`.
- [ ] **Step 4:** Report to the user with the journey list and what to try by hand.
