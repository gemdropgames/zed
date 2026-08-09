//! Off-thread tileset loading + the pure grid geometry the viewer needs:
//! open one `.til` (worldlib resolves its companion `.pal` itself) and
//! compose the whole tile sheet into a single BGRA [`RenderImage`] -- the
//! tileset-panel analog of `ggo_metasprite_panel::loader` (same one-shot
//! background pass; the panel guards staleness with a load-generation
//! counter).
//!
//! Read-only: nothing here writes, and there is no `TilesetDocStore` --
//! `open_tileset`'s snapshot is composed once and then only READ, so the
//! panel never needs the store's undo/dirty machinery.

use std::path::Path;
use std::sync::Arc;

use ggo_common::to_render_image;
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::palette565::{PAL_SLOTS, Pal, TRANSPARENT_SLOT};
use ggo_worldlib::sprites::tileset_doc::{
    TILE_PX, compose_tile_grid, resolve_cols, tile_grid_layout,
};
use gpui::RenderImage;

/// Column count for the grid overview when nothing better is known --
/// the same fallback ggo-ide's `pages::assets::GRID_COLS_FALLBACK` uses,
/// so a tileset laid out there and here reads identically.
pub const GRID_COLS_FALLBACK: usize = 8;

/// Everything the panel needs to enter its Ready state, assembled
/// entirely off the UI thread.
pub struct LoadedTileset {
    pub tile_count: usize,
    /// Columns the grid was composed at ([`grid_cols`]).
    pub cols: usize,
    pub palette: Pal,
    /// True when the `.til` had no decodable companion `.pal` and worldlib
    /// substituted its 16-gray fallback -- worth saying out loud in the
    /// UI, since the colors on screen then aren't the asset's own.
    pub missing_pal: bool,
    /// The `.pal` rel worldlib derived from the `.til` rel (shown next to
    /// the source path; this panel never writes it).
    pub pal_path: String,
    /// The whole tile sheet as ONE composed BGRA image. Built once at
    /// load: nothing in this panel mutates the tileset, so there is no
    /// invalidation point at all (contrast `ggo_metasprite_panel`'s
    /// rebuild-after-every-op).
    pub grid: Arc<RenderImage>,
    /// `grid`'s pixel size, kept alongside it so the zoom math doesn't
    /// have to go back through `RenderImage::size`.
    pub grid_size: (u32, u32),
}

/// How many columns to lay a `tile_count`-tile sheet out in.
///
/// This panel has neither of worldlib's two better sources -- no db-backed
/// `{cols}` setting (that lives in ggo-ide's own store) and no legacy
/// `.meta.json` sidecar reader -- so both chain steps are `None` and
/// [`resolve_cols`]'s fallback decides. The fallback is clamped to
/// `tile_count` so a sheet SHORTER than one full row renders exactly its
/// own tiles instead of a row padded with blanks.
pub fn grid_cols(tile_count: usize) -> usize {
    let fallback = GRID_COLS_FALLBACK.min(tile_count.max(1));
    resolve_cols(None, None, tile_count, fallback)
}

/// The composed grid's pixel size for `tile_count` tiles at `cols` --
/// [`compose_tile_grid`]'s own `(w, h)` rule (including its `rows.max(1)`
/// floor, so an empty tileset is one blank row rather than a zero-height
/// image gpui can't build).
pub fn grid_pixel_size(tile_count: usize, cols: usize) -> (u32, u32) {
    let (_, rows) = tile_grid_layout(tile_count, cols);
    ((cols * TILE_PX) as u32, (rows.max(1) * TILE_PX) as u32)
}

/// One palette slot as straight-alpha RGBA8, for the swatch row.
/// Slot 0 is the locked transparent entry (PPU contract §1), so it reads
/// back alpha-0 no matter what color the `.pal` stored in it -- the same
/// rule `sprites::preview::compose_frame_rgba` applies to pixels.
pub fn swatch_rgba(palette: &Pal, slot: usize) -> [u8; 4] {
    let (r, g, b) = ggo_asset_formats::pixel::rgb888(palette[slot]);
    let a = if slot == TRANSPARENT_SLOT { 0 } else { 255 };
    [r, g, b, a]
}

/// Expand an indexed pixel buffer through `palette` into straight-alpha
/// RGBA8. worldlib composes SPRITE frames (`preview::compose_frame_rgba`)
/// but has no indexed-buffer->RGBA entry point for a bare tile sheet, so
/// this applies that function's exact rules -- 565->888 per slot, index 0
/// fully transparent -- to `compose_tile_grid`'s output.
pub fn indices_to_rgba(indices: &[u8], palette: &Pal) -> Vec<u8> {
    let mut out = Vec::with_capacity(indices.len() * 4);
    for &idx in indices {
        out.extend_from_slice(&swatch_rgba(palette, (idx as usize) % PAL_SLOTS));
    }
    out
}

/// Open `rel` (a project-relative `.til`) and compose its whole sheet.
pub fn load_tileset(project_dir: &Path, rel: &str) -> Result<LoadedTileset, String> {
    let opened = io::open_tileset(project_dir, rel).map_err(|e| e.to_string())?;
    let cols = grid_cols(opened.tile_count);
    let (buf, ..) = compose_tile_grid(&opened.indices, opened.tile_count, cols);
    let rgba = indices_to_rgba(&buf, &opened.palette);
    // Size comes from [`grid_pixel_size`] rather than `compose_tile_grid`'s
    // own returned `(w, h)` so the panel's geometry fn is the ONE definition
    // of the grid's dimensions (the two agreeing is pinned by
    // `grid_pixel_size_matches_the_composed_buffer`; a drift would fail here
    // as a dimension/length mismatch, not as a silently skewed image).
    let (w, h) = grid_pixel_size(opened.tile_count, cols);
    let grid = to_render_image(&rgba, w, h)
        .ok_or_else(|| "composed tile grid had invalid dimensions".to_string())?;
    Ok(LoadedTileset {
        tile_count: opened.tile_count,
        cols,
        palette: opened.palette,
        missing_pal: opened.missing_pal,
        pal_path: opened.pal_path,
        grid,
        grid_size: (w, h),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback clamp: a full-or-larger sheet gets the standard 8
    /// columns, a short one gets exactly its own tile count (no padded
    /// trailing blanks), and an empty sheet still gets a legal 1.
    #[test]
    fn grid_cols_clamps_the_fallback_to_short_tilesets() {
        assert_eq!(grid_cols(64), GRID_COLS_FALLBACK);
        assert_eq!(grid_cols(8), 8);
        assert_eq!(grid_cols(3), 3);
        assert_eq!(grid_cols(1), 1);
        assert_eq!(grid_cols(0), 1);
    }

    /// Grid dimensions follow `compose_tile_grid`'s layout: full rows
    /// wide, a partial last row still counts as a whole row, and an empty
    /// sheet floors at one row.
    #[test]
    fn grid_pixel_size_matches_the_composed_buffer() {
        for (tile_count, cols) in [(0usize, 1usize), (1, 1), (3, 3), (9, 8), (16, 8)] {
            let (buf, w, h) =
                compose_tile_grid(&vec![0u8; tile_count * TILE_PX * TILE_PX], tile_count, cols);
            assert_eq!(
                grid_pixel_size(tile_count, cols),
                (w as u32, h as u32),
                "{tile_count} tiles at {cols} cols"
            );
            assert_eq!(buf.len(), w * h);
        }
        // 9 tiles at 8 cols is two rows, the second one only 1/8 filled.
        assert_eq!(
            grid_pixel_size(9, 8),
            (8 * TILE_PX as u32, 2 * TILE_PX as u32)
        );
    }

    /// Index 0 is transparent regardless of the color stored in slot 0;
    /// every other index is opaque with its 565->888 color.
    #[test]
    fn indices_to_rgba_makes_slot_zero_transparent() {
        let mut palette = [0u16; PAL_SLOTS];
        palette[0] = 0xFFFF; // an opaque-looking white the rule must ignore
        palette[1] = 0xF800; // pure 565 red
        let rgba = indices_to_rgba(&[0, 1], &palette);
        assert_eq!(rgba[..4], [255, 255, 255, 0]);
        assert_eq!(rgba[4..], [255, 0, 0, 255]);
    }
}
