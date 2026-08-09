//! Off-thread sprite loading: enumerate `.spr` files and open one into a
//! [`SpriteState`] plus per-frame composed [`RenderImage`]s -- the
//! metasprite-panel analog of `ggo_world_panel::loader` (same one-shot
//! background pass; the panel guards staleness with a load-generation
//! counter).
//!
//! Enumeration is worldlib's own `sprites::io::list_sprites` -- the exact
//! walk ggo-ide's asset rail uses for its sprite listing
//! (`pages/assets/sprite.rs` -> `io::list_sprites`): every decode-valid
//! `.spr` under the project root, case-insensitive extension, dotfiles +
//! `target`/`node_modules`/`.git`/`dist` skipped, depth-capped, sorted
//! forward-slash rel paths. Nothing to port -- the semantics live in the
//! shared crate.

use std::path::Path;
use std::sync::Arc;

use ggo_common::to_render_image;
use ggo_worldlib::sprites::cow::SpriteState;
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::preview::compose_frame_rgba;
use gpui::RenderImage;

/// Everything the panel needs to enter its Ready state, assembled
/// entirely off the UI thread.
pub struct LoadedSprite {
    pub state: SpriteState,
    /// One composed BGRA image per frame index, built once at load time
    /// (frame-strip thumbnail AND large preview -- gpui scales the same
    /// image to both sizes). M5 hook: when the doc becomes editable,
    /// recompose the touched frame's entry on each doc op instead of
    /// reloading; this per-index Vec is the cache to invalidate.
    pub frames: Vec<Arc<RenderImage>>,
}

/// Every `.spr` under `root`, sorted rel paths -- the picker's feed.
pub fn list_sprites(root: &Path) -> Vec<String> {
    io::list_sprites(root)
}

/// Open `rel` and compose every frame (no LCD filter -- thumbnails and
/// preview alike, matching ggo-ide's frame strip).
pub fn load_sprite(project_dir: &Path, rel: &str) -> Result<LoadedSprite, String> {
    let opened = io::open_sprite(project_dir, rel).map_err(|e| e.to_string())?;
    let state = opened.state;
    let mut frames = Vec::with_capacity(state.frames.len());
    for idx in 0..state.frames.len() {
        let rgba = compose_frame_rgba(&state, idx, false);
        let (w, h) = rgba.dimensions();
        let image = to_render_image(rgba.as_raw(), w, h)
            .ok_or_else(|| format!("frame {idx}: composed image had invalid dimensions"))?;
        frames.push(image);
    }
    Ok(LoadedSprite { state, frames })
}
