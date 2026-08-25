//! The project panel's thumbnail decoder for GGO assets (registered with
//! `workspace::ggo_thumbnails` from `init`): a `.til` shows its first
//! tiles, a `.spr` its first frame, a `.png` itself -- each fitted into a
//! [`THUMB_PX`] square off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ggo_common::{thumbnail_rgba, to_render_image};
use ggo_worldlib::sprites::import::decode_png;
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::palette565::indices_to_rgba;
use ggo_worldlib::sprites::preview::compose_frame_rgba;
use ggo_worldlib::sprites::tileset_doc::{compose_tile_grid, tile_grid_layout, TILE_PX};
use gpui::RenderImage;

pub const EXTENSIONS: &[&str] = &["til", "spr", "png"];
pub const THUMB_PX: usize = 16;
/// A `.til` thumbnail shows its first `THUMB_COLS x THUMB_ROWS` tiles.
const THUMB_COLS: usize = 4;
const THUMB_ROWS: usize = 2;

pub fn decode_thumbnail(path: &Path) -> Option<Arc<RenderImage>> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let (rgba, w, h) = match extension.as_str() {
        "png" => {
            let bytes = std::fs::read(path).ok()?;
            let png = decode_png(&bytes).ok()?;
            (png.rgba, png.w, png.h)
        }
        "til" => {
            let (root, rel) = split_for_worldlib(path)?;
            let tileset = io::open_tileset(&root, &rel).ok()?;
            let shown = tileset.tile_count.min(THUMB_COLS * THUMB_ROWS);
            let cols = THUMB_COLS.min(shown.max(1));
            let pixels = tileset.indices.get(..shown * TILE_PX * TILE_PX)?;
            let (indices, ..) = compose_tile_grid(pixels, shown, cols);
            let (_, rows) = tile_grid_layout(shown, cols);
            (
                indices_to_rgba(&indices, &tileset.palette),
                cols * TILE_PX,
                rows.max(1) * TILE_PX,
            )
        }
        "spr" => {
            let (root, rel) = split_for_worldlib(path)?;
            let sprite = io::open_sprite(&root, &rel).ok()?;
            let image = compose_frame_rgba(&sprite.state, 0, false);
            let (w, h) = (image.width() as usize, image.height() as usize);
            (image.into_raw(), w, h)
        }
        _ => return None,
    };
    let thumb = thumbnail_rgba(&rgba, w, h, THUMB_PX);
    to_render_image(&thumb, THUMB_PX as u32, THUMB_PX as u32)
}

/// worldlib opens assets by `(root, rel)`: the emerald asset root when
/// `path` is under one (so a `.spr`'s asset-root-relative `.til` resolves),
/// else the file's own directory.
fn split_for_worldlib(path: &Path) -> Option<(PathBuf, String)> {
    let name = path.file_name()?.to_str()?.to_string();
    if let Some(project) = ggo_common::emerald_project_root(path) {
        let assets = project.join(crate::ASSETS_DIR);
        if let Ok(under) = path.strip_prefix(&assets) {
            let rel = under
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            return Some((assets, rel));
        }
    }
    Some((path.parent()?.to_path_buf(), name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{two_tone_rgba, write_png_fixture};
    use ggo_worldlib::sprites::palette565::PAL_SLOTS;

    #[test]
    fn png_and_til_thumbnails_are_thumb_sized() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        write_png_fixture(&png, 32, 8, &two_tone_rgba(32, 8));
        let image = decode_thumbnail(&png).expect("png thumbnail");
        assert_eq!(image.size(0), gpui::size(THUMB_PX as i32, THUMB_PX as i32).map(|d| d.into()));

        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800;
        io::save_tileset(dir.path(), "t.til", &[1u8; TILE_PX * TILE_PX], 1, &palette).unwrap();
        let image = decode_thumbnail(&dir.path().join("t.til")).expect("til thumbnail");
        let bytes = image.as_bytes(0).unwrap();
        assert_eq!(bytes.len(), THUMB_PX * THUMB_PX * 4);
        assert_eq!(&bytes[..4], &[0, 0, 255, 255], "BGRA red at the top-left");

        assert!(decode_thumbnail(&dir.path().join("nope.til")).is_none());
        assert!(decode_thumbnail(&dir.path().join("x.txt")).is_none());
    }

    #[gpui::test]
    async fn the_cache_decodes_off_thread_and_notifies(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        write_png_fixture(&png, 4, 4, &two_tone_rgba(4, 4));
        let cache = cx.update(|cx| {
            workspace::ggo_thumbnails::register_thumbnail_decoder(cx, EXTENSIONS, decode_thumbnail);
            workspace::ggo_thumbnails::thumbnail_cache(cx).expect("a decoder is registered")
        });
        let first = cache.update(cx, |cache, cx| cache.get(&png, cx));
        assert!(first.is_none(), "a miss starts the decode and returns nothing yet");
        cx.executor().run_until_parked();
        let second = cache.update(cx, |cache, cx| cache.get(&png, cx));
        assert!(second.is_some(), "the decode landed");
        assert!(cache.read_with(cx, |cache, _| cache.is_cached(&png)));
        let none = cache.update(cx, |cache, cx| cache.get(&dir.path().join("b.txt"), cx));
        assert!(none.is_none(), "no decoder for .txt");
    }
}
