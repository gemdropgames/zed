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

/// Which pool tile a click at `(local_x, local_y)` inside the tile
/// picker's sheet lands on. The sheet is a `cols`-wide grid of
/// `cell_px`-square cells (`loader::compose_pool_strip` composes it,
/// `render_tile_picker` scales it so one tile is exactly `cell_px` CSS
/// px), row-major -- the same shape [`cell_at`] walks, but sized by a
/// per-cell edge rather than by a fit box, and bounded by `tile_count`
/// instead of the grid: the last row of a sheet whose tile count isn't a
/// multiple of `cols` is zero-padded, and a click on that padding must
/// select nothing rather than a tile the pool doesn't have.
pub fn picker_tile_at(
    local_x: f32,
    local_y: f32,
    cell_px: f32,
    cols: usize,
    tile_count: usize,
) -> Option<u16> {
    if cell_px <= 0.0 || cols == 0 || local_x < 0.0 || local_y < 0.0 {
        return None;
    }
    let col = (local_x / cell_px) as usize;
    let row = (local_y / cell_px) as usize;
    if col >= cols {
        return None;
    }
    let index = row * cols + col;
    (index < tile_count).then(|| index as u16)
}

/// The hardware-budget meter line for the open sprite: `Pool`
/// (`tile_count` / `VRAM_TILE_CAP`), `OAM` (greedy [`hw::oam_split`]
/// entry count / `OAM_ENTRIES`), `Scanline` (worst-case per-scanline OAM
/// coverage / `SPRITE_LINE_CAP`), and `Cache` (the shown frame's
/// worst-row distinct-tile working set / `SPRITE_CACHE_TILES`; 0 without
/// a frame). Same inputs, same order as ggo-ide `hw_meter::rows`.
pub fn hw_meter_line(state: &SpriteState, frame: Option<&Frame>) -> String {
    let w = state.w_tiles as usize;
    let h = state.h_tiles as usize;
    let oam = hw::oam_split(w, h).len();
    let scanline = hw::scanline_coverage(w, h);
    let cache = frame.map_or(0, |f| hw::cache_pressure(&f.map, w, h));
    format!(
        "Pool {}/{VRAM_TILE_CAP} · OAM {oam}/{OAM_ENTRIES} · Scanline {scanline}/{SPRITE_LINE_CAP} · Cache {cache}/{SPRITE_CACHE_TILES}",
        state.tile_count
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

    // ----------------------------------------------------- picker_tile_at

    #[test]
    fn picker_tile_at_is_row_major_over_the_sheet_grid() {
        // 4 columns of 24px cells, 6 tiles => two rows, the second half
        // full.
        assert_eq!(picker_tile_at(0.0, 0.0, 24.0, 4, 6), Some(0));
        assert_eq!(picker_tile_at(23.9, 23.9, 24.0, 4, 6), Some(0));
        assert_eq!(picker_tile_at(24.0, 0.0, 24.0, 4, 6), Some(1));
        assert_eq!(picker_tile_at(72.0, 0.0, 24.0, 4, 6), Some(3));
        assert_eq!(picker_tile_at(0.0, 24.0, 24.0, 4, 6), Some(4));
        assert_eq!(picker_tile_at(24.0, 24.0, 24.0, 4, 6), Some(5));
    }

    #[test]
    fn picker_tile_at_rejects_the_padded_tail_of_a_partial_last_row() {
        // 6 tiles at 4 cols: cells 6 and 7 are zero-fill, not tiles.
        assert_eq!(picker_tile_at(48.0, 24.0, 24.0, 4, 6), None);
        assert_eq!(picker_tile_at(72.0, 24.0, 24.0, 4, 6), None);
    }

    #[test]
    fn picker_tile_at_rejects_clicks_outside_the_sheet() {
        assert_eq!(picker_tile_at(-0.1, 0.0, 24.0, 4, 6), None);
        assert_eq!(picker_tile_at(0.0, -0.1, 24.0, 4, 6), None);
        assert_eq!(
            picker_tile_at(96.0, 0.0, 24.0, 4, 6),
            None,
            "past the last column"
        );
        assert_eq!(picker_tile_at(0.0, 96.0, 24.0, 4, 6), None, "past the rows");
    }

    #[test]
    fn picker_tile_at_rejects_degenerate_geometry() {
        assert_eq!(picker_tile_at(0.0, 0.0, 0.0, 4, 6), None);
        assert_eq!(picker_tile_at(0.0, 0.0, 24.0, 0, 6), None);
        assert_eq!(picker_tile_at(0.0, 0.0, 24.0, 4, 0), None, "empty pool");
    }

    // ------------------------------------------------------ hw_meter_line

    #[test]
    fn hw_meter_line_matches_the_hw_calculators_for_a_4x4_sprite() {
        let s = blank_sprite_state(4, 4).unwrap();
        // 4x4 -> one 4x4 OAM square -> scanline coverage 1; the blank
        // frame is a single tile everywhere -> cache pressure 1.
        assert_eq!(
            hw_meter_line(&s, s.frames.first()),
            format!(
                "Pool 1/{VRAM_TILE_CAP} · OAM 1/{OAM_ENTRIES} · Scanline 1/{SPRITE_LINE_CAP} · Cache 1/{SPRITE_CACHE_TILES}"
            )
        );
    }

    #[test]
    fn hw_meter_line_without_a_frame_reads_cache_0_like_hw_meter_rows() {
        let s = blank_sprite_state(2, 2).unwrap();
        let line = hw_meter_line(&s, None);
        assert!(
            line.ends_with(&format!("Cache 0/{SPRITE_CACHE_TILES}")),
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
