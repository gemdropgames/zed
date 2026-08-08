//! Shared helpers for GGO fork panels -- currently just the RGBA->BGRA
//! `RenderImage` bridge (gpui's `RenderImage` frames are BGRA, see
//! `gpui/src/assets.rs`'s "A cached and processed image, in BGRA format";
//! worldlib composes straight-alpha RGBA, so only a channel swap is
//! needed, no alpha unpremultiply). Deliberately depends on only `gpui`
//! and `image` -- no worldlib dependency -- so any GGO panel can use it
//! without pulling in world-doc types.

use std::sync::Arc;

use gpui::RenderImage;
use image::Frame;

/// In-place RGBA8 -> BGRA8 (straight alpha in, straight alpha out --
/// gpui's own non-SVG decode paths do exactly this `swap(0, 2)`, see
/// `gpui/src/elements/img.rs`'s WebP branch; the SVG path's extra alpha
/// divide is for tiny-skia's PREMULTIPLIED output, which worldlib's
/// composes are not).
pub fn rgba_to_bgra(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// Build the one gpui-side image for a composed `w`x`h` RGBA8 buffer.
/// Intended to be called once per source image at load time, never per
/// frame.
pub fn to_render_image(rgba: &[u8], w: u32, h: u32) -> Option<Arc<RenderImage>> {
    let mut data = rgba.to_vec();
    rgba_to_bgra(&mut data);
    let buffer = image::ImageBuffer::from_raw(w, h, data)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic red/blue swap bug: RGBA in, BGRA out, alpha untouched.
    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_only() {
        let mut data = vec![10, 20, 30, 40, 1, 2, 3, 4];
        rgba_to_bgra(&mut data);
        assert_eq!(data, vec![30, 20, 10, 40, 3, 2, 1, 4]);
    }

    #[test]
    fn to_render_image_produces_one_frame_of_the_right_size() {
        let rgba = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let rendered = to_render_image(&rgba, 2, 1).unwrap();
        assert_eq!(rendered.frame_count(), 1);
        // Red pixel first: BGRA bytes [0, 0, 255, 255].
        assert_eq!(
            rendered.as_bytes(0).unwrap(),
            &[0, 0, 255, 255, 255, 0, 0, 255]
        );
    }
}
