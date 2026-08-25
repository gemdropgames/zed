//! Off-thread map loading and the two composes the panel draws with.
//!
//! **The initial canvas image comes from the SHARED compose**
//! (`ggo_worldlib::sprites::io::compose_map_rgba`, moved into worldlib by
//! F5.1 Task M1): the same function `ggo_world_panel` draws `[[background]]`
//! maps with, so a map looks identical in both panels by construction.
//!
//! **The live one is [`compose_live_rgba`]**, and it has to exist: the
//! shared compose takes a `(project_dir, stem)` and re-reads both files off
//! disk, so it can only ever show the SAVED map. This panel edits, so every
//! applied `MapOp` has to be composed out of the in-memory `MapDocStore`
//! instead.
//!
//! It is not a second COMPOSER, though, and (since fix round 1) not a
//! second expansion either: both halves are worldlib's --
//! `map_doc::compose_map_indices` for the per-cell work and
//! `palette565::indices_to_rgba` for the indexed->RGBA8888 step. That
//! second one used to be a local copy here, guarded by a fork-side
//! byte-equality test against `compose_map_rgba`; the guard was the wrong
//! shape (it could not fail for a ggo developer editing worldlib) and the
//! copy is gone -- ggo PR #80 moved the rule into `palette565` and
//! collapsed every call site, this one included. There is nothing left for
//! a drift check to compare.
//!
//! Loading a map resolves its bound tileset too -- the panel needs that
//! tileset's pixels for the strip and for [`compose_live_rgba`] anyway, and
//! an unbound or unreadable binding is a state the panel shows rather than
//! an error that fails the open (a map created by "New Map…" starts
//! unbound by design; see `ggo_map_panel`'s `new_map` doc).

use std::path::Path;
use std::sync::Arc;

use ggo_asset_formats::MapData;
use ggo_common::to_render_image;
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::map_doc::{MapState, compose_map_indices};
use ggo_worldlib::sprites::palette565::{Pal, indices_to_rgba};
use ggo_worldlib::sprites::tileset_meta::{TilesetMeta, load_tileset_meta};
use ggo_worldlib::sprites::tileset_doc::{compose_tile_grid, resolve_cols};
use gpui::RenderImage;

/// Column count for the strip when nothing better is known -- ggo-ide's
/// `pages::assets::GRID_COLS_FALLBACK`, the same value
/// `ggo_tileset_panel::loader` falls back to, so one tileset lays out
/// identically in the sheet viewer and in this panel's picker.
pub const GRID_COLS_FALLBACK: usize = 8;

/// The bound tileset's resolved pixel data -- ggo-ide's
/// `pages::assets::map::TilesetPanelData` plus the strip's layout.
pub struct Tileset {
    pub indices: Vec<u8>,
    pub palette: Pal,
    pub tile_count: usize,
    /// Columns the strip is laid out (and stamp-indexed) at.
    pub cols: usize,
    /// True when the `.til` had no readable companion `.pal` and worldlib
    /// substituted its 16-gray fallback -- the colors on screen then
    /// aren't the asset's own, which is worth saying out loud.
    pub missing_pal: bool,
    /// The `.pal` rel worldlib derived from the `.til` rel. This is what
    /// `MapOp::BindTileset` records, so binding never has to re-derive the
    /// pairing rule itself (which is private to worldlib).
    pub pal_path: String,
}

impl Tileset {
    /// Rows the strip lays out in -- floored at 1 so an empty tileset is
    /// one blank row rather than a zero-height image gpui can't build.
    pub fn rows(&self) -> usize {
        self.tile_count.div_ceil(self.cols.max(1)).max(1)
    }
}

/// Everything the panel needs to enter its Ready state, assembled entirely
/// off the UI thread.
pub struct LoadedMap {
    pub data: MapData,
    pub tileset: Option<Tileset>,
    /// The bound tileset's editor sidecar (autotile terrains live there),
    /// and the worktree-relative path it is keyed by -- `None` when the
    /// asset root is not inside the worktree.
    pub tileset_meta: TilesetMeta,
    pub til_meta_rel: Option<String>,
    /// Why there is no [`Self::tileset`]: an empty binding, or the bound
    /// `.til` failing to open. `None` when one loaded fine.
    pub tileset_error: Option<String>,
    /// The bound tileset's whole sheet, composed once (invalidated only by
    /// a rebind -- the map panel never edits tiles).
    pub strip: Option<Arc<RenderImage>>,
    /// The composed map, from the SHARED disk compose (see the module doc).
    pub image: Option<Arc<RenderImage>>,
    /// Every `.til` under the asset root, for the bind picker.
    pub tilesets: Vec<String>,
}

/// How many columns to lay a `tile_count`-tile sheet out in -- the rule
/// `ggo_tileset_panel::loader::grid_cols` applies, so the two panels agree
/// on a tileset's layout (and therefore on which pool index a given strip
/// cell is). Clamped to `tile_count` so a sheet shorter than one full row
/// renders exactly its own tiles instead of a row padded with blanks.
pub fn grid_cols(tile_count: usize) -> usize {
    let fallback = GRID_COLS_FALLBACK.min(tile_count.max(1));
    resolve_cols(None, None, tile_count, fallback)
}

/// Compose the LIVE document (not the file on disk) into straight-alpha
/// RGBA8 plus its pixel size -- worldlib's `compose_map_indices` followed
/// by worldlib's `indices_to_rgba`, i.e. exactly what
/// `io::compose_map_rgba` does to the SAVED bytes. See the module doc for
/// why the live twin exists.
pub fn compose_live_rgba(state: &MapState, tileset: &Tileset) -> (Vec<u8>, u32, u32) {
    let (indices, w, h) = compose_map_indices(
        &state.cells,
        state.w,
        state.h,
        &tileset.indices,
        tileset.tile_count,
    );
    (
        indices_to_rgba(&indices, &tileset.palette),
        w as u32,
        h as u32,
    )
}

/// The live compose as a gpui image, or `None` when the result has no
/// pixels (a zero-dimension map can't be built into a `RenderImage`).
pub fn compose_live_image(state: &MapState, tileset: &Tileset) -> Option<Arc<RenderImage>> {
    let (rgba, w, h) = compose_live_rgba(state, tileset);
    to_render_image(&rgba, w, h)
}

/// Compose the bound tileset's whole sheet for the picker strip.
pub fn compose_strip(tileset: &Tileset) -> Option<Arc<RenderImage>> {
    let (buf, w, h) = compose_tile_grid(&tileset.indices, tileset.tile_count, tileset.cols);
    let rgba = indices_to_rgba(&buf, &tileset.palette);
    to_render_image(&rgba, w as u32, h as u32)
}

/// The worktree-relative path of `til_rel` (an asset-root-relative `.til`),
/// which is what the tileset editor keys its sidecar by. `None` when the
/// asset root is not inside `project_root`.
pub fn tileset_meta_rel(asset_root: &Path, project_root: &Path, til_rel: &str) -> Option<String> {
    let under = asset_root.strip_prefix(project_root).ok()?;
    let under = under.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    Some(if under.is_empty() {
        til_rel.to_string()
    } else {
        format!("{under}/{til_rel}")
    })
}

/// The bound tileset's sidecar (defaults when there is none).
pub fn tileset_meta(asset_root: &Path, project_root: &Path, til_rel: &str) -> TilesetMeta {
    match tileset_meta_rel(asset_root, project_root, til_rel) {
        Some(rel) => load_tileset_meta(project_root, &rel),
        None => TilesetMeta::default(),
    }
}

/// Resolve `til_rel` against `root` into the strip's pixel data, laid out
/// at the sidecar's `cols` when it has one that fits this sheet.
pub fn load_tileset(root: &Path, til_rel: &str, cols_hint: Option<usize>) -> Result<Tileset, String> {
    if til_rel.is_empty() {
        return Err("no tileset bound".to_string());
    }
    let opened = io::open_tileset(root, til_rel).map_err(|e| e.to_string())?;
    Ok(Tileset {
        cols: resolve_cols(cols_hint, None, opened.tile_count, grid_cols(opened.tile_count)),
        indices: opened.indices,
        palette: opened.palette,
        tile_count: opened.tile_count,
        missing_pal: opened.missing_pal,
        pal_path: opened.pal_path,
    })
}

/// The asset-root-relative stem `io::compose_map_rgba` keys on: `rel`
/// minus its (case-insensitive) `.map` suffix. Every OTHER worldlib map
/// entry point takes the full rel; only the shared compose takes a stem
/// (it is the world format's own asset-stem frame), so the conversion
/// lives here rather than in the panel.
pub fn map_stem(rel: &str) -> &str {
    const MAP_EXT: &str = ".map";
    if rel.len() >= MAP_EXT.len() && rel[rel.len() - MAP_EXT.len()..].eq_ignore_ascii_case(MAP_EXT)
    {
        &rel[..rel.len() - MAP_EXT.len()]
    } else {
        rel
    }
}

/// Open the asset-root-relative `.map` at `rel` under `root`, resolve its
/// bound tileset, and compose both surfaces.
pub fn load_map(root: &Path, rel: &str, project_root: &Path) -> Result<LoadedMap, String> {
    let data = io::open_map(root, rel).map_err(|e| e.to_string())?;
    let tileset_meta = tileset_meta(root, project_root, &data.til_path);
    let til_meta_rel = tileset_meta_rel(root, project_root, &data.til_path);
    let (tileset, tileset_error) = match load_tileset(root, &data.til_path, tileset_meta.cols) {
        Ok(tileset) => (Some(tileset), None),
        Err(e) => (None, Some(e)),
    };
    let strip = tileset.as_ref().and_then(compose_strip);
    // The SHARED compose (task M1) for the initial image -- see the module
    // doc. It re-reads both files, which is exactly right here: nothing has
    // been edited yet, so disk IS the document.
    let image = tileset.as_ref().and_then(|_| {
        let composed = io::compose_map_rgba(root, map_stem(rel)).ok()?;
        to_render_image(&composed.rgba, composed.w, composed.h)
    });
    Ok(LoadedMap {
        data,
        tileset,
        tileset_meta,
        til_meta_rel,
        tileset_error,
        strip,
        image,
        tilesets: io::list_tilesets(root),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::map_doc::{CELL_BLANK, MapDocStore, pack_cell};
    use ggo_worldlib::sprites::palette565::PAL_SLOTS;
    use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;

    fn write_tileset(root: &Path, stem: &str, tiles: usize) {
        let mut indices = vec![0u8; tiles * TILE_PIXELS];
        for (t, chunk) in indices.chunks_exact_mut(TILE_PIXELS).enumerate() {
            chunk.fill((t % PAL_SLOTS) as u8);
        }
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800; // pure 565 red
        palette[2] = 0x07E0; // green
        io::save_tileset(
            root,
            &format!("tiles/{stem}.til"),
            &indices,
            tiles,
            &palette,
        )
        .unwrap();
    }

    #[test]
    fn map_stem_strips_the_extension_case_insensitively() {
        assert_eq!(map_stem("maps/level1.map"), "maps/level1");
        assert_eq!(map_stem("level1.MAP"), "level1");
        assert_eq!(map_stem("noext"), "noext");
    }

    #[test]
    fn grid_cols_clamps_the_fallback_to_short_tilesets() {
        assert_eq!(grid_cols(64), GRID_COLS_FALLBACK);
        assert_eq!(grid_cols(3), 3);
        assert_eq!(grid_cols(0), 1);
    }

    /// An unbound map (`til_path` empty -- what "New Map…" writes) still
    /// loads: no tileset, no images, and a reason to show.
    #[test]
    fn an_unbound_map_loads_without_a_tileset() {
        let dir = tempfile::tempdir().unwrap();
        io::save_new_map(dir.path(), "maps/fresh.map", 4, 4).unwrap();
        let loaded = load_map(dir.path(), "maps/fresh.map", dir.path()).unwrap();
        assert!(loaded.tileset.is_none());
        assert!(loaded.tileset_error.is_some());
        assert!(loaded.image.is_none());
        assert!(loaded.strip.is_none());
        assert_eq!(loaded.data.w, 4);
        assert!(loaded.data.cells.iter().all(|&c| c == CELL_BLANK));
    }

    /// An edit changes the composed pixels -- the property the live
    /// compose exists for (the disk compose cannot see it).
    #[test]
    fn the_live_compose_tracks_an_unsaved_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tileset(root, "world", 4);
        let state = MapState {
            w: 2,
            h: 1,
            cells: vec![CELL_BLANK, CELL_BLANK],
            til_path: "tiles/world.til".to_string(),
            pal_path: "tiles/world.pal".to_string(),
            dirty: false,
        };
        io::save_map(root, "level.map", &state).unwrap();
        let loaded = load_map(root, "level.map", root).unwrap();
        let tileset = loaded.tileset.unwrap();

        let mut store = MapDocStore::new(
            loaded.data.til_path.clone(),
            loaded.data.pal_path.clone(),
            loaded.data.w,
            loaded.data.h,
            loaded.data.cells,
        );
        let before = compose_live_rgba(&store.state(), &tileset).0;
        store.apply(ggo_worldlib::sprites::map_doc::MapOp::RectFill {
            x0: 0,
            y0: 0,
            x1: 1,
            y1: 0,
            cell: pack_cell(1, 0, false, false),
        });
        let after = compose_live_rgba(&store.state(), &tileset).0;
        assert_ne!(before, after, "the live compose must see an unsaved edit");
        assert_eq!(
            io::compose_map_rgba(root, "level").unwrap().rgba.to_vec(),
            before,
            "the disk compose still shows the SAVED document"
        );
    }
}
