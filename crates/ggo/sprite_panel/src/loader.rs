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
use ggo_worldlib::sprites::hw;
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::palette565::indices_to_rgba;
use ggo_worldlib::sprites::preview::{compose_frame_rgba, compose_frame_rgba_transformed};
use ggo_worldlib::sprites::tileset_doc::{
    TILE_PIXELS, compose_tile_grid, tile_grid_layout, unpack_til_to_indices,
};
use gpui::RenderImage;

use crate::editor_meta::{self, EditorMeta};
use crate::onion;

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
    /// The per-sprite editor-settings sidecar (defaults when absent).
    pub meta: EditorMeta,
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
    /// Sheet cells in display order, each naming the POOL index it
    /// shows. All-zero (blank) pool tiles are excluded outright: the doc
    /// needs them for empty cells, but they are never the user's to pick
    /// -- erasing is the Eraser tool's job, and showing them reads as a
    /// stray extra tile.
    pub tiles: Vec<u16>,
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

/// Compose frame `idx` with its affine transform applied, for the big
/// preview: worldlib's `compose_frame_rgba_transformed` (identity
/// delegates to the legacy composer; non-identity renders onto the
/// doubled DOUBLE_SIZE canvas), bridged to a [`RenderImage`] exactly
/// like [`compose_frames`]. The strip thumbnails deliberately stay
/// legacy-composed -- transforms are per-copy PLAY data, and a rotated
/// thumbnail would hide which tiles the frame actually edits. Callers
/// cache the result per frame (`OpenSprite::transformed_frames`,
/// the ghost-cache idiom): pixels only change on doc mutations, which
/// clear the cache.
pub fn compose_transformed_frame(state: &SpriteState, idx: usize) -> Option<Arc<RenderImage>> {
    let rgba = compose_frame_rgba_transformed(state, idx, false);
    let (w, h) = rgba.dimensions();
    to_render_image(rgba.as_raw(), w, h)
}

/// Compose the sprite's pool -- which IS its bound `.til`, byte for byte
/// (`io::open_sprite` reads the tileset file straight into
/// `SpriteState::pool`) -- into the tile picker's single sheet image,
/// `cols` tiles wide (clamped to at least 1 and at most the pool's tile
/// count; [`PICKER_COLS`] is the panel's default wrap).
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
pub fn compose_pool_strip(state: &SpriteState, cols: usize) -> Option<PoolStrip> {
    let indices = unpack_til_to_indices(&state.pool, state.tile_count);
    // Display order = pool order minus the blanks.
    let tiles: Vec<u16> = (0..state.tile_count)
        .filter(|&t| {
            let off = t * hw::TILE_BYTES;
            !state.pool[off..off + hw::TILE_BYTES].iter().all(|&b| b == 0)
        })
        .map(|t| t as u16)
        .collect();
    if tiles.is_empty() {
        return None;
    }
    let mut shown = Vec::with_capacity(tiles.len() * TILE_PIXELS);
    for &t in &tiles {
        let off = t as usize * TILE_PIXELS;
        shown.extend_from_slice(&indices[off..off + TILE_PIXELS]);
    }
    let cols = cols.max(1).min(tiles.len());
    let (sheet, w, h) = compose_tile_grid(&shown, tiles.len(), cols);
    let rgba = indices_to_rgba(&sheet, &state.palette);
    let image = to_render_image(&rgba, w as u32, h as u32)?;
    let (_, rows) = tile_grid_layout(tiles.len(), cols);
    Some(PoolStrip {
        image,
        cols,
        rows: rows.max(1),
        tiles,
    })
}

/// Blend every OPAQUE pixel of `rgba` (straight RGBA8, four bytes per
/// pixel) toward `tint` by `strength`, leaving alpha untouched. A pixel
/// whose alpha is already 0 (the locked transparent palette slot,
/// `palette565::TRANSPARENT_SLOT`) is skipped outright -- tinting its RGB
/// would do nothing visible on its own, but leaving the branch out would
/// be exactly the "tint the transparent index into visibility" bug this
/// exists to avoid if a future caller ever stops treating alpha 0 as
/// fully transparent.
fn tint_rgba(rgba: &mut [u8], tint: (u8, u8, u8), strength: f32) {
    let strength = strength.clamp(0.0, 1.0);
    let (tr, tg, tb) = (tint.0 as f32, tint.1 as f32, tint.2 as f32);
    for px in rgba.chunks_exact_mut(4) {
        if px[3] == 0 {
            continue;
        }
        px[0] = (px[0] as f32 + (tr - px[0] as f32) * strength).round() as u8;
        px[1] = (px[1] as f32 + (tg - px[1] as f32) * strength).round() as u8;
        px[2] = (px[2] as f32 + (tb - px[2] as f32) * strength).round() as u8;
    }
}

/// Compose one onion-skin ghost: frame `idx` of `state`, tinted red
/// (behind) or blue (ahead) per [`onion::tint_for`]/[`onion::tint_strength`]
/// for `dist` -- the panel's ghost's signed distance from the shown frame
/// (`onion::Ghost::dist`; negative is past, positive is future).
///
/// Reuses [`compose_frame_rgba`] -- the SAME decode [`compose_frames`]
/// calls -- rather than re-expanding indices to RGBA itself (ggo PR #80's
/// one definition of that expansion lives in
/// [`ggo_worldlib::sprites::palette565::indices_to_rgba`], which
/// `compose_frame_rgba` already calls); this only tints the bytes that
/// decode already produced. Straight RGBA in and out of [`tint_rgba`],
/// exactly like `compose_frames`' own buffers -- [`to_render_image`] does
/// the one BGRA swap, here as everywhere else.
///
/// Callers should cache the result keyed by `(dist, idx)`
/// (`OpenSprite::ghost_cache`) -- this recomposes from scratch every call,
/// which is fine once per cache miss but wrong to call every paint.
pub fn compose_ghost(state: &SpriteState, idx: usize, dist: i32) -> Option<Arc<RenderImage>> {
    let rgba = compose_frame_rgba(state, idx, false);
    let (w, h) = rgba.dimensions();
    let mut bytes = rgba.into_raw();
    tint_rgba(
        &mut bytes,
        onion::tint_for(dist),
        onion::tint_strength(dist),
    );
    to_render_image(&bytes, w, h)
}

/// Open `rel` and compose every frame plus the tile-picker sheet.
pub fn load_sprite(project_dir: &Path, rel: &str) -> Result<LoadedSprite, String> {
    let opened = io::open_sprite(project_dir, rel).map_err(|e| e.to_string())?;
    let frames = compose_frames(&opened.state)?;
    let meta = editor_meta::load(project_dir, rel);
    let pool_strip = compose_pool_strip(&opened.state, meta.picker_cols.unwrap_or(PICKER_COLS));
    Ok(LoadedSprite {
        state: opened.state,
        til_path: opened.til_path,
        pal_path: opened.pal_path,
        frames,
        pool_strip,
        meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::cow::{PixelWrite, write_pixels};
    use ggo_worldlib::sprites::sprite_doc::blank_sprite_state;

    // ----------------------------------------------- compose_pool_strip

    #[test]
    fn compose_pool_strip_excludes_blank_tiles_from_the_sheet() {
        let mut s = blank_sprite_state(1, 1).expect("1x1 sprite");
        // Tile 0 is worldlib's blank; append non-blank tile 1, a second
        // blank tile 2, and non-blank tile 3. The picker must show ONLY
        // the two non-blank tiles -- the blanks the doc needs for empty
        // cells are never the user's to pick (Eraser covers erasing).
        s.pool.extend(std::iter::repeat_n(0x11u8, 128));
        s.pool.extend(std::iter::repeat_n(0u8, 128));
        s.pool.extend(std::iter::repeat_n(0x22u8, 128));
        s.tile_count = 4;
        let strip = compose_pool_strip(&s, 4).expect("sheet composes");
        assert_eq!(
            strip.tiles,
            vec![1, 3],
            "display order maps sheet cells to pool indices, blanks gone"
        );
        assert_eq!((strip.cols, strip.rows), (2, 1), "layout fits 2 tiles");
    }

    #[test]
    fn compose_pool_strip_of_an_all_blank_pool_is_none() {
        let s = blank_sprite_state(1, 1).expect("1x1 sprite");
        assert!(
            compose_pool_strip(&s, 4).is_none(),
            "nothing pickable -> no sheet"
        );
    }

    // ------------------------------------------------------- tint_rgba

    #[test]
    fn tint_rgba_pushes_an_opaque_pixel_toward_the_tint_and_leaves_alpha_alone() {
        let mut rgba = vec![10, 20, 30, 255];
        tint_rgba(&mut rgba, (200, 0, 0), 0.5);
        // Halfway from (10, 20, 30) to (200, 0, 0), alpha untouched.
        assert_eq!(rgba, vec![105, 10, 15, 255]);
    }

    #[test]
    fn tint_rgba_leaves_a_transparent_pixel_fully_untouched() {
        let mut rgba = vec![0, 0, 0, 0];
        tint_rgba(&mut rgba, (255, 0, 0), 1.0);
        assert_eq!(
            rgba,
            vec![0, 0, 0, 0],
            "alpha-0 pixels must never be tinted into visibility"
        );
    }

    #[test]
    fn tint_rgba_at_full_strength_becomes_the_tint_color_exactly() {
        let mut rgba = vec![1, 2, 3, 255];
        tint_rgba(&mut rgba, onion::TINT_PREV, 1.0);
        assert_eq!(
            rgba,
            vec![
                onion::TINT_PREV.0,
                onion::TINT_PREV.1,
                onion::TINT_PREV.2,
                255
            ]
        );
    }

    // ------------------------------------------------------ compose_ghost

    /// A single-tile sprite with one opaque pixel set to a known
    /// mid-tone: enough to tell, byte for byte, that a PAST ghost (dist
    /// -1) is pushed toward red and a FUTURE ghost (dist +1) toward blue
    /// for the exact same source pixel, and that the frame's remaining
    /// (still-transparent) pixels stay alpha 0 in both directions -- the
    /// two things TASK 1 asked this fast-follow to prove at the byte
    /// level, through the real `compose_frame_rgba` -> `to_render_image`
    /// path rather than just the `tint_rgba` helper in isolation.
    #[test]
    fn compose_ghost_tints_a_past_ghost_red_and_a_future_ghost_blue_for_the_same_pixel() {
        let mut s = blank_sprite_state(1, 1).expect("1x1 tile sprite");
        // A mid-grey slot, opaque and far from either tint so the push is
        // unambiguous in both directions.
        s.palette[1] = 0x0000; // 565 black -> rgb888 (0, 0, 0), opaque
        let r = write_pixels(
            &s,
            0,
            &[PixelWrite {
                x: 0,
                y: 0,
                index: 1,
            }],
        )
        .expect("in-bounds pixel write");
        s = r.state;

        let past = compose_ghost(&s, 0, -1).expect("past ghost composes");
        let future = compose_ghost(&s, 0, 1).expect("future ghost composes");
        // BGRA bytes: [B, G, R, A].
        let past_px = past.as_bytes(0).expect("one frame");
        let future_px = future.as_bytes(0).expect("one frame");

        // The written pixel, blended from black toward each tint at
        // ONION_FALLOFF's dist-1 (nearest-ghost) strength: past reads
        // redder (higher R, byte index 2) than future, future reads
        // bluer (higher B, byte index 0) than past.
        assert!(
            past_px[2] > future_px[2],
            "past ghost should read redder: past {past_px:?} future {future_px:?}"
        );
        assert!(
            future_px[0] > past_px[0],
            "future ghost should read bluer: past {past_px:?} future {future_px:?}"
        );
        assert_eq!(past_px[3], 255, "the written pixel stays opaque");
        assert_eq!(future_px[3], 255, "the written pixel stays opaque");

        // Every other pixel in this 16x16 tile is still index 0
        // (transparent) -- both ghosts must leave it alpha 0, not tint it
        // into visibility.
        assert_eq!(
            &past_px[4..8],
            &[0, 0, 0, 0],
            "an untouched transparent pixel must stay alpha 0 in the past ghost"
        );
        assert_eq!(
            &future_px[4..8],
            &[0, 0, 0, 0],
            "an untouched transparent pixel must stay alpha 0 in the future ghost"
        );
    }
}
