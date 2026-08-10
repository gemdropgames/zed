//! Off-thread sprite loading: open one `.spr` into a [`SpriteState`] plus
//! per-frame composed [`RenderImage`]s -- the sprite-panel analog of
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
use ggo_worldlib::sprites::cow::SpriteState;
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::palette565::indices_to_rgba;
use ggo_worldlib::sprites::preview::compose_frame_rgba;
use ggo_worldlib::sprites::tileset_doc::{
    compose_tile_grid, tile_grid_layout, unpack_til_to_indices,
};
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
    /// The bound tileset's tiles as ONE composed sheet -- the tile
    /// picker. Same invalidation story as `frames`: rebuilt wholesale via
    /// [`compose_pool_strip`] after every doc mutation (dedup fold-back on
    /// save, undo across it). `None` only if the sheet came out with
    /// dimensions gpui can't build an image from.
    pub pool_strip: Option<PoolStrip>,
}

/// How many tiles wide the tile picker lays the bound tileset out. Fixed
/// rather than [`ggo_tileset_panel`]'s `resolve_cols` chain: this strip
/// lives in a narrow side column next to the frame grid, so the column
/// count is a LAYOUT constant here, not a property of the asset.
pub const PICKER_COLS: usize = 4;

/// The bound tileset composed for the picker: one sheet image plus the
/// grid it was laid out in, which is what [`super::tiles::picker_tile_at`]
/// needs to turn a click into a pool index.
pub struct PoolStrip {
    pub image: Arc<RenderImage>,
    pub cols: usize,
    pub rows: usize,
    pub tile_count: usize,
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

/// Compose the sprite's pool -- which IS its bound `.til`, byte for byte
/// (`io::open_sprite` reads the tileset file straight into
/// `SpriteState::pool`) -- into the tile picker's single sheet image,
/// [`PICKER_COLS`] tiles wide.
///
/// Deliberately the SAME three worldlib calls `ggo_tileset_panel::loader`
/// makes for its own sheet -- `unpack_til_to_indices` ->
/// `compose_tile_grid` -> [`indices_to_rgba`] -- rather than a private
/// index->RGBA loop. That rule (transparent slot 0, 565->888) has exactly
/// one implementation, in worldlib, since ggo PR #80 collapsed four copies
/// onto it; a fifth here is precisely what that PR removed.
///
/// This replaces F2/M6's per-tile compose, which faked a bare tile render
/// by repointing the sole cell of a 1x1-tile `SpriteState` VIEW at each
/// pool tile in turn and running `compose_frame_rgba` over it -- a
/// workaround for an entry point that now exists.
pub fn compose_pool_strip(state: &SpriteState) -> Option<PoolStrip> {
    let cols = PICKER_COLS.min(state.tile_count.max(1));
    let indices = unpack_til_to_indices(&state.pool, state.tile_count);
    let (sheet, w, h) = compose_tile_grid(&indices, state.tile_count, cols);
    let rgba = indices_to_rgba(&sheet, &state.palette);
    let image = to_render_image(&rgba, w as u32, h as u32)?;
    let (_, rows) = tile_grid_layout(state.tile_count, cols);
    Some(PoolStrip {
        image,
        cols,
        rows: rows.max(1),
        tile_count: state.tile_count,
    })
}

/// Open `rel` and compose every frame plus the tile-picker sheet.
pub fn load_sprite(project_dir: &Path, rel: &str) -> Result<LoadedSprite, String> {
    let opened = io::open_sprite(project_dir, rel).map_err(|e| e.to_string())?;
    let frames = compose_frames(&opened.state)?;
    let pool_strip = compose_pool_strip(&opened.state);
    Ok(LoadedSprite {
        state: opened.state,
        til_path: opened.til_path,
        pal_path: opened.pal_path,
        frames,
        pool_strip,
    })
}
