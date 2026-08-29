# World-hosted map editing

Date: 2026-08-29
Status: approved design, pre-plan

## Goal

Retire the standalone `.map` editor tab and move all map creation/editing into
the world editor, so tile painting happens with the surrounding entities and
other background layers visible. A world view shows the four hardware
background layers plus the entity layer; painting a layer edits the linked
`.map` file directly.

## Decisions (settled during brainstorming)

1. **`.map` stays the source of truth.** The world editor paints into the
   linked map's document store in place. There is no bake step: "bake"
   collapses to plain save. No changes to EMAP v3, to the world TOML
   `[[background]]` shape, or to engine/firmware.
2. **The standalone map editor tab is deleted** — `MapEditorItem`, the `.map`
   path-open interceptor, and the "New Map…" context-menu entry all go.
   Clicking a `.map` in the project panel falls through to default handling.
3. **Tilemap-component entity maps are edited the same way**: selecting a
   `Tilemap` entity enters the same in-world paint mode, targeting its map at
   the entity's world position.
4. **Background layer creation is automatic**: user picks a tileset for a
   slot; the editor writes `maps/<world-stem>.bg<N>.map` bound to it and links
   the slot. No naming dialog.
5. **The full tool set migrates**: brush, rect fill, flood fill, select,
   terrain autotile, eyedropper, eraser, plus the tileset strip picker,
   palSub/flip controls, and resize.
6. **Approach A**: `ggo_map_panel` is stripped into a paint library consumed
   by `ggo_world_panel` (see Architecture).

## Architecture

### Crate roles after the change

- `ggo_map_panel` (keeps its name) becomes a library crate with no workspace
  `Item`, no interceptor, no standalone canvas. It exports:
  - `PaintSession`: wraps a `MapDocStore`, the tileset display cache (strip
    image, palette, resolved columns), and the tool state machine
    (`MapTool`, stamp anchor/far, palSub, flips, selection).
  - The tileset strip picker widget and the terrain editor UI/logic.
  - The op funnel (`apply_op` / `step_history` equivalents) and the
    off-thread loader.
- `ggo_world_panel` hosts paint mode. It owns a `HashMap<stem, PaintSession>`
  and renders the tool rail / strip picker while in paint mode.
- `ggo-worldlib` gains background editing ops (below). Format codecs in
  `ggo-asset-formats` are untouched.

### World document changes (`worldlib/src/world_doc.rs`)

- New op: `WorldOp::SetBackground { layer: u8, map: Option<String> }`.
  `Some(path)` sets or replaces the slot; `None` clears it. The inverse is the
  slot's previous value, so undo/redo uses the existing `InverseOp`
  machinery. `store.backgrounds` stops being a read-only pass-through.
- Op-time validation mirrors the parse-time rules: layer in `0..4`; the
  duplicate-layer case cannot arise because a slot holds at most one entry.
- Removing a layer never deletes the `.map` from disk — unlink only. Other
  worlds or instances may reference the same map (`merge_backgrounds`), and
  unlink is undoable while deletion is not.

### Add-layer flow

1. User clicks "add" on an empty background slot in the layers rail.
2. Tileset picker over the project's `.til` assets; the palette pairs
   automatically via the existing `tileset_pal_path` rule.
3. The panel writes `maps/<world-stem>.bg<N>.map` to disk immediately —
   `NEW_MAP_DIM` (16×16), bound to the chosen tileset — mirroring the old
   "New Map…" immediate-write behavior. `<world-stem>` is the world's
   asset-root-relative stem with the leading `worlds/` stripped and path
   separators preserved, so nested worlds cannot collide.
4. `WorldOp::SetBackground` links the slot. Undoing the op unlinks but leaves
   the file on disk — an accepted, harmless orphan.

If the target `.map` path already exists on disk, the flow links it instead of
overwriting (covers redo-after-undo and manual re-adds).

### Paint mode

- World panel mode enum: `Entities` (default; today's behavior) |
  `Paint(PaintTarget)`, where `PaintTarget` = `BgSlot(u8)` |
  `TilemapEntity(EntityKey)`.
- Entry: click a linked slot in the layers rail; or "Paint tiles" on a
  `Tilemap` entity (context menu / double-click).
- Exit: Escape clears the marquee selection if one exists, otherwise exits to
  `Entities` — the old editor's escape semantics, one level up.
- While painting: entity manipulation is disabled; the tool rail, strip
  picker, palSub/flip controls, and resize control render from the paint
  library. The target layer draws at full opacity; all other content (other
  backgrounds, entities, instances) stays visible but dimmed.
- Hit-testing goes through the existing world camera transform to a
  map-local cell. Backgrounds anchor at world origin `(0,0)`; a
  `TilemapEntity` target anchors at `Transform.pos + (col,row) * TILE_PX`,
  matching `render.rs::push_tilemap_item`.
- Live preview: each mutation recomposes the target map
  (`compose_map_indices` + `indices_to_rgba` — the existing shared funnel,
  once per mutation, never per render) and swaps the composed image keyed by
  stem, so a map drawn in several places (bg slot and Tilemap entity) updates
  everywhere at once.

### Sessions, undo, dirty, save

- `PaintSession`s are created lazily on first paint-mode entry per stem,
  loaded off-thread, and live until the world closes — undo history and dirty
  state survive switching layers.
- Undo is mode-scoped: in paint mode, ctrl-Z/ctrl-shift-Z drive the target
  map's `MapDocStore`; in entity mode, the `WorldDocStore`. There is no
  unified cross-store timeline (known limitation).
- `WorldCanvasItem::is_dirty` = world store dirty ∨ any session dirty. Save
  writes the world TOML and `io::save_map`s every dirty session, each against
  `OpenWorld::root` captured at open time (existing repoint protection).
  Close-prompt and "Don't Save"/reload behavior route through the existing
  `Item` impl; discard drops sessions too.
- Emulate paths extend `save_if_open_and_dirty` to flush dirty paint
  sessions, preserving the `emd pack-ggo` reads-from-disk invariant.

### Failure states (displayed, never fatal)

- Slot references a missing/unreadable `.map`: error badge on the rail slot;
  entering paint mode shows a message instead of a session.
- Map with an empty/unresolvable `til_path`: paint mode opens with a
  bind-tileset prompt (the old panel's `tileset_error` philosophy).

## Deletion / migration inventory

Removed: `map_item.rs` (`MapEditorItem`), `intercept_map_open`,
`contribute_map_menu` and "New Map…", the standalone canvas/zoom/scroll
rendering in `ggo_map_panel.rs`, the panel registration in
`crates/zed/src/main.rs`.

Survives (relocated into the library surface): `MapTool` and the stamp
builder, `current_stamp`, strip picker, terrain editor, op funnel, loader,
and the still-relevant `geom.rs` helpers.

The fork-hooks rule still applies: the world panel's interceptor/menu paths
must keep pane-touching work inside `cx.defer_in`.

## Testing

- **worldlib**: `SetBackground` apply/inverse/undo/redo, layer-range
  validation, `to_doc` round-trip with edited backgrounds.
- **Paint library**: existing tool/stamp/terrain unit tests survive largely
  intact — the logic layer does not change.
- **Smoke** (`crates/ggo/smoke/src/ggo_smoke.rs`): the map edit journeys
  (currently :1546-1858) are rewritten world-hosted — fixture world with a
  bound background map; open world → enter paint mode → paint/undo/save
  round-trip with byte-exact reopen and preserved `til_path`; the
  select/delete/escape branches. One new journey covers the add-layer flow:
  tileset pick → `.map` created on disk + `[[background]]` written → paint →
  save.
- Every commit chains verification per repo rule:
  `./script/clippy -p <crate> && cargo test -p <crate> --lib && git commit`.

## Known limitations (accepted)

- No unified undo timeline across world and map stores; undo is scoped to the
  active mode.
- Undoing an add-layer leaves an orphan `.map` on disk.
- `.map` files are no longer directly openable from the project panel.

## Process notes

- Implementation plan follows via the writing-plans skill; the plan goes
  through plannotator for review.
- Model split: Fable designs/reviews; Opus implements.
