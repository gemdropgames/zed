# Pixel tools — design

Task 7 of `tasks/editor-gaps.md`. The tileset editor has Pencil / Eraser /
Picker / Select and a 16-slot palette; this adds the tools a pixel editor
is expected to have, without changing how edits reach the document.

Decisions with the user (2026-08-25): fill floods the composed sheet across
tile borders; the magnified canvas is a **focus mode** on the existing
sheet view; selection transforms are move + flip H/V; delete row/column
are edge affordances mirroring insert. Defaults: brush 1–4 px; shapes are
outlines, shift-drag fills them; no rotate.

## 1. Library half (`ggo-worldlib`)

### 1.1 `sprites/pixel_tools.rs` — sheet-space geometry

Pure functions over `(x, y)` sheet pixels (the composed grid's
coordinates, `cols × TILE_PX` wide). No document access; the editor maps
each point to `(tile, x, y)` afterwards and drops points over pad cells.

```rust
pub fn line(a: (i32, i32), b: (i32, i32)) -> Vec<(i32, i32)>          // Bresenham
pub fn rect(a, b, filled: bool) -> Vec<(i32, i32)>                    // inclusive corners
pub fn ellipse(a, b, filled: bool) -> Vec<(i32, i32)>                 // midpoint, inside the bbox
pub fn brush_expand(points: &[(i32,i32)], size: usize) -> Vec<(i32,i32)> // size×size square, anchored top-left at the point, deduped
pub fn mirror_in_tile(points: &[(i32,i32)], tile_px: usize, h: bool, v: bool) -> Vec<(i32,i32)> // adds the reflections about each point's own tile's centre lines, deduped
pub fn flood(sample: impl Fn(i32, i32) -> Option<u8>, start: (i32, i32)) -> Vec<(i32, i32)>
    // 4-connected region of `sample(start)`; `None` (off-sheet / pad) is a wall
```

Tests: line symmetry and endpoints, rect/ellipse outline vs filled counts,
brush dedupe at overlaps, mirror produces 2/4 points and dedupes on the
axis, flood stops at a wall and at `None`.

### 1.2 `TilesetOp::DeleteRow { cols, at_top }` / `DeleteColumn { cols, at_left }`

Same shape as the insert ops: pad to full rows, then remove the edge
row (`cols` tiles) or one tile from every row; `tile_count` shrinks
accordingly; a strip with one row (or one column) refuses (no-op, no
undo entry). Undo is the existing `InverseOp::Reshape` snapshot. Tests:
delete after insert restores the strip; undo restores; the last row /
column refuses.

## 2. Editor half (`ggo_tileset_panel`)

### 2.1 Tools and toolbar

`Tool` += `Fill`, `Line`, `Rect`, `Ellipse`. `OpenTileset` gains
`brush: usize` (1–4), `mirror_h`, `mirror_v`, `shape_drag: Option<(anchor, head, filled)>`,
`focus: Option<usize>`, `move_drag: Option<(start, head)>`.

Toolbar: the four new tool buttons; brush stepper (`[` / `]`); Mirror H /
Mirror V toggles; Flip H / Flip V (enabled with a selection); Focus /
Back. Edge "−" bars beside the "+" bars delete the top/bottom row and
left/right column.

### 2.2 Painting path

One helper, `write_points(points: &[(i32,i32)], color)`: map every sheet
point through `doc_pixel` (dropping pad cells), then
`begin_stroke` / `apply_stroke_paint` × N / `end_stroke` — one undo step
per gesture, the same path paste already uses.

- Pencil / Eraser drags: each move's point goes through
  `brush_expand` then `mirror_in_tile` before `write_points`, inside the
  gesture's open stroke (drag = one undo step, as today).
- Line / Rect / Ellipse: mouse-down anchors; moves update `shape_drag`
  and the overlay draws the preview points (accent-coloured squares);
  release computes the points (`filled` = shift held at release), applies
  brush + mirror, `write_points` once.
- Fill: click → `flood` with `sample` = the doc colour through
  `doc_pixel` → `write_points` with the paint colour.

### 2.3 Selection move + flip

Select tool, mouse-down **inside** the marquee → `move_drag`; moves
update it and the overlay draws the marquee at the offset; release with
a non-zero offset: read the region's pixels, write slot 0 over the
source, then the pixels at the destination (points off the sheet are
dropped), one stroke; the marquee follows. Mouse-down outside the
marquee starts a new marquee as today.

Flip H / Flip V: rewrite the marquee's region with columns / rows
reversed, one stroke. Keys: `shift-h`, `shift-v`.

### 2.4 Focus mode

`focus = Some(tile)`: `recompose_grid` composes a 1-column grid of just
that tile, `grid_size` is one tile, `doc_pixel` maps `(sx, sy)` to
`(tile, sx, sy)`, and the zoom shown is `max(zoom, 8)`. Enter by
double-clicking a tile (any tool) or the Focus button; `←`/`→` step to
the previous/next tile; `escape` or Back returns to the sheet (zoom and
scroll restored). The selection is cleared on enter/exit. Every tool
and the palette work unchanged.

### 2.5 Keys (panel context)

`[`/`]` brush, `shift-h`/`shift-v` flip, `escape` leave focus (else clear
selection), `left`/`right` in focus. Existing bindings unchanged.

## 3. Testing

worldlib: §1. Panel: line drag paints its endpoints and is one undo
step; rect filled vs outline; fill across a tile border stops at a
different colour; brush 2 paints a 2×2; mirror H paints the reflection
in the same tile; move-drag clears the source and writes the
destination in one undo; flip H reverses a row; delete row shrinks
`tile_count` and undo restores; focus composes one tile, paints map to
it, arrows step, escape returns.

## 4. Out of scope

Rotate, arbitrary-angle transforms, per-sheet mirror axes, a second
canvas, brush shapes other than square.
