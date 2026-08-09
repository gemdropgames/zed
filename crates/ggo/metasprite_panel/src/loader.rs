//! Off-thread sprite loading: open one `.spr` into a [`SpriteState`] plus
//! per-frame composed [`RenderImage`]s -- the metasprite-panel analog of
//! `ggo_world_panel::loader` (same one-shot background pass; the panel
//! guards staleness with a load-generation counter).
//!
//! There is no enumeration here any more: F4 X1 replaced the in-panel
//! picker with file-explorer routing, so the panel is handed one rel path
//! at a time and never lists the project's sprites (worldlib still owns
//! `sprites::io::list_sprites` for callers that do).

use std::path::Path;
use std::sync::Arc;

use ggo_common::to_render_image;
use ggo_worldlib::sprites::cow::{Frame, SpriteState};
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::preview::compose_frame_rgba;
use ggo_worldlib::sprites::sprite_doc::DEFAULT_FRAME_DURATION_MS;
use gpui::RenderImage;

/// Everything the panel needs to enter its Ready state, assembled
/// entirely off the UI thread.
pub struct LoadedSprite {
    pub state: SpriteState,
    /// The `.til`/`.pal` sidecar rels `open_sprite` resolved (bare-sibling
    /// fallback already applied) -- `io::save_sprite` needs them back to
    /// rewrite the same trio the sprite was read from.
    pub til_path: String,
    pub pal_path: String,
    /// One composed BGRA image per frame index (frame-strip thumbnail AND
    /// large preview -- gpui scales the same image to both sizes). Built
    /// here at load; after every doc mutation the panel rebuilds the whole
    /// Vec via [`compose_frames`] (M4's documented invalidation point --
    /// wholesale recompose is O(frames x sprite px) per op, acceptable at
    /// sprite scale: <= 16x16 tiles, undo-capped edit cadence).
    pub frames: Vec<Arc<RenderImage>>,
    /// One composed BGRA image per pool tile index -- the tile-palette
    /// section's thumbnails. Same invalidation story as `frames`: rebuilt
    /// wholesale via [`compose_pool_tiles`] after every doc mutation
    /// (pixel writes, dedup fold-back on save, undo across either).
    pub pool_tiles: Vec<Arc<RenderImage>>,
}

/// Compose every frame of `state` into a BGRA [`RenderImage`] (no LCD
/// filter -- thumbnails and preview alike, matching ggo-ide's frame
/// strip). Shared by the initial load and the panel's after-op refresh.
pub fn compose_frames(state: &SpriteState) -> Result<Vec<Arc<RenderImage>>, String> {
    let mut frames = Vec::with_capacity(state.frames.len());
    for idx in 0..state.frames.len() {
        let rgba = compose_frame_rgba(state, idx, false);
        let (w, h) = rgba.dimensions();
        let image = to_render_image(rgba.as_raw(), w, h)
            .ok_or_else(|| format!("frame {idx}: composed image had invalid dimensions"))?;
        frames.push(image);
    }
    Ok(frames)
}

/// Compose every pool tile of `state` into a BGRA [`RenderImage`] (the
/// tile-palette thumbnails). worldlib has no bare tile->RGBA compose --
/// ggo-ide's `panels::pool_tile_pixels` walks `cow::read_tile_pixel` and
/// re-applies the palette itself -- so instead of duplicating the
/// index->RGBA rules (transparent slot 0, 565->888), this reuses
/// [`compose_frame_rgba`] end to end on a 1x1-tile VIEW of the same pool:
/// one borrowed-pool clone up front, then the sole frame cell is
/// repointed at each tile in turn. O(tiles x TILE_PX^2), same
/// after-every-op cadence as [`compose_frames`].
pub fn compose_pool_tiles(state: &SpriteState) -> Result<Vec<Arc<RenderImage>>, String> {
    let mut view = SpriteState {
        pool: state.pool.clone(),
        tile_count: state.tile_count,
        session_tiles: std::collections::HashSet::new(),
        palette: state.palette,
        frames: vec![Frame {
            map: vec![0],
            duration_ms: DEFAULT_FRAME_DURATION_MS,
        }],
        clips: Vec::new(),
        w_tiles: 1,
        h_tiles: 1,
        pool_shared: state.pool_shared,
    };
    let mut tiles = Vec::with_capacity(state.tile_count);
    for t in 0..state.tile_count {
        view.frames[0].map[0] = t as u16;
        let rgba = compose_frame_rgba(&view, 0, false);
        let (w, h) = rgba.dimensions();
        let image = to_render_image(rgba.as_raw(), w, h)
            .ok_or_else(|| format!("tile {t}: composed image had invalid dimensions"))?;
        tiles.push(image);
    }
    Ok(tiles)
}

/// Open `rel` and compose every frame and pool tile.
pub fn load_sprite(project_dir: &Path, rel: &str) -> Result<LoadedSprite, String> {
    let opened = io::open_sprite(project_dir, rel).map_err(|e| e.to_string())?;
    let frames = compose_frames(&opened.state)?;
    let pool_tiles = compose_pool_tiles(&opened.state)?;
    Ok(LoadedSprite {
        state: opened.state,
        til_path: opened.til_path,
        pal_path: opened.pal_path,
        frames,
        pool_tiles,
    })
}
