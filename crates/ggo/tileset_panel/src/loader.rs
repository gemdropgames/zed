//! Off-thread tileset loading + the pure grid geometry the editor needs:
//! open one `.til` (worldlib resolves its companion `.pal` itself) and
//! compose the whole tile sheet into a single BGRA [`RenderImage`] -- the
//! tileset-panel analog of `ggo_sprite_panel::loader` (same one-shot
//! background pass; the panel guards staleness with a load-generation
//! counter). The panel seeds its `TilesetDocStore` from the loaded
//! indices and calls [`compose_grid`] again after every doc op.

use std::path::Path;
use std::sync::Arc;

use ggo_common::to_render_image;
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::palette565::{Pal, indices_to_rgba};
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
    /// The unpacked per-pixel palette indices -- the editor seeds its
    /// `TilesetDocStore` with these.
    pub indices: Vec<u8>,
    pub tile_count: usize,
    /// Columns the grid was composed at ([`grid_cols`]).
    pub cols: usize,
    pub palette: Pal,
    /// True when the `.til` had no decodable companion `.pal` and worldlib
    /// substituted its 16-gray fallback -- worth saying out loud in the
    /// UI, since the colors on screen then aren't the asset's own.
    pub missing_pal: bool,
    /// The `.pal` rel worldlib derived from the `.til` rel (shown next to
    /// the source path; saves write it back alongside the `.til`).
    pub pal_path: String,
    /// The whole tile sheet as ONE composed BGRA image. Rebuilt by the
    /// panel via [`compose_grid`] after every doc op.
    pub grid: Arc<RenderImage>,
    /// `grid`'s pixel size, kept alongside it so the zoom math doesn't
    /// have to go back through `RenderImage::size`.
    pub grid_size: (u32, u32),
}

/// Compose the whole sheet into one BGRA image -- the load-time pass and
/// the after-every-op rebuild share this. `None` only for dimensions
/// gpui can't build an image from (never for a well-formed doc).
pub fn compose_grid(
    indices: &[u8],
    tile_count: usize,
    cols: usize,
    palette: &Pal,
) -> Option<Arc<RenderImage>> {
    let (buf, ..) = compose_tile_grid(indices, tile_count, cols);
    let rgba = indices_to_rgba(&buf, palette);
    let (w, h) = grid_pixel_size(tile_count, cols);
    to_render_image(&rgba, w, h)
}

/// How many columns to lay a `tile_count`-tile sheet out in.
///
/// This panel has neither of worldlib's two better sources -- no db-backed
/// `{cols}` setting and no legacy `.meta.json` sidecar reader -- so both
/// chain steps are `None` and [`resolve_cols`]'s fallback decides. The
/// fallback is clamped to `tile_count` so a sheet SHORTER than one full row
/// renders exactly its own tiles instead of a row padded with blanks.
///
/// The db-backed setting is not merely unread: the `tileset_cols:*` rows in
/// `~/.ggo/ggo_ide.db` were written and read by ggo-ide, which was deleted
/// in ggo `281fd557` (F5.5). Nothing writes them and nothing reads them now,
/// so any width a user once set is unreachable. Wiring this step up is a
/// settings read against a db `ggo_charts_panel`/`ggo_emu_panel` already
/// open; see `docs/ggo/MIGRATION.md`'s closing section.
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

/// Open `rel` (a project-relative `.til`) and compose its whole sheet.
pub fn load_tileset(project_dir: &Path, rel: &str) -> Result<LoadedTileset, String> {
    let opened = io::open_tileset(project_dir, rel).map_err(|e| e.to_string())?;
    let cols = grid_cols(opened.tile_count);
    // Size comes from [`grid_pixel_size`] rather than `compose_tile_grid`'s
    // own returned `(w, h)` so the panel's geometry fn is the ONE definition
    // of the grid's dimensions (the two agreeing is pinned by
    // `grid_pixel_size_matches_the_composed_buffer`; a drift would fail here
    // as a dimension/length mismatch, not as a silently skewed image).
    let grid = compose_grid(&opened.indices, opened.tile_count, cols, &opened.palette)
        .ok_or_else(|| "composed tile grid had invalid dimensions".to_string())?;
    let grid_size = grid_pixel_size(opened.tile_count, cols);
    Ok(LoadedTileset {
        indices: opened.indices,
        tile_count: opened.tile_count,
        cols,
        palette: opened.palette,
        missing_pal: opened.missing_pal,
        pal_path: opened.pal_path,
        grid,
        grid_size,
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

}
