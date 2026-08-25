# Map editor tools — design

Status: approved 2026-08-25. Task 5 of `tasks/editor-gaps.md`.

## Goal

Flood fill, cell multi-select with copy/paste/delete, brush strokes as
one undo entry, the per-tileset `cols` the map panel lost, and
8-neighbour ("blob", 47-tile) autotile terrains authored by labelling
tiles with their neighbour mask.

## Facts this rests on

- `.map` (`ggo-asset-formats/src/map.rs`): `w`, `h`, bound tileset, then
  `w*h` cells of one `u16` -- `[9:0]` tile, `[13:10]` palette sub, `[14]`
  hflip, `[15]` vflip. `CELL_BLANK = 0x03FF` is the editor's "never
  painted". One `.map` is one layer; worlds layer maps via `[[background]]`.
- `MapDocStore` (`worldlib/sprites/map_doc.rs`): `MapOp::{Brush, RectFill,
  Erase, Resize, BindTileset}`, op-level `InverseOp::Cells` undo, no
  gesture coalescing -- a brush drag is one undo entry per mouse move.
  `TilesetDocStore::{begin_stroke, end_stroke, apply_stroke_paint}` is the
  in-fork precedent for folding a drag.
- The tileset panel persists `zoom`/`cols`/`lines` in
  `<worktree>/.ggo-ide/<worktree-rel>.editor.json`
  (`tileset_panel/src/loader.rs::ViewMeta`). The map panel never reads it
  (asset-root frame vs worktree frame) and falls back to 8 columns, which
  scatters metatiles laid out at another width.
- No autotile/terrain code exists anywhere in the fork or its libraries.

## Decisions

1. **Ops (worldlib)**
   - `MapOp::Fill { x, y, cell }`: 4-connected flood over cells equal to
     the start cell's full `u16` (a blank region fills too); no-op when the
     start already equals `cell`; one `InverseOp::Cells`.
   - `MapOp::SetCells(Vec<(i32, i32, u16)>)`: sparse writes, out-of-range
     entries dropped, one `InverseOp::Cells`.
   - Strokes: `begin_stroke()` / `end_stroke()`; while a stroke is open,
     `Brush`, `Erase` and `SetCells` append their changes to the stroke's
     single `InverseOp::Cells` (an op that changes nothing appends
     nothing). Any other op, undo, redo or `mark_saved` ends the stroke.
2. **Terrains (worldlib `sprites/terrain.rs`)**
   - `Terrain { name: String, tiles: Vec<TerrainTile { tile: u16, mask: u8 }> }`.
     Mask bits: N=1, NE=2, E=4, SE=8, S=16, SW=32, W=64, NW=128.
   - `canonical(mask) -> u8` clears a diagonal bit unless both adjacent
     edge bits are set; the canonical set has exactly 47 members.
   - Membership: a cell belongs to a terrain when its tile index is one of
     the terrain's tiles (palette sub and flips ignored).
   - `resolve(cells, w, h, targets, terrain, paint) -> Vec<(x, y, cell)>`:
     for each target and each of its 8 neighbours that belongs to the
     terrain (targets themselves count as members when `paint`, as
     non-members when erasing), compute the canonical mask from membership
     and write the matching tile; a mask with no tile falls back to the
     tile whose mask has the most bits in common (ties: lower mask), and
     to nothing when the terrain has no tiles. Erasing writes `CELL_BLANK`
     to the targets. Written cells keep palette sub 0 and no flips.
3. **Shared sidecar (`ggo_common::tileset_meta`)**: `TilesetMeta { zoom,
   cols, lines, terrains: Vec<Terrain> }`, path
   `<worktree>/.ggo-ide/<worktree-rel>.editor.json`, load = defaults on
   missing/corrupt, save = best effort. The tileset panel's `ViewMeta`
   becomes this type. The map panel resolves the tileset's worktree-rel
   from its asset root + `til_path` and reads `cols` (through worldlib's
   `resolve_cols`) and `terrains`.
4. **Map panel tools**: `Brush | RectFill | Fill | Select | Terrain |
   Eyedropper | Eraser`.
   - Brush/Eraser drags run inside a stroke (mouse down → `begin_stroke`,
     up/cancel → `end_stroke`).
   - Fill: click floods with the stamp's first cell.
   - Select: drag a cell marquee (anchor/head, inclusive, clamped),
     outline drawn on the canvas. `ctrl/cmd-c` copies the region into the
     panel's in-memory clipboard (a `Stamp`, survives switching maps);
     `ctrl/cmd-v` pastes it as one `Brush` at the hover cell when the
     cursor is over the canvas, else at the selection's top-left, else
     `(0, 0)`, and selects the pasted region; `delete`/`backspace` blanks
     the region as one `RectFill`. `escape` clears the selection.
   - Terrain: dropdown of the tileset's terrains; click/drag paints
     (`SetCells` from `resolve`, inside a stroke); shift-drag erases.
     Inert with no terrain selected.
5. **Terrain editor**: a collapsible section under the tool row: terrain
   list (select / `+ New` with a name editor / rename / remove), a 3×3
   toggle grid for the current mask, `Assign to tile` which sets the
   current mask on the strip's anchor tile (adding it to the terrain, or
   re-labelling it), rows `tile N  <mask glyphs>  ×`. A non-canonical mask
   is refused with a message. Every change writes the sidecar.

## Keys (`GgoMapPanel`)

`ctrl/cmd-c` Copy, `ctrl/cmd-v` Paste, `delete`/`backspace` DeleteSelection,
`escape` ClearSelection. Tools stay toolbar buttons (letters are free but
the panel's convention is buttons).

## Tests

- worldlib: fill region/blank/no-op/clip; `SetCells` clipping; stroke folds
  brush+erase+setcells into one entry and any other op ends it; terrain:
  canonical set is 47; resolve on a 3×3 island yields the expected masks;
  fallback picks the closest mask; erase re-resolves neighbours.
- ggo_common: sidecar round trip incl. terrains; corrupt = defaults.
- map panel: cols read from the sidecar (worktree frame) and used by the
  strip; fill through `paint_at`; select/copy/paste/delete; brush drag =
  one undo; terrain paint writes the neighbourhood and is one undo; the
  editor's assign/remove persists.

## Out of scope

Layers inside `.map`, per-cell collision, corner-terrain search, terrains
shared across tilesets, autotile on rect-fill/flood-fill.
