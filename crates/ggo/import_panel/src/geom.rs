//! The import panel's PURE geometry: the crop canvas's camera (integer
//! zoom + pan), image-pixel hit-testing, and the readouts that describe
//! what a commit is about to slice. Nothing here touches gpui, worldlib's
//! wizard state, or the filesystem, so all of it is unit-tested directly --
//! the discipline `ggo_map_panel::geom` and `ggo_world_panel::canvas`
//! established (spec risk 2: keep the pure parts out of the gpui layer and
//! unit-tested).
//!
//! **What is NOT here.** The crop rect itself, the cell-size clamp, the
//! quantized preview and the tile slicing are all worldlib's
//! (`sprites::import`), extracted and unit-tested there in ggo PR #81. This
//! module only adds the CAMERA (which worldlib deliberately leaves to the
//! caller -- see `WizardState::crop_anchor`'s doc, "the CALLER reports
//! already-clamped image coordinates") plus two derived readouts. Anything
//! that could plausibly live in worldlib does.
//!
//! **Camera model.** The source PNG is drawn at an INTEGER zoom with a pan
//! offset in canvas-local CSS px, so one source pixel occupies `zoom` px and
//! the image's top-left sits at `pan`. Integer only, same call
//! `ggo_tileset_panel` and `ggo_map_panel` made: the source is pixel art and
//! a fractional scale would resample it into blur -- and a crop rect read
//! off a blurred image would not be the crop the user drew.

use ggo_asset_formats::TILE_PX;
use ggo_worldlib::sprites::import::{Region, grid_lines, uniform_rects};

/// Integer zoom bounds for the crop canvas. A tile sheet's individual
/// pixels have to be aimable, so the ladder runs higher than the map
/// panel's tile-cell ladder needs to.
pub const MIN_ZOOM: usize = 1;
pub const MAX_ZOOM: usize = 16;
/// 2x, same default as the other two image panels: 1x is unreadably small
/// on a HiDPI dock.
pub const DEFAULT_ZOOM: usize = 2;

/// The image pixel under canvas-local `local` px, CLAMPED into the
/// `w` x `h` image.
///
/// Clamped rather than `Option`, unlike `ggo_map_panel::geom::grid_cell_at`,
/// and the difference is deliberate. A map click outside the grid must miss
/// (painting a cell that doesn't exist is meaningless), but a crop DRAG that
/// leaves the image must keep extending to the image's edge -- that is how
/// you select a strip flush against the border without landing the pointer
/// exactly on the last pixel. worldlib's wizard documents this as the
/// caller's job (`WizardState::on_primary_down`/`on_moved` take
/// "already-clamped image pixel" coordinates), so the clamp lives here.
///
/// A zero-sized image (nothing decoded yet) yields `(0, 0)`.
pub fn image_coord(local: [f32; 2], zoom: usize, pan: [f32; 2], w: usize, h: usize) -> (i32, i32) {
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let z = zoom.max(1) as f32;
    let x = ((local[0] - pan[0]) / z).floor() as i32;
    let y = ((local[1] - pan[1]) / z).floor() as i32;
    (x.clamp(0, w as i32 - 1), y.clamp(0, h as i32 - 1))
}

/// A `w` x `h` image's on-screen size at integer `zoom`.
pub fn image_pixel_size(w: usize, h: usize, zoom: usize) -> (f32, f32) {
    let z = zoom.max(1) as f32;
    (w as f32 * z, h as f32 * z)
}

/// Step the integer zoom by `delta`, clamped to the ladder ends.
pub fn zoom_by(zoom: usize, delta: isize) -> usize {
    (zoom as isize + delta).clamp(MIN_ZOOM as isize, MAX_ZOOM as isize) as usize
}

/// New pan such that the image pixel under canvas-local `cursor` stays under
/// it after switching from `zoom` to `new_zoom` (`screen = pan + image *
/// zoom`) -- the cursor-anchored wheel zoom the map and world panels both
/// give their canvases.
pub fn zoom_at(pan: [f32; 2], zoom: usize, cursor: [f32; 2], new_zoom: usize) -> [f32; 2] {
    let (z, nz) = (zoom.max(1) as f32, new_zoom.max(1) as f32);
    [
        cursor[0] - (cursor[0] - pan[0]) / z * nz,
        cursor[1] - (cursor[1] - pan[1]) / z * nz,
    ]
}

/// The region a commit will actually quantize and slice: the crop when one
/// was drawn, else the whole `w` x `h` image.
///
/// `WizardState::sprite_full_rect` computes exactly this and is what the
/// panel calls; this is the same rule restated for code paths that hold a
/// bare `Option<Region>` (the paint scene, the readouts) rather than the
/// whole wizard. Kept in one place so the canvas outline, the grid overlay
/// and the tile-count readout can never disagree with what `quantize_region`
/// is handed.
pub fn effective_region(region: Option<Region>, w: usize, h: usize) -> Region {
    region.unwrap_or(Region { x: 0, y: 0, w, h })
}

/// Interior tile-divider offsets INSIDE `crop`, in image px relative to the
/// crop's own top-left.
///
/// **Pinned to [`TILE_PX`], with no cell-size parameter, deliberately.**
/// `slice_to_tiles` is hard-wired to `TILE_PX` (`worldlib`'s
/// `sprites::import::slice_to_tiles`), so a grid drawn at any other step
/// would draw lines where no cut happens -- which is exactly the mis-port
/// fix round 1 caught: ggo-ide's Cell W/H inputs and their live overlay were
/// inside `<Show when={mode() === 'metasprite'}>` and never existed in
/// Tileset mode, where the cut is always one tile.
///
/// Relative to the CROP, not the image: the slice starts at the crop's own
/// origin, so a grid anchored anywhere else would be off by the crop offset.
/// Straight through worldlib's [`grid_lines`]; this wrapper exists to name
/// the anchoring rule and to pin the step.
pub fn crop_grid_lines(crop: Region) -> (Vec<usize>, Vec<usize>) {
    grid_lines(crop.w, crop.h, TILE_PX, TILE_PX)
}

/// How many WHOLE tiles fit inside `crop` -- `uniform_rects` only emits
/// fully in-bounds cells, so this is the count that does NOT need padding.
///
/// Compared against [`tiles_for`]'s count (which rounds UP) it is the
/// "your right/bottom edge will be zero-padded" signal, in the same unit at
/// last: both are tiles.
pub fn whole_tiles(crop: Region) -> usize {
    uniform_rects(crop.w, crop.h, TILE_PX, TILE_PX).len()
}

/// Will `crop` produce zero-padded edge tiles? True when either side is not a
/// whole number of [`TILE_PX`] tiles.
///
/// Stated as the modulo rather than as `whole_tiles != tiles_for`, even
/// though the two now agree: the comparison form is what silently went wrong
/// when the two sides were measured in different units (cells vs tiles), and
/// `whole_tiles_and_is_ragged_agree` pins that they say the same thing.
pub fn is_ragged(crop: Region) -> bool {
    !crop.w.is_multiple_of(TILE_PX) || !crop.h.is_multiple_of(TILE_PX)
}

/// The `(cols, rows, count)` of `TILE_PX` tiles a commit of `crop` will
/// produce -- `slice_to_tiles`'s own `div_ceil` layout, restated so the UI
/// can show the count without running the slice.
///
/// Pinned against `slice_to_tiles` itself by
/// `tiles_for_agrees_with_slice_to_tiles`, not against a restatement of its
/// formula.
pub fn tiles_for(crop: Region) -> (usize, usize, usize) {
    let cols = crop.w.div_ceil(TILE_PX);
    let rows = crop.h.div_ceil(TILE_PX);
    (cols, rows, cols * rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::import::slice_to_tiles;

    /// At 1x with no pan, one source pixel is one CSS px, and a point past
    /// any edge clamps onto the nearest in-image pixel rather than missing.
    #[test]
    fn image_coord_maps_pixels_at_identity_and_clamps_off_image() {
        let at = |x: f32, y: f32| image_coord([x, y], 1, [0.0, 0.0], 8, 4);
        assert_eq!(at(0.0, 0.0), (0, 0));
        assert_eq!(at(0.9, 0.9), (0, 0));
        assert_eq!(at(1.0, 0.0), (1, 0));
        assert_eq!(at(7.5, 3.5), (7, 3));
        // Past every edge: clamped, never `None`, never wrapped.
        assert_eq!(at(100.0, 100.0), (7, 3));
        assert_eq!(at(-0.5, -40.0), (0, 0));
        assert_eq!(at(-0.5, 2.0), (0, 2), "only the axis that left clamps");
    }

    /// Zoom scales the pixel step and pan shifts the origin; the clamp still
    /// applies in the panned/zoomed frame.
    #[test]
    fn image_coord_accounts_for_zoom_and_pan() {
        let at = |x: f32, y: f32| image_coord([x, y], 4, [100.0, 50.0], 8, 4);
        assert_eq!(at(100.0, 50.0), (0, 0));
        assert_eq!(at(103.9, 50.0), (0, 0));
        assert_eq!(at(104.0, 50.0), (1, 0));
        assert_eq!(at(100.0, 58.0), (0, 2));
        assert_eq!(at(99.0, 50.0), (0, 0), "left of the panned origin clamps");
        assert_eq!(at(1000.0, 50.0), (7, 0));
    }

    /// A zero zoom would divide by zero, and a zero-sized image has no
    /// pixel to name.
    #[test]
    fn image_coord_and_size_survive_degenerate_inputs() {
        assert_eq!(image_coord([5.0, 5.0], 0, [0.0, 0.0], 4, 4), (4 - 1, 4 - 1));
        assert_eq!(image_coord([5.0, 5.0], 2, [0.0, 0.0], 0, 0), (0, 0));
        assert_eq!(image_pixel_size(8, 4, 0), (8.0, 4.0));
        assert_eq!(image_pixel_size(8, 4, 3), (24.0, 12.0));
    }

    /// The cursor-anchored zoom invariant: the pixel under the cursor before
    /// the zoom is the pixel under it after.
    #[test]
    fn zoom_at_keeps_the_pixel_under_the_cursor_fixed() {
        let pan = [17.0, -9.0];
        let cursor = [123.0, 77.0];
        for (zoom, new_zoom) in [(1usize, 4usize), (4, 1), (2, 3), (16, 2)] {
            let before = image_coord(cursor, zoom, pan, 512, 512);
            let new_pan = zoom_at(pan, zoom, cursor, new_zoom);
            let after = image_coord(cursor, new_zoom, new_pan, 512, 512);
            assert_eq!(before, after, "{zoom}x -> {new_zoom}x");
        }
        assert_eq!(zoom_at(pan, 3, cursor, 3), pan, "a no-op zoom is no pan");
    }

    #[test]
    fn zoom_by_clamps_at_both_ends() {
        assert_eq!(zoom_by(DEFAULT_ZOOM, 1), DEFAULT_ZOOM + 1);
        assert_eq!(zoom_by(MAX_ZOOM, 1), MAX_ZOOM);
        assert_eq!(zoom_by(MIN_ZOOM, -1), MIN_ZOOM);
        assert_eq!(zoom_by(MIN_ZOOM, -100), MIN_ZOOM);
        assert_eq!(zoom_by(4, 100), MAX_ZOOM);
    }

    /// No crop means the whole image; a crop is passed through untouched.
    #[test]
    fn effective_region_falls_back_to_the_whole_image() {
        assert_eq!(
            effective_region(None, 40, 24),
            Region {
                x: 0,
                y: 0,
                w: 40,
                h: 24
            }
        );
        let crop = Region {
            x: 8,
            y: 8,
            w: 16,
            h: 16,
        };
        assert_eq!(effective_region(Some(crop), 40, 24), crop);
    }

    /// The overlay is anchored on the CROP, so a crop at a non-zero origin
    /// gets the same line offsets a crop at the origin would -- the caller
    /// adds `crop.x`/`crop.y` when painting. Getting this wrong draws lines
    /// where `slice_to_tiles` makes no cut.
    #[test]
    fn crop_grid_lines_are_relative_to_the_crop_origin() {
        let at_origin = Region {
            x: 0,
            y: 0,
            w: 48,
            h: 32,
        };
        let offset = Region {
            x: 7,
            y: 13,
            w: 48,
            h: 32,
        };
        assert_eq!(crop_grid_lines(at_origin), (vec![16, 32], vec![16]));
        assert_eq!(crop_grid_lines(offset), crop_grid_lines(at_origin));
    }

    /// **Fix round 1, BLOCKING 1.** Every divider the overlay draws must sit
    /// on a `TILE_PX` boundary, because that is the ONLY step
    /// `slice_to_tiles` cuts at. The panel used to take a user-editable cell
    /// size here, which drew lines at (say) 8px on a 32x16 crop -- three
    /// vertical dividers where the slicer makes one cut. There is no cell
    /// parameter to get wrong any more; this pins that it stays that way.
    #[test]
    fn the_overlay_step_is_always_the_tile_size() {
        for (w, h) in [(48usize, 32usize), (32, 16), (20, 40), (8, 8), (33, 17)] {
            let crop = Region { x: 3, y: 5, w, h };
            let (xs, ys) = crop_grid_lines(crop);
            assert!(
                xs.iter()
                    .chain(ys.iter())
                    .all(|n| n.is_multiple_of(TILE_PX)),
                "{w}x{h}: every divider must land on a tile boundary, got {xs:?}/{ys:?}"
            );
            // ...and there is exactly one divider per interior tile boundary.
            assert_eq!(xs.len(), w.saturating_sub(1) / TILE_PX, "{w}x{h} verticals");
            assert_eq!(
                ys.len(),
                h.saturating_sub(1) / TILE_PX,
                "{w}x{h} horizontals"
            );
        }
    }

    /// A crop that is an exact multiple of the tile reports the same number
    /// of whole tiles as tiles written; a ragged one reports FEWER, which is
    /// the zero-padding warning the footer surfaces. Both counts are TILES --
    /// the unit mismatch between them is what fix round 1 removed.
    #[test]
    fn whole_tiles_and_is_ragged_agree() {
        let exact = Region {
            x: 0,
            y: 0,
            w: 32,
            h: 32,
        };
        assert_eq!(whole_tiles(exact), 4);
        assert_eq!(tiles_for(exact).2, 4);
        assert!(!is_ragged(exact));

        let ragged = Region {
            x: 0,
            y: 0,
            w: 20,
            h: 16,
        };
        assert_eq!(whole_tiles(ragged), 1);
        assert_eq!(tiles_for(ragged).2, 2, "the ragged edge still costs a tile");
        assert!(is_ragged(ragged));

        // The two formulations must never disagree.
        for (w, h) in [(16usize, 16usize), (32, 16), (1, 1), (33, 17), (48, 32)] {
            let crop = Region { x: 0, y: 0, w, h };
            assert_eq!(
                is_ragged(crop),
                whole_tiles(crop) != tiles_for(crop).2,
                "{w}x{h}"
            );
        }
    }

    /// The readout is pinned against `slice_to_tiles` ITSELF, so a drift in
    /// worldlib's padding rule fails here instead of silently mis-reporting.
    #[test]
    fn tiles_for_agrees_with_slice_to_tiles() {
        for (w, h) in [(16, 16), (20, 16), (48, 32), (1, 1), (33, 17)] {
            let crop = Region { x: 0, y: 0, w, h };
            let (cols, rows, count) = tiles_for(crop);
            let (_, sliced) = slice_to_tiles(&vec![0u8; w * h], w, h);
            assert_eq!(count, sliced, "{w}x{h}");
            assert_eq!(cols * rows, sliced, "{w}x{h}");
        }
    }
}
