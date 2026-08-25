//! Off-thread PNG decoding and the two gpui images the panel draws: the raw
//! source (the crop surface) and the quantized preview (what a commit will
//! actually write).
//!
//! The split mirrors every other GGO panel's `loader`: the one slow,
//! filesystem-touching step runs on a background task behind the panel's
//! load-generation guard, and nothing here knows about gpui state.
//!
//! Neither decode nor quantize is reimplemented -- both are
//! `ggo_worldlib::sprites::import`'s (`decode_png`, `quantize_region`,
//! `preview_rgba`), and the indexed->RGBA expansion inside `preview_rgba` is
//! the ONE shared `palette565::indices_to_rgba` rule (ggo PR #80). This
//! module is the `Arc<RenderImage>` bridge and nothing else.

use std::path::Path;
use std::sync::Arc;

use ggo_common::to_render_image;
use ggo_worldlib::sprites::import::{
    DecodedFrame, DecodedPng, Preview, decode_source, preview_rgba,
};
use gpui::RenderImage;

/// A decoded source PNG plus its gpui image, assembled entirely off the UI
/// thread.
pub struct LoadedPng {
    pub decoded: DecodedPng,
    /// The raw source as ONE composed BGRA image, built once at load: the
    /// crop gesture changes the OUTLINE, never the pixels underneath, so
    /// there is no invalidation point at all.
    pub image: Arc<RenderImage>,
    /// Every frame (one for a PNG); a multi-frame source becomes a
    /// sprite's frames.
    pub frames: Vec<DecodedFrame>,
}

/// Read and decode the PNG at absolute path `abs`.
///
/// Takes an ABSOLUTE path, not a project-relative one: unlike every other
/// GGO panel's loader, the thing being opened is a source file the user
/// points at, and the asset root a commit writes against is derived
/// separately (and may differ from the worktree root the click came in
/// through) -- see the panel's `split_png_path`.
pub fn load_png(abs: &Path) -> Result<LoadedPng, String> {
    let bytes = std::fs::read(abs).map_err(|e| format!("reading {}: {e}", abs.display()))?;
    let name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let frames = decode_source(name, &bytes).map_err(|e| e.to_string())?;
    let first = frames
        .first()
        .ok_or_else(|| "the file has no frames".to_string())?;
    let decoded: DecodedPng = first.clone().into();
    let image = to_render_image(&decoded.rgba, decoded.w as u32, decoded.h as u32)
        .ok_or_else(|| "the image has no pixels".to_string())?;
    Ok(LoadedPng {
        decoded,
        image,
        frames,
    })
}

/// Lay `frames` side by side into one RGBA strip (`w * n` wide), so the
/// sprite cut can stay one `sprite_import` call.
pub fn frame_strip(frames: &[DecodedFrame]) -> (Vec<u8>, usize, usize) {
    let Some(first) = frames.first() else {
        return (Vec::new(), 0, 0);
    };
    let (w, h, n) = (first.w, first.h, frames.len());
    let mut strip = vec![0u8; w * n * h * 4];
    for (index, frame) in frames.iter().enumerate() {
        for y in 0..h.min(frame.h) {
            let src = &frame.rgba[y * frame.w * 4..][..w.min(frame.w) * 4];
            let at = (y * w * n + index * w) * 4;
            strip[at..at + src.len()].copy_from_slice(src);
        }
    }
    (strip, w * n, h)
}

/// Composite a quantized [`Preview`] into a gpui image -- what the commit
/// will write, drawn through the palette it derived.
///
/// `None` for a zero-sized preview (an empty crop), which is a state the
/// canvas simply doesn't draw rather than an error: the next crop drag
/// recovers.
pub fn preview_image(preview: &Preview) -> Option<Arc<RenderImage>> {
    to_render_image(&preview_rgba(preview), preview.w as u32, preview.h as u32)
}

/// Encode a `w` x `h` RGBA buffer as a real PNG on disk -- the fixture every
/// test in this crate imports from. Deliberately at module level (not inside
/// `mod tests`) so the panel's own test module can reach it: the point of
/// these tests is that a REAL PNG round-trips, so there is exactly one
/// encoder and both test modules use it.
#[cfg(test)]
pub(crate) fn write_png_fixture(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let buffer: image::RgbaImage = image::ImageBuffer::from_raw(w, h, rgba.to_vec()).unwrap();
    buffer
        .save_with_format(path, image::ImageFormat::Png)
        .unwrap();
}

/// A `w` x `h` source: an opaque red left half and an opaque blue right
/// half, so a crop provably changes which colors reach the palette.
#[cfg(test)]
pub(crate) fn two_tone_rgba(w: usize, h: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(w * h * 4);
    for _ in 0..h {
        for x in 0..w {
            if x < w / 2 {
                rgba.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                rgba.extend_from_slice(&[0, 0, 255, 255]);
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::import::{Region, quantize_region};

    #[test]
    fn load_png_round_trips_the_pixels_and_builds_a_bgra_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art/hero.png");
        let rgba = two_tone_rgba(4, 2);
        write_png_fixture(&path, 4, 2, &rgba);

        let loaded = load_png(&path).unwrap();
        assert_eq!((loaded.decoded.w, loaded.decoded.h), (4, 2));
        assert_eq!(loaded.decoded.rgba, rgba);
        // BGRA out: the first pixel is red, so [0, 0, 255, 255].
        let bytes = loaded.image.as_bytes(0).unwrap();
        assert_eq!(bytes.len(), 4 * 2 * 4);
        assert_eq!(&bytes[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn load_png_reads_every_aseprite_frame_and_strips_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anim.ase");
        let a = two_tone_rgba(2, 1);
        let b: Vec<u8> = a.chunks(4).rev().flatten().copied().collect();
        std::fs::write(
            &path,
            ggo_worldlib::sprites::aseprite::encode_rgba_frames(&[&a, &b], 2, 1),
        )
        .unwrap();
        let loaded = load_png(&path).unwrap();
        assert_eq!(loaded.frames.len(), 2);
        assert_eq!(loaded.decoded.rgba, a, "frame 0 is the wizard's image");
        let (strip, w, h) = frame_strip(&loaded.frames);
        assert_eq!((w, h), (4, 1));
        assert_eq!(strip, [a, b].concat());
    }

    #[test]
    fn load_png_reports_a_missing_or_undecodable_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_png(&dir.path().join("nope.png")).is_err());
        let bad = dir.path().join("bad.png");
        std::fs::write(&bad, b"not a png").unwrap();
        assert!(load_png(&bad).is_err());
    }

    /// The preview image is the quantized crop, at the crop's size -- not
    /// the source's.
    #[test]
    fn preview_image_is_the_size_of_the_quantized_crop() {
        let rgba = two_tone_rgba(8, 8);
        let preview = quantize_region(
            &rgba,
            8,
            8,
            true,
            Some(Region {
                x: 0,
                y: 0,
                w: 4,
                h: 8,
            }),
        )
        .unwrap();
        let image = preview_image(&preview).unwrap();
        assert_eq!(image.as_bytes(0).unwrap().len(), 4 * 8 * 4);
    }
}
