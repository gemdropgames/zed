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
//! applied `MapOp` has to be composed out of the in-memory
//! `MapDocStore` instead. It is not a second composer in the sense the task
//! forbids -- the actual per-cell work is worldlib's
//! `map_doc::compose_map_indices` in both paths, and the only thing
//! [`compose_live_rgba`] adds is the indexed->RGBA palette expansion the
//! shared function performs inline on its own output. The two agreeing is
//! not left to inspection: `live_compose_matches_the_shared_disk_compose`
//! runs both over the same fixture and compares every byte, so a drift in
//! either fails a test (the mechanically-verified-mirror discipline this
//! repo applies to cross-layer constants).
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
use ggo_worldlib::sprites::palette565::{PAL_SLOTS, Pal, TRANSPARENT_SLOT};
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

/// Expand an indexed pixel buffer through `palette` into straight-alpha
/// RGBA8, index [`TRANSPARENT_SLOT`] fully transparent -- the exact
/// per-pixel rule `io::compose_map_rgba` and
/// `sprites::preview::compose_frame_rgba` apply.
pub fn indices_to_rgba(indices: &[u8], palette: &Pal) -> Vec<u8> {
    let mut out = Vec::with_capacity(indices.len() * 4);
    for &idx in indices {
        let slot = idx as usize % PAL_SLOTS;
        let (r, g, b) = ggo_asset_formats::pixel::rgb888(palette[slot]);
        let a = if slot == TRANSPARENT_SLOT { 0 } else { 255 };
        out.extend_from_slice(&[r, g, b, a]);
    }
    out
}

/// Compose the LIVE document (not the file on disk) into straight-alpha
/// RGBA8 plus its pixel size. See the module doc for why this exists
/// alongside the shared disk compose, and for the test that pins them
/// equal.
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

/// Resolve `til_rel` against `root` into the strip's pixel data.
pub fn load_tileset(root: &Path, til_rel: &str) -> Result<Tileset, String> {
    if til_rel.is_empty() {
        return Err("no tileset bound".to_string());
    }
    let opened = io::open_tileset(root, til_rel).map_err(|e| e.to_string())?;
    Ok(Tileset {
        cols: grid_cols(opened.tile_count),
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
pub fn load_map(root: &Path, rel: &str) -> Result<LoadedMap, String> {
    let data = io::open_map(root, rel).map_err(|e| e.to_string())?;
    let (tileset, tileset_error) = match load_tileset(root, &data.til_path) {
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

    #[test]
    fn indices_to_rgba_makes_slot_zero_transparent() {
        let mut palette = [0u16; PAL_SLOTS];
        palette[0] = 0xFFFF; // an opaque-looking white the rule must ignore
        palette[1] = 0xF800;
        let rgba = indices_to_rgba(&[0, 1], &palette);
        assert_eq!(rgba[..4], [255, 255, 255, 0]);
        assert_eq!(rgba[4..], [255, 0, 0, 255]);
    }

    /// **The drift check** (see the module doc): the live in-memory
    /// compose and worldlib's shared disk compose must produce
    /// byte-identical RGBA for the same, unmodified document. If either
    /// side's palette expansion or transparency rule ever moves, this
    /// fails instead of the two panels quietly drawing the same map
    /// differently.
    #[test]
    fn live_compose_matches_the_shared_disk_compose() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tileset(root, "world", 4);
        // A map exercising every branch that could diverge: blank cells,
        // an in-range tile, an OUT-of-range tile (drawn transparent), and
        // both flips.
        let cells = vec![
            CELL_BLANK,
            pack_cell(1, 0, false, false),
            pack_cell(2, 3, true, true),
            pack_cell(99, 0, false, false),
            pack_cell(3, 0, true, false),
            CELL_BLANK,
        ];
        let state = MapState {
            w: 3,
            h: 2,
            cells,
            til_path: "tiles/world.til".to_string(),
            pal_path: "tiles/world.pal".to_string(),
            dirty: false,
        };
        io::save_map(root, "maps/level.map", &state).unwrap();

        let loaded = load_map(root, "maps/level.map").unwrap();
        let tileset = loaded.tileset.expect("the fixture binds a real tileset");
        let (live, lw, lh) = compose_live_rgba(&state, &tileset);
        let shared = io::compose_map_rgba(root, "maps/level").unwrap();
        assert_eq!((lw, lh), (shared.w, shared.h));
        assert_eq!(
            live,
            shared.rgba.to_vec(),
            "the live compose must match worldlib's shared disk compose byte for byte"
        );
        assert!(
            loaded.image.is_some(),
            "the initial image comes from shared"
        );
        assert!(loaded.strip.is_some());
        assert_eq!(loaded.tilesets, vec!["tiles/world.til".to_string()]);
    }

    /// An unbound map (`til_path` empty -- what "New Map…" writes) still
    /// loads: no tileset, no images, and a reason to show.
    #[test]
    fn an_unbound_map_loads_without_a_tileset() {
        let dir = tempfile::tempdir().unwrap();
        io::save_new_map(dir.path(), "maps/fresh.map", 4, 4).unwrap();
        let loaded = load_map(dir.path(), "maps/fresh.map").unwrap();
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
        let loaded = load_map(root, "level.map").unwrap();
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
