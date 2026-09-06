# Cart-Owned Editing — Phase 4 (Zed: save from the cart, component round trip) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In Live, the inspector edits components on the cart, structural edits are cart commands, the document mirrors every cart-side change, and Save writes what the cart holds (world snapshot + layer cells). Journeys prove each of these end to end; a real-artifact smoke proves the build/boot/drag path with the real `emd`.

**Architecture:** `LinkMailbox` v4 senders/mirrors (phase 3) plug into `live_step`: `ComponentChanged` → `WorldOp::SetField`s (or a new `SetComponent` op) on the document, `EntityAdded` → `AddEntity`, inspector commits → `set_component`, add/remove entity → `spawn`/`despawn`; Save in Live → `request_snapshot` + `read_layer` per loaded slot → rebuild direct entities from bags → existing TOML/map writers.

**Tech Stack:** Rust, GPUI, `emerald-editor-link` v4, `emerald-world` (`FieldReader::from_bag`, schema), `ggo_worldlib` (`world_to_toml`, map writers).

**Spec:** `docs/superpowers/specs/2026-09-06-cart-owned-editing-design.md` — "Editor: Mirror and undo", "Save", "Painting", "Testing".

## Global Constraints

- Branch `cart-owned-editing` (zed), on top of phase 2; emerald checkout on its phase-3 branch. Gates as in phase 2.
- Bag ↔ document: a bag decodes to `(component name via schema hash lookup, fields: name → JSON value)` using `emerald_editor_link` schemas (component names + field names + kinds) and `emerald_world::FieldReader::from_bag`; `Fixed` → JSON number (px), `Vec2` → `[x, y]`, `Str` → string, `Bool`, `Int`, `List` → array. The reverse (document component → bag) uses the same schema and `FieldWriter`-equivalent host-side encoder in `live.rs` (`bag_from_fields`).
- Instance members: components read-only in the inspector; `ComponentChanged` for a member updates only its transform mirror (`MoveInstance` rule from phase 2).
- Save in Live: "Saving…" on the status row; snapshot + layers must arrive within 5 s (cart clock) else "save failed: <reason>" and the file is untouched. Design mode saves the document as today.
- Comments why only; no `unwrap()` outside tests; no `let _ =`.

---

### Task 1: Bag ↔ document conversion (`live.rs`, pure)

- Produces: `pub fn fields_from_bag(schemas: &[SchemaEntry], bag: &[u8]) -> Option<(String, serde_json::Map<String, Value>)>`, `pub fn bag_from_fields(schemas, comp: &str, fields: &Map) -> Option<Vec<u8>>`.
- [ ] Tests: round trip for every builtin schema shape (Int, Fixed, Bool, Str, Vec2 list, AssetRef as Str); unknown component hash → `None`; a field missing from the bag keeps the document's value (merge semantics documented).
- [ ] Implement; gate; commit `ggo_world_panel: component bags to and from document fields`.

---

### Task 2: Inspector edits and structure go to the cart; the mirror follows

- `live_step`: `take_component_changes()` → for direct entities `WorldOp::SetField` per changed field (mirror op, no resend); `take_added()` → `AddEntity` with the cart's index and the bags fetched from the cart (`Entity` blob for that index, requested via `request_entity(index)` — add to phase 3's link if missing, else `Snapshot`-and-pick); inspector field commit in Live → `set_component(index, bag_from_fields(..))` in addition to the document op (no world resend); `+ Entity`/`Delete` in Live → `spawn(bags)`/`despawn(index)` (no resend); undo/redo of `SetField`/`AddComponent`/`RemoveComponent`/`AddEntity`/`RemoveEntity` in Live → the matching cart command; everything else (instances, backgrounds) keeps the world resend.
- [ ] Tests (hand-rolled datagrams): `ComponentChanged` with a Transform bag updates the document; inspector commit sends `SetComponent` with the encoded bag and no `BlobBegin`; add entity sends an `Entity` blob and the document gains the entity at the cart's reported index; undo of a field edit sends the old bag.
- [ ] Implement; gate; commit `ggo_world_panel: component edits and structure are cart commands`.

---

### Task 3: Save from the cart

- `save_impl` in Live: `request_snapshot()` + `read_layer(slot)` for each loaded slot; a `Saving` state on `LiveView` collects blobs (`take_blob`); when all arrived: rebuild the document's direct entities from the snapshot (instance members skipped; a snapshot index the document does not know → appended as a new entity), write layer cells into the paint session/map files, then the existing `save_impl` write path; clear dirty. Timeout/failed transfer → status error, no write. `WorldCanvasItem::save` awaits the same path (returns when the write lands or fails).
- [ ] Tests: snapshot with a moved entity → the saved TOML has the moved position; a snapshot with an extra entity → appended; layer cells → the `.map` file has the painted cell; a failed cart blob → file unchanged and status shows the reason; Design save unchanged.
- [ ] Implement; gate; commit `ggo_world_panel: save writes the cart's world`.

---

### Task 4: Journeys (extend `cart_journeys.rs`)

- [ ] 12. `inspector_edit_round_trips_through_the_cart`: edit Transform pos in the inspector → cart row moves → overlay follows → document matches.
  13. `paint_a_cell_save_reload_the_cell_is_there`: paint in Live (SetCell), save, reload from disk, the `.map` has it.
  14. `a_user_system_edit_survives_save`: `edit_systems` entry that moves entity 0 by +5 and `mark_dirty`s it on a command → save → the TOML has +5.
  15. `add_and_delete_entities_in_live`: `+ Entity` → cart row appears; `DeleteSelected` → gone; undo restores on the cart.
  16. `snapshot_failure_keeps_the_file`: harness drops all cart→host chunks → save reports failure, file unchanged.
- [ ] Gate; commit `ggo_world_panel: persistence journeys`.

---

### Task 5: Real-artifact smoke (`ggo_smoke`)

- A test gated at runtime on `which emd` and a riscv toolchain (`rustup target list --installed | grep riscv32imc`); otherwise prints a skip line and passes. Builds a minimal fixture emerald project (`emd new`), runs `emd editor-cart --ggo --json`, boots the `.ggo` through `ViewerRun` (real emulator thread), greets over the `LinkEndpoint`, loads a world, sends `Pointer` press/move/release datagrams, asserts rows moved; asserts the registry never rebuilt (runner call count stays 1) across a 2 s window after the build outputs landed.
- [ ] Implement; gate; commit `ggo_smoke: real emd editor-cart boot and drag`.

---

### Task 6: Gate, review, merge both repos, reinstall, report

- [ ] Whole-fork gate; opus review of the whole `cart-owned-editing` branch (zed) + emerald phase 3 diff; fix wave; merge emerald → main, zed → ggo + main; push; reinstall `emd`, `zedgg`, `zedgg-emu-mcp`; final report with the journey list, rulings, and known gaps.
