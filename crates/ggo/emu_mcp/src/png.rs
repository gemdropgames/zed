//! BGRA8 framebuffer -> PNG bytes, for MCP image content.

/// Encode a BGRA8 buffer (the panel's native frame format) as PNG.
pub fn bgra_to_png(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, String> {
    if bgra.len() != (width * height * 4) as usize {
        return Err(format!(
            "framebuffer is {} bytes, expected {}x{}x4 = {}",
            bgra.len(),
            width,
            height,
            width * height * 4
        ));
    }
    let mut rgba = bgra.to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(width, height, rgba).expect("size checked above");
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_pixel_through_png() {
        // One blue-ish BGRA pixel: B=255, G=16, R=32.
        let png = bgra_to_png(1, 1, &[255, 16, 32, 255]).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [32, 16, 255, 255]); // RGBA order
    }

    #[test]
    fn wrong_length_is_a_named_error() {
        let err = bgra_to_png(2, 2, &[0; 3]).unwrap_err();
        assert!(err.contains("expected 2x2x4"), "{err}");
    }
}
