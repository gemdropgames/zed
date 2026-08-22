//! Pure tile-editing helpers (F2 task M6): preview cell hit math and the
//! hardware-budget meter line -- the framework-free half of the tile
//! palette / cell-set wiring, kept out of the gpui layer so both rules are
//! directly unit-testable. The meter FEED mirrors ggo-ide's
//! `sprites/hw_meter.rs::rows` (same four value/cap pairs from
//! `sprites::hw`'s pure calculators, same "only the cache row is
//! frame-specific" split); the presentation is a single compact text line
//! instead of iced's four dot-colored rows, sized for a 360px sidebar.

use ggo_worldlib::sprites::cow::{Frame, SpriteState};
use ggo_worldlib::sprites::hw::{
    self, OAM_ENTRIES, SPRITE_CACHE_TILES, SPRITE_LINE_CAP, VRAM_TILE_CAP,
};

/// Which frame cell a click at `(local_x, local_y)` inside a preview
/// rendered at `fit_w x fit_h` CSS px lands on. The composed frame image
/// is a `w_tiles x h_tiles` grid of `TILE_PX` tiles scaled uniformly into
/// the fit box ([`super::playback::fit_size`] preserves aspect), so the
/// box divides evenly into `w_tiles` columns and `h_tiles` rows -- no
/// TILE_PX term survives the scale. Returns the row-major cell index, or
/// `None` for a click outside the box (or a degenerate box/grid). The
/// `min` clamps guard the exact-right/bottom-edge float case (`local ==
/// fit` is already rejected, but `fit_w / w_tiles` rounding can land a
/// last-column hit on `w_tiles` otherwise).
pub fn cell_at(
    local_x: f32,
    local_y: f32,
    fit_w: f32,
    fit_h: f32,
    w_tiles: usize,
    h_tiles: usize,
) -> Option<usize> {
    if fit_w <= 0.0 || fit_h <= 0.0 || w_tiles == 0 || h_tiles == 0 {
        return None;
    }
    if local_x < 0.0 || local_y < 0.0 || local_x >= fit_w || local_y >= fit_h {
        return None;
    }
    let col = ((local_x / (fit_w / w_tiles as f32)) as usize).min(w_tiles - 1);
    let row = ((local_y / (fit_h / h_tiles as f32)) as usize).min(h_tiles - 1);
    Some(row * w_tiles + col)
}

/// Where the tile picker sheet's zero-fill padding starts, as `(col,
/// row)` of the first pad cell -- `None` when the last row is full (or
/// the grid is degenerate). The sheet is composed `cols` wide with a
/// zero-filled partial last row (`compose_tile_grid`); those cells are
/// not tiles and must not be dressed as ones, so the picker overlay
/// blanks everything from this cell to the sheet's corner.
pub fn picker_pad_region(tile_count: usize, cols: usize) -> Option<(usize, usize)> {
    if tile_count == 0 || cols == 0 {
        return None;
    }
    let rem = tile_count % cols;
    (rem != 0).then(|| (rem, tile_count / cols))
}

/// A rectangular multi-tile selection from the picker sheet: the
/// marquee's normalized rect plus the pool tiles under it, row-major.
/// Pad cells (sheet slots past the pool's tile count) select `None` --
/// they stamp nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct TileBlock {
    /// Top-left of the normalized rect, `(col, row)` in sheet cells.
    pub origin: (usize, usize),
    pub cols: usize,
    pub rows: usize,
    /// Row-major, `cols * rows` entries.
    pub tiles: Vec<Option<u16>>,
}

impl TileBlock {
    /// The single selected tile, when the block is exactly one real tile
    /// -- the shape every pre-marquee code path (and toggle-deselect)
    /// still works in.
    pub fn single(&self) -> Option<u16> {
        match (self.cols, self.rows, self.tiles.as_slice()) {
            (1, 1, [tile]) => *tile,
            _ => None,
        }
    }
}

/// The block between two marquee corners `a`/`b` (each `(col, row)`, any
/// order) over a `sheet_cols`-wide sheet whose cells show the POOL tiles
/// in `display` (blank tiles are excluded from the sheet, so a sheet
/// index maps through this table rather than being a pool index itself).
pub fn marquee_block(
    a: (usize, usize),
    b: (usize, usize),
    sheet_cols: usize,
    display: &[u16],
) -> TileBlock {
    let (c0, c1) = (a.0.min(b.0), a.0.max(b.0));
    let (r0, r1) = (a.1.min(b.1), a.1.max(b.1));
    let cols = c1 - c0 + 1;
    let rows = r1 - r0 + 1;
    let mut tiles = Vec::with_capacity(cols * rows);
    for row in r0..=r1 {
        for col in c0..=c1 {
            tiles.push(display.get(row * sheet_cols + col).copied());
        }
    }
    TileBlock {
        origin: (c0, r0),
        cols,
        rows,
        tiles,
    }
}

/// The `(cell, tile)` pairs a block stamp writes: `anchor_cell` is the
/// clicked frame cell (the block's top-left), the block is clipped at the
/// frame's right/bottom edges, pad (`None`) entries stamp nothing, and
/// already-matching cells are dropped so an empty batch means "no op to
/// push" (undo-stack hygiene).
pub fn stamp_sets(
    block: &TileBlock,
    anchor_cell: usize,
    w_tiles: usize,
    h_tiles: usize,
    map: &[u16],
) -> Vec<(usize, u16)> {
    if w_tiles == 0 {
        return Vec::new();
    }
    let anchor_col = anchor_cell % w_tiles;
    let anchor_row = anchor_cell / w_tiles;
    let mut sets = Vec::new();
    for row in 0..block.rows {
        for col in 0..block.cols {
            let Some(Some(tile)) = block.tiles.get(row * block.cols + col) else {
                continue;
            };
            let (target_col, target_row) = (anchor_col + col, anchor_row + row);
            if target_col >= w_tiles || target_row >= h_tiles {
                continue;
            }
            let cell = target_row * w_tiles + target_col;
            if map.get(cell) == Some(tile) {
                continue;
            }
            sets.push((cell, *tile));
        }
    }
    sets
}

/// The hardware's shared sprite affine parameter-set budget (ggo
/// `docs/ppu-contract.md` §9: `pidx[4:0]` picks one of 32 sets). Not in
/// worldlib's `hw` module yet, so pinned here where the meter needs it.
pub const AFFINE_PARAM_SETS: usize = 32;

/// The hardware-budget meter line for the open sprite: `Pool`
/// (`tile_count` / `VRAM_TILE_CAP`), `OAM` (greedy [`hw::oam_split`]
/// entry count / `OAM_ENTRIES`), `Scanline` (worst-case per-scanline OAM
/// coverage / `SPRITE_LINE_CAP`), `Cache` (the shown frame's worst-row
/// distinct-tile working set / `SPRITE_CACHE_TILES`; 0 without a frame),
/// and `Sets` (distinct non-identity transform MATRICES across the
/// sprite's frames / [`AFFINE_PARAM_SETS`] -- counted on the composed
/// `matrix()`, the value the runtime's dedup allocator actually keys
/// sets by, so two transforms that collapse to one matrix cost one
/// set). Same inputs, same order as ggo-ide `hw_meter::rows` plus the
/// affine budget.
pub fn hw_meter_line(state: &SpriteState, frame: Option<&Frame>) -> String {
    let w = state.w_tiles as usize;
    let h = state.h_tiles as usize;
    let oam = hw::oam_split(w, h).len();
    let scanline = hw::scanline_coverage(w, h);
    let cache = frame.map_or(0, |f| hw::cache_pressure(&f.map, w, h));
    let mut matrices: Vec<(u32, u32)> = Vec::new();
    for f in &state.frames {
        if f.transform.is_identity() {
            continue;
        }
        let matrix = f.transform.matrix();
        if !matrices.contains(&matrix) {
            matrices.push(matrix);
        }
    }
    format!(
        "Pool {}/{VRAM_TILE_CAP} · OAM {oam}/{OAM_ENTRIES} · Scanline {scanline}/{SPRITE_LINE_CAP} · Cache {cache}/{SPRITE_CACHE_TILES} · Sets {}/{AFFINE_PARAM_SETS}",
        state.tile_count,
        matrices.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::sprite_doc::blank_sprite_state;

    // ------------------------------------------------------------ cell_at

    #[test]
    fn cell_at_maps_quadrants_of_a_2x2_grid() {
        // 96x96 box over a 2x2 grid: 48px cells.
        assert_eq!(cell_at(0.0, 0.0, 96.0, 96.0, 2, 2), Some(0));
        assert_eq!(cell_at(47.9, 47.9, 96.0, 96.0, 2, 2), Some(0));
        assert_eq!(cell_at(48.0, 0.0, 96.0, 96.0, 2, 2), Some(1));
        assert_eq!(cell_at(0.0, 48.0, 96.0, 96.0, 2, 2), Some(2));
        assert_eq!(cell_at(95.9, 95.9, 96.0, 96.0, 2, 2), Some(3));
    }

    #[test]
    fn cell_at_is_row_major_on_a_non_square_grid() {
        // 90x60 box over 3x2: 30px cells both ways.
        assert_eq!(cell_at(31.0, 5.0, 90.0, 60.0, 3, 2), Some(1));
        assert_eq!(cell_at(61.0, 31.0, 90.0, 60.0, 3, 2), Some(5));
        assert_eq!(cell_at(5.0, 31.0, 90.0, 60.0, 3, 2), Some(3));
    }

    #[test]
    fn cell_at_rejects_clicks_outside_the_box() {
        assert_eq!(cell_at(-0.1, 0.0, 96.0, 96.0, 2, 2), None);
        assert_eq!(cell_at(0.0, -0.1, 96.0, 96.0, 2, 2), None);
        assert_eq!(
            cell_at(96.0, 0.0, 96.0, 96.0, 2, 2),
            None,
            "right edge is exclusive"
        );
        assert_eq!(
            cell_at(0.0, 96.0, 96.0, 96.0, 2, 2),
            None,
            "bottom edge is exclusive"
        );
    }

    #[test]
    fn cell_at_rejects_degenerate_boxes_and_grids() {
        assert_eq!(cell_at(0.0, 0.0, 0.0, 96.0, 2, 2), None);
        assert_eq!(cell_at(0.0, 0.0, 96.0, 0.0, 2, 2), None);
        assert_eq!(cell_at(0.0, 0.0, 96.0, 96.0, 0, 2), None);
        assert_eq!(cell_at(0.0, 0.0, 96.0, 96.0, 2, 0), None);
    }

    #[test]
    fn cell_at_clamps_float_rounding_at_the_far_edges_into_the_last_cell() {
        // A fit width that doesn't divide evenly: 100/3 columns. The last
        // in-box x must still land in column 2, never column 3.
        assert_eq!(cell_at(99.999, 0.0, 100.0, 96.0, 3, 1), Some(2));
    }

    #[test]
    fn cell_at_on_a_1x1_grid_is_always_cell_0_inside_the_box() {
        assert_eq!(cell_at(0.0, 0.0, 240.0, 240.0, 1, 1), Some(0));
        assert_eq!(cell_at(239.9, 239.9, 240.0, 240.0, 1, 1), Some(0));
    }

    // -------------------------------------------------------- marquee_block

    #[test]
    fn marquee_block_normalizes_the_rect_and_reads_tiles_row_major() {
        // 4-col sheet, 8 tiles; drag from (2,1) up-left to (1,0).
        let block = marquee_block((2, 1), (1, 0), 4, &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!((block.origin, block.cols, block.rows), ((1, 0), 2, 2));
        assert_eq!(
            block.tiles,
            vec![Some(1), Some(2), Some(5), Some(6)],
            "sheet indices row-major from the normalized top-left"
        );
    }

    #[test]
    fn marquee_block_of_a_single_cell_is_a_1x1_block() {
        let block = marquee_block((3, 0), (3, 0), 4, &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!((block.cols, block.rows), (1, 1));
        assert_eq!(block.tiles, vec![Some(3)]);
    }

    #[test]
    fn marquee_block_marks_pad_cells_as_none() {
        // 6 tiles at 4 cols: rect over the ragged last row includes the
        // two zero-fill pad cells, which must select nothing.
        let block = marquee_block((1, 1), (3, 1), 4, &[0, 1, 2, 3, 4, 5]);
        assert_eq!(block.tiles, vec![Some(5), None, None]);
    }

    // ---------------------------------------------------------- stamp_sets

    #[test]
    fn stamp_sets_anchors_the_block_and_skips_unchanged_cells() {
        let block = marquee_block((0, 0), (1, 1), 4, &[0, 1, 2, 3, 4, 5, 6, 7]); // tiles 0,1,4,5
        // 3x3 frame, cell 7 already holds tile 5 (the block's
        // bottom-right lands there and must be dropped as unchanged).
        let map = vec![9, 9, 9, 9, 9, 9, 9, 5, 9];
        let sets = stamp_sets(&block, 3, 3, 3, &map);
        assert_eq!(
            sets,
            vec![(3, 0), (4, 1), (6, 4)],
            "anchor cell 3 = block top-left; (7 -> 5) dropped as unchanged"
        );
    }

    #[test]
    fn stamp_sets_clips_the_block_at_the_frame_edges() {
        let block = marquee_block((0, 0), (1, 1), 4, &[0, 1, 2, 3, 4, 5, 6, 7]);
        // 2x2 frame, anchored at bottom-right cell: only the block's
        // top-left lands.
        let sets = stamp_sets(&block, 3, 2, 2, &[9, 9, 9, 9]);
        assert_eq!(sets, vec![(3, 0)]);
    }

    #[test]
    fn stamp_sets_skips_pad_cells_and_empty_batches() {
        let block = marquee_block((2, 1), (3, 1), 4, &[0, 1, 2, 3, 4, 5]); // [None, None]
        assert!(stamp_sets(&block, 0, 2, 2, &[9, 9, 9, 9]).is_empty());
    }

    // ------------------------------------------------------ hw_meter_line

    #[test]
    fn hw_meter_line_matches_the_hw_calculators_for_a_4x4_sprite() {
        let s = blank_sprite_state(4, 4).unwrap();
        // 4x4 -> one 4x4 OAM square -> scanline coverage 1; the blank
        // frame is a single tile everywhere -> cache pressure 1; every
        // transform is identity -> no affine sets.
        assert_eq!(
            hw_meter_line(&s, s.frames.first()),
            format!(
                "Pool 1/{VRAM_TILE_CAP} · OAM 1/{OAM_ENTRIES} · Scanline 1/{SPRITE_LINE_CAP} · Cache 1/{SPRITE_CACHE_TILES} · Sets 0/{AFFINE_PARAM_SETS}"
            )
        );
    }

    #[test]
    fn hw_meter_line_without_a_frame_reads_cache_0_like_hw_meter_rows() {
        let s = blank_sprite_state(2, 2).unwrap();
        let line = hw_meter_line(&s, None);
        assert!(
            line.contains(&format!("Cache 0/{SPRITE_CACHE_TILES}")),
            "{line}"
        );
    }

    #[test]
    fn hw_meter_counts_distinct_non_identity_transforms() {
        use ggo_worldlib::sprites::cow::FrameTransform;
        let mut s = blank_sprite_state(1, 1).unwrap();
        let base = s.frames[0].clone();
        s.frames = vec![base.clone(), base.clone(), base.clone(), base];
        let quarter = FrameTransform {
            angle256: 64,
            ..FrameTransform::IDENTITY
        };
        let doubled = FrameTransform {
            sx: 0x0200,
            ..FrameTransform::IDENTITY
        };
        // Frame 0 stays identity (not counted); 1 and 2 share a matrix
        // (one set); 3 is distinct (a second set).
        s.frames[1].transform = quarter;
        s.frames[2].transform = quarter;
        s.frames[3].transform = doubled;
        let line = hw_meter_line(&s, s.frames.first());
        assert!(
            line.ends_with(&format!("Sets 2/{AFFINE_PARAM_SETS}")),
            "{line}"
        );
    }

    #[test]
    fn hw_meter_line_cache_counts_the_worst_rows_distinct_tiles() {
        let mut s = blank_sprite_state(2, 1).unwrap();
        // Two distinct tiles on the single row.
        s.pool.extend(std::iter::repeat_n(0x11u8, 128));
        s.tile_count = 2;
        s.frames[0].map = vec![0, 1];
        let line = hw_meter_line(&s, s.frames.first());
        assert!(
            line.contains(&format!("Cache 2/{SPRITE_CACHE_TILES}")),
            "{line}"
        );
        assert!(
            line.starts_with(&format!("Pool 2/{VRAM_TILE_CAP}")),
            "{line}"
        );
    }
}
