//! The map panel's PURE geometry and clamping: cell hit-testing under
//! zoom/pan, on-screen sizes, the resize dimension rule, and the palSub /
//! zoom clamps. Nothing here touches gpui, the store, or the filesystem, so
//! all of it is unit-tested directly -- the discipline `ggo_world_panel`'s
//! `canvas` module established (spec risk 2: "keep the pure parts (stamp
//! geometry, cell hit-testing, resize clamping) out of the gpui layer and
//! unit-tested").
//!
//! **Camera model.** Both surfaces (the map canvas and the tileset strip)
//! draw a tile grid at an INTEGER zoom with a pan offset in canvas-local
//! CSS px, so one cell occupies `TILE_PX * zoom` px and the grid's top-left
//! sits at `pan`. That is deliberately simpler than `ggo_world_panel`'s
//! float camera: the map is pixel art composed at 1:1, and a fractional
//! scale would resample 16x16 tiles into blur (the same call
//! `ggo_tileset_panel` made for its sheet view).

use ggo_worldlib::sprites::tileset_doc::TILE_PX;

/// Integer zoom bounds for both surfaces. 1x is unreadably small for
/// 16x16 tiles on a HiDPI panel, so the default is 2x -- same ladder and
/// default as `ggo_tileset_panel`.
pub const MIN_ZOOM: usize = 1;
pub const MAX_ZOOM: usize = 8;
pub const DEFAULT_ZOOM: usize = 2;

/// The tileset strip is a picker, not the working surface, so it gets its
/// own (fixed, small) scale rather than following the canvas zoom.
pub const STRIP_ZOOM: usize = 2;

/// Map dimension bounds -- ggo-ide's `pages::assets::{MIN_MAP_DIM,
/// MAX_MAP_DIM}`, ported verbatim. The cap is a UI limit, not a format
/// one: EMAP v3 stores `w`/`h` as `u16`, but a 256x256 map already
/// composes ~16.8M pixels per repaint.
pub const MIN_MAP_DIM: u16 = 1;
pub const MAX_MAP_DIM: u16 = 256;

/// A new map's default size -- ggo-ide's `NEW_MAP_DEFAULT_DIM`.
pub const NEW_MAP_DIM: u16 = 16;

/// The packed cell's palSub field range (4 bits, `[13:10]`) -- ggo-ide's
/// `{PAL_SUB_MIN, PAL_SUB_MAX}`.
pub const PAL_SUB_MIN: u16 = 0;
pub const PAL_SUB_MAX: u16 = 15;

/// The grid cell under canvas-local `local` px for a `cols` x `rows` tile
/// grid drawn at integer `zoom` with top-left at `pan`, or `None` when the
/// point falls outside the grid.
///
/// Used by BOTH surfaces: the map canvas (cols/rows = the map's `w`/`h`)
/// and the tileset strip (cols/rows = the sheet's layout). Out-of-grid is
/// `None` rather than a clamped edge cell, which is the gate ggo-ide's
/// `PixelEvent::in_bounds` applies before every map/palette pointer
/// action -- a drag that leaves the canvas must stop painting, not smear
/// along the border.
///
/// Truncating division would fold `-0.5` and `+0.5` onto the same cell 0,
/// so the negative side is rejected before the divide rather than floored
/// into a bogus hit.
pub fn grid_cell_at(
    local: [f32; 2],
    zoom: usize,
    pan: [f32; 2],
    cols: u16,
    rows: u16,
) -> Option<(i32, i32)> {
    let step = (TILE_PX * zoom.max(1)) as f32;
    let x = local[0] - pan[0];
    let y = local[1] - pan[1];
    if x < 0.0 || y < 0.0 {
        return None;
    }
    let cx = (x / step) as i32;
    let cy = (y / step) as i32;
    (cx < cols as i32 && cy < rows as i32).then_some((cx, cy))
}

/// A `cols` x `rows` tile grid's on-screen size at integer `zoom`.
pub fn grid_pixel_size(cols: u16, rows: u16, zoom: usize) -> (f32, f32) {
    let step = (TILE_PX * zoom.max(1)) as f32;
    (cols as f32 * step, rows as f32 * step)
}

/// A tile pool index's `(col, row)` in a `cols`-wide sheet -- the inverse
/// of `map_doc::build_stamp`'s own `row * cols + col` indexing, which is
/// what the eyedropper needs to move the strip's selection onto the tile
/// it just picked out of a cell.
pub fn tile_cell(tile: u16, cols: usize) -> (i32, i32) {
    let cols = cols.max(1) as i32;
    (tile as i32 % cols, tile as i32 / cols)
}

/// Clamp a raw dimension to the map size limits.
pub fn clamp_dim(raw: i64) -> u16 {
    raw.clamp(MIN_MAP_DIM as i64, MAX_MAP_DIM as i64) as u16
}

/// Parse a resize field's text into a legal dimension, or `None` when it
/// isn't a number at all.
///
/// An out-of-range NUMBER clamps rather than rejecting (typing `9999`
/// gives a 256-wide map, ggo-ide's `parse().unwrap_or(default).clamp(..)`
/// behavior), but a non-number is `None` so an "Apply" on garbage text
/// leaves the document alone instead of silently resizing it to the
/// minimum.
pub fn parse_dim(text: &str) -> Option<u16> {
    text.trim().parse::<i64>().ok().map(clamp_dim)
}

/// Step the integer zoom by `delta`, clamped to the ladder ends.
pub fn zoom_by(zoom: usize, delta: isize) -> usize {
    (zoom as isize + delta).clamp(MIN_ZOOM as isize, MAX_ZOOM as isize) as usize
}

/// New pan such that the map pixel under canvas-local `cursor` stays under
/// it after switching from `zoom` to `new_zoom` -- the cursor-anchored
/// wheel zoom `ggo_world_panel::canvas::zoom_at` does, restated for this
/// panel's integer camera (`screen = pan + world * zoom`).
pub fn zoom_at(pan: [f32; 2], zoom: usize, cursor: [f32; 2], new_zoom: usize) -> [f32; 2] {
    let (z, nz) = (zoom.max(1) as f32, new_zoom.max(1) as f32);
    [
        cursor[0] - (cursor[0] - pan[0]) / z * nz,
        cursor[1] - (cursor[1] - pan[1]) / z * nz,
    ]
}

    #[cfg(test)]
/// Step the brush's palSub by `delta`, clamped to the 4-bit field.
pub fn pal_sub_by(pal_sub: u16, delta: i32) -> u16 {
    (pal_sub as i32 + delta).clamp(PAL_SUB_MIN as i32, PAL_SUB_MAX as i32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::map_doc::{build_stamp, pack_cell};

    /// At 1x with no pan, cell boundaries land exactly on `TILE_PX`
    /// multiples, and anything past the last cell -- or left/above the
    /// origin -- is a miss.
    #[test]
    fn grid_cell_at_maps_pixels_to_cells_at_identity() {
        let hit = |x: f32, y: f32| grid_cell_at([x, y], 1, [0.0, 0.0], 4, 3);
        assert_eq!(hit(0.0, 0.0), Some((0, 0)));
        assert_eq!(hit(15.9, 15.9), Some((0, 0)));
        assert_eq!(hit(16.0, 0.0), Some((1, 0)));
        assert_eq!(hit(0.0, 32.0), Some((0, 2)));
        // Past the right edge (4 cols = 64 px) and past the bottom (3 rows
        // = 48 px).
        assert_eq!(hit(64.0, 0.0), None);
        assert_eq!(hit(0.0, 48.0), None);
        // Negative side: rejected, NOT truncated onto cell 0.
        assert_eq!(hit(-0.5, 0.0), None);
        assert_eq!(hit(0.0, -0.5), None);
    }

    /// Zoom scales the cell step, pan shifts the whole grid: a point that
    /// hit cell (0,0) at identity hits it again exactly `pan` px later,
    /// and a point before `pan` misses entirely.
    #[test]
    fn grid_cell_at_accounts_for_zoom_and_pan() {
        let hit = |x: f32, y: f32| grid_cell_at([x, y], 4, [100.0, 50.0], 4, 3);
        let step = (TILE_PX * 4) as f32; // 64 px per cell
        assert_eq!(hit(100.0, 50.0), Some((0, 0)));
        assert_eq!(hit(99.9, 50.0), None, "left of the panned origin is a miss");
        assert_eq!(hit(100.0 + step - 0.1, 50.0), Some((0, 0)));
        assert_eq!(hit(100.0 + step, 50.0), Some((1, 0)));
        assert_eq!(hit(100.0, 50.0 + step * 2.0), Some((0, 2)));
        assert_eq!(hit(100.0 + step * 4.0, 50.0), None);
        assert_eq!(hit(100.0, 50.0 + step * 3.0), None);
    }

    /// A zoom of 0 would divide by zero; the floor keeps it at 1x.
    #[test]
    fn grid_cell_at_survives_a_zero_zoom() {
        assert_eq!(grid_cell_at([0.0, 0.0], 0, [0.0, 0.0], 1, 1), Some((0, 0)));
        assert_eq!(grid_pixel_size(1, 1, 0), (TILE_PX as f32, TILE_PX as f32));
    }

    #[test]
    fn grid_pixel_size_scales_by_zoom() {
        assert_eq!(
            grid_pixel_size(4, 3, 2),
            ((4 * TILE_PX * 2) as f32, (3 * TILE_PX * 2) as f32)
        );
    }

    /// The eyedropper's inverse of `build_stamp`'s indexing -- checked
    /// against `build_stamp` ITSELF, not against a restatement of its
    /// formula.
    ///
    /// (Fix round 1, FOLD IN 5: the first version asserted
    /// `r * cols + c == tile`, which is the same expression `tile_cell`
    /// inverts, and ran on a 4-tile/4-column/1-row fixture where a
    /// `row * cols` error can't show up at all. This one picks a stamp of
    /// exactly the eyedropped tile out of a genuinely 2-D sheet -- 5
    /// columns, 5 rows, 23 tiles, so both a transposed `(c, r)` and a
    /// wrong stride land on the wrong tile.)
    #[test]
    fn tile_cell_selects_the_same_tile_build_stamp_would() {
        const COLS: usize = 5;
        const TILES: usize = 23; // 5 rows, the last one partial
        for tile in 0u16..TILES as u16 {
            let (c, r) = tile_cell(tile, COLS);
            assert!(r >= 1 || tile < COLS as u16, "tile {tile} must leave row 0");
            let stamp = build_stamp((c, r, c, r), COLS, TILES, 0, false, false);
            assert_eq!((stamp.w, stamp.h), (1, 1));
            assert_eq!(
                stamp.cells,
                vec![pack_cell(tile, 0, false, false)],
                "tile {tile} at ({c}, {r})"
            );
        }
        assert_eq!(tile_cell(0, 0), (0, 0), "a zero-column sheet can't divide");
    }

    /// Resize clamping: legal values pass through, out-of-range NUMBERS
    /// clamp to the limits, and non-numbers are rejected outright (so an
    /// Apply on garbage doesn't resize the map to 1x1).
    #[test]
    fn parse_dim_clamps_numbers_and_rejects_garbage() {
        assert_eq!(parse_dim("16"), Some(16));
        assert_eq!(parse_dim("  32 "), Some(32));
        assert_eq!(parse_dim("0"), Some(MIN_MAP_DIM));
        assert_eq!(parse_dim("-4"), Some(MIN_MAP_DIM));
        assert_eq!(parse_dim("9999"), Some(MAX_MAP_DIM));
        assert_eq!(parse_dim("99999999999999999999"), None, "not an i64");
        assert_eq!(parse_dim(""), None);
        assert_eq!(parse_dim("abc"), None);
        assert_eq!(parse_dim("12.5"), None);
    }

    /// The cursor-anchored zoom invariant: the cell under the cursor before
    /// the zoom is the cell under it after.
    #[test]
    fn zoom_at_keeps_the_cell_under_the_cursor_fixed() {
        let pan = [17.0, -9.0];
        let cursor = [123.0, 77.0];
        for (zoom, new_zoom) in [(1usize, 4usize), (4, 1), (2, 3), (8, 2)] {
            let before = grid_cell_at(cursor, zoom, pan, 64, 64);
            let new_pan = zoom_at(pan, zoom, cursor, new_zoom);
            let after = grid_cell_at(cursor, new_zoom, new_pan, 64, 64);
            assert_eq!(before, after, "{zoom}x -> {new_zoom}x");
        }
        // A no-op zoom is a no-op pan.
        assert_eq!(zoom_at(pan, 3, cursor, 3), pan);
    }

    #[test]
    fn zoom_and_pal_sub_clamp_at_both_ends() {
        assert_eq!(zoom_by(DEFAULT_ZOOM, 1), DEFAULT_ZOOM + 1);
        assert_eq!(zoom_by(MAX_ZOOM, 1), MAX_ZOOM);
        assert_eq!(zoom_by(MIN_ZOOM, -1), MIN_ZOOM);
        assert_eq!(zoom_by(MIN_ZOOM, -100), MIN_ZOOM);
        assert_eq!(pal_sub_by(0, -1), PAL_SUB_MIN);
        assert_eq!(pal_sub_by(PAL_SUB_MAX, 1), PAL_SUB_MAX);
        assert_eq!(pal_sub_by(3, 2), 5);
    }
}
