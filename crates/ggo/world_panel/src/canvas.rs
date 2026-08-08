//! The viewer's paint layer: pure camera math (pan/zoom, tested), the
//! RGBA->BGRA `RenderImage` bridge (gpui's `RenderImage` frames are BGRA --
//! see `gpui/src/assets.rs`'s "A cached and processed image, in BGRA
//! format"; worldlib composes straight-alpha RGBA, so only a channel swap
//! is needed, no alpha unpremultiply), and the per-`DrawKind` painters.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, Hsla, Path as GpuiPath, Pixels, Point,
    RenderImage, SharedString, TextAlign, TextRun, Window, bounds, fill, outline, point, px, size,
};
use image::Frame;
use smallvec::SmallVec;

use ggo_worldlib::drag_ops::{self, View};
use ggo_worldlib::render::{
    AssetLoads, DEVICE_SCREEN_H, DEVICE_SCREEN_W, DrawItem, DrawKind, Loadable, RgbaImage,
};

// ------------------------------------------------------------ camera math

pub const ZOOM_MIN: f64 = 0.25;
pub const ZOOM_MAX: f64 = 8.0;

/// Integer-ish zoom ladder (task brief: wheel zoom in steps, clamped
/// 0.25x..8x) -- deliberately NOT ggo-ide's multiplicative 1.1 wheel
/// factor.
pub const ZOOM_LEVELS: &[f64] = &[0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0];

/// Next ladder step above (`dir > 0`) or below (`dir < 0`) `zoom`,
/// saturating at the ladder ends.
pub fn zoom_step(zoom: f64, dir: i32) -> f64 {
    const EPS: f64 = 1e-9;
    if dir > 0 {
        for &level in ZOOM_LEVELS {
            if level > zoom + EPS {
                return level;
            }
        }
        ZOOM_MAX
    } else {
        for &level in ZOOM_LEVELS.iter().rev() {
            if level < zoom - EPS {
                return level;
            }
        }
        ZOOM_MIN
    }
}

/// New pan such that the world point under `cursor` (canvas-relative CSS
/// px) stays under it after switching to `new_zoom`.
pub fn zoom_at(pan: [f64; 2], zoom: f64, cursor: [f64; 2], new_zoom: f64) -> [f64; 2] {
    let view = View {
        zoom,
        pan_x: pan[0],
        pan_y: pan[1],
        dpr: None,
    };
    let world = drag_ops::screen_to_world(cursor[0], cursor[1], &view);
    [
        cursor[0] - world[0] * new_zoom,
        cursor[1] - world[1] * new_zoom,
    ]
}

/// Pan placing `world_center` at the canvas center.
pub fn centering_pan(canvas_w: f64, canvas_h: f64, zoom: f64, world_center: [f64; 2]) -> [f64; 2] {
    [
        canvas_w / 2.0 - world_center[0] * zoom,
        canvas_h / 2.0 - world_center[1] * zoom,
    ]
}

/// The world point the camera view centers on: `active_camera_origin`'s
/// screen-rect origin plus half the device screen.
pub fn camera_center(origin: [f64; 2]) -> [f64; 2] {
    [
        origin[0] + DEVICE_SCREEN_W / 2.0,
        origin[1] + DEVICE_SCREEN_H / 2.0,
    ]
}

// -------------------------------------------------------- BGRA conversion

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

/// Build the one gpui-side image for a composed worldlib image. Called
/// once per stem at load time, never per frame.
pub fn to_render_image(img: &RgbaImage) -> Option<Arc<RenderImage>> {
    let mut data = img.rgba.to_vec();
    rgba_to_bgra(&mut data);
    let buffer = image::ImageBuffer::from_raw(img.w, img.h, data)?;
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        Frame::new(buffer),
        1,
    ))))
}

/// Key an [`RgbaImage`] by its shared pixel buffer's address --
/// `DrawKind::Image` carries an `Arc` clone of the load-map entry's
/// buffer, so the address is stable for the lifetime of a load set and
/// maps a draw item back to its cached `RenderImage` without a per-frame
/// conversion.
pub fn image_key(img: &RgbaImage) -> usize {
    Arc::as_ptr(&img.rgba) as *const u8 as usize
}

/// One `RenderImage` per `Loadable::Ready` entry across the given load
/// maps, keyed by [`image_key`].
pub fn build_image_cache(loads: &[&AssetLoads]) -> HashMap<usize, Arc<RenderImage>> {
    let mut cache = HashMap::new();
    for map in loads {
        for load in map.values() {
            if let Loadable::Ready(img) = load
                && let Some(render_image) = to_render_image(img)
            {
                cache.insert(image_key(img), render_image);
            }
        }
    }
    cache
}

// ---------------------------------------------------------------- colors

fn color(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

fn rgb565_color(c: u16) -> Hsla {
    let (r, g, b) = ggo_asset_formats::pixel::rgb888(c);
    gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
}

const TEXT_FONT_SIZE: f32 = 12.0;
const TEXT_LINE_HEIGHT: f32 = 14.0;
const LABEL_GAP: f32 = 2.0;

// ---------------------------------------------------------------- painting

/// Everything the paint closure needs, captured at render time.
pub struct Scene {
    pub items: Vec<DrawItem>,
    pub images: Arc<HashMap<usize, Arc<RenderImage>>>,
    pub zoom: f64,
    pub pan: [f64; 2],
    /// `active_camera_origin` -- the device-screen outline's top-left.
    pub screen_origin: [f64; 2],
    pub background: Hsla,
    pub text_color: Hsla,
}

pub fn paint_scene(
    scene: &Scene,
    canvas_bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    window.with_content_mask(
        Some(ContentMask {
            bounds: canvas_bounds,
        }),
        |window| {
            window.paint_quad(fill(canvas_bounds, scene.background));
            let view = View {
                zoom: scene.zoom,
                pan_x: scene.pan[0],
                pan_y: scene.pan[1],
                dpr: None,
            };
            for item in &scene.items {
                paint_item(scene, item, &view, canvas_bounds, window, cx);
            }
            paint_device_screen(scene, &view, canvas_bounds, window, cx);
        },
    );
}

fn to_screen(view: &View, origin: Point<Pixels>, wx: f64, wy: f64) -> Point<Pixels> {
    let s = drag_ops::world_to_screen(wx, wy, view);
    point(origin.x + px(s[0] as f32), origin.y + px(s[1] as f32))
}

fn item_bounds(
    view: &View,
    origin: Point<Pixels>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Bounds<Pixels> {
    let top_left = to_screen(view, origin, x, y);
    let bottom_right = to_screen(view, origin, x + w, y + h);
    bounds(
        top_left,
        size(bottom_right.x - top_left.x, bottom_right.y - top_left.y),
    )
}

fn paint_item(
    scene: &Scene,
    item: &DrawItem,
    view: &View,
    canvas_bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let b = item_bounds(view, canvas_bounds.origin, item.x, item.y, item.w, item.h);
    if !b.intersects(&canvas_bounds) {
        return;
    }
    match &item.kind {
        DrawKind::Rect { color565 } => {
            window.paint_quad(fill(b, rgb565_color(*color565)));
        }
        DrawKind::Text { content } => {
            window.paint_quad(fill(b, gpui::rgba(0x00000066)));
            paint_label(content, b.origin, scene.text_color, window, cx);
        }
        DrawKind::Marker => {
            window.paint_quad(outline(b, color(0x88c0d0), BorderStyle::default()));
        }
        DrawKind::InstanceOrigin => {
            // Crosshair: a horizontal and a vertical 1px line through the
            // box center.
            let c = b.center();
            let stroke = color(0xa3be8c);
            window.paint_quad(fill(
                bounds(point(b.origin.x, c.y), size(b.size.width, px(1.))),
                stroke,
            ));
            window.paint_quad(fill(
                bounds(point(c.x, b.origin.y), size(px(1.), b.size.height)),
                stroke,
            ));
        }
        DrawKind::Placeholder { stem } => {
            let stroke = color(0xd08770);
            window.paint_quad(outline(b, stroke, BorderStyle::default()));
            // Diagonal cross, matching ggo-ide's canvas fallback look
            // approximately.
            let bottom_right = point(b.origin.x + b.size.width, b.origin.y + b.size.height);
            let top_right = point(b.origin.x + b.size.width, b.origin.y);
            let bottom_left = point(b.origin.x, b.origin.y + b.size.height);
            if let Some(path) = line_path(b.origin, bottom_right, 1.0) {
                window.paint_path(path, stroke);
            }
            if let Some(path) = line_path(bottom_left, top_right, 1.0) {
                window.paint_path(path, stroke);
            }
            paint_label(
                stem,
                point(
                    b.origin.x,
                    b.origin.y - px(TEXT_LINE_HEIGHT) - px(LABEL_GAP),
                ),
                stroke,
                window,
                cx,
            );
        }
        DrawKind::Image { image } => match scene.images.get(&image_key(image)) {
            Some(render_image) => {
                let _ =
                    window.paint_image(b, b, Corners::default(), render_image.clone(), 0, false);
            }
            // A Ready image missing from the cache can only mean the cache
            // and draw list are out of sync -- degrade to a placeholder
            // outline rather than panic.
            None => window.paint_quad(outline(b, color(0xd08770), BorderStyle::default())),
        },
        DrawKind::SelectionOutline => {
            window.paint_quad(outline(b, color(0xebcb8b), BorderStyle::default()));
        }
    }
}

fn paint_device_screen(
    scene: &Scene,
    view: &View,
    canvas_bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let b = item_bounds(
        view,
        canvas_bounds.origin,
        scene.screen_origin[0],
        scene.screen_origin[1],
        DEVICE_SCREEN_W,
        DEVICE_SCREEN_H,
    );
    let stroke = color(0x5e81ac);
    window.paint_quad(outline(b, stroke, BorderStyle::default()));
    paint_label(
        "screen",
        point(
            b.origin.x,
            b.origin.y - px(TEXT_LINE_HEIGHT) - px(LABEL_GAP),
        ),
        stroke,
        window,
        cx,
    );
}

/// Paint a single line of text at fixed (screen-space) size -- same rule
/// as ggo-ide's canvas labels, which don't scale with zoom. Newlines
/// would panic `shape_line`, so only the first line is shown.
fn paint_label(text: &str, origin: Point<Pixels>, color: Hsla, window: &mut Window, cx: &mut App) {
    let first_line = text.lines().next().unwrap_or("");
    if first_line.is_empty() {
        return;
    }
    let text: SharedString = first_line.to_string().into();
    let run = TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(text, px(TEXT_FONT_SIZE), &[run], None);
    let _ = line.paint(
        origin,
        px(TEXT_LINE_HEIGHT),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// A `width`-px filled quad along `p0 -> p1` -- gpui paths are filled
/// regions, so a stroked line is a thin polygon. `None` for a degenerate
/// (zero-length) segment.
fn line_path(p0: Point<Pixels>, p1: Point<Pixels>, width: f32) -> Option<GpuiPath<Pixels>> {
    let dx = f32::from(p1.x - p0.x);
    let dy = f32::from(p1.y - p0.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len < f32::EPSILON {
        return None;
    }
    let nx = -dy / len * width / 2.0;
    let ny = dx / len * width / 2.0;
    let mut path = GpuiPath::new(point(p0.x + px(nx), p0.y + px(ny)));
    path.line_to(point(p1.x + px(nx), p1.y + px(ny)));
    path.line_to(point(p1.x - px(nx), p1.y - px(ny)));
    path.line_to(point(p0.x - px(nx), p0.y - px(ny)));
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_step_walks_the_ladder_and_saturates() {
        assert_eq!(zoom_step(1.0, 1), 2.0);
        assert_eq!(zoom_step(2.0, -1), 1.0);
        assert_eq!(zoom_step(8.0, 1), ZOOM_MAX);
        assert_eq!(zoom_step(0.25, -1), ZOOM_MIN);
        // An off-ladder zoom snaps to the nearest step in the direction of
        // travel.
        assert_eq!(zoom_step(1.5, 1), 2.0);
        assert_eq!(zoom_step(1.5, -1), 1.0);
    }

    #[test]
    fn zoom_at_keeps_the_world_point_under_the_cursor_fixed() {
        let pan = [10.0, 20.0];
        let zoom = 1.0;
        let cursor = [100.0, 80.0];
        let before = drag_ops::screen_to_world(
            cursor[0],
            cursor[1],
            &View {
                zoom,
                pan_x: pan[0],
                pan_y: pan[1],
                dpr: None,
            },
        );
        let new_zoom = 2.0;
        let new_pan = zoom_at(pan, zoom, cursor, new_zoom);
        let after = drag_ops::screen_to_world(
            cursor[0],
            cursor[1],
            &View {
                zoom: new_zoom,
                pan_x: new_pan[0],
                pan_y: new_pan[1],
                dpr: None,
            },
        );
        assert!((before[0] - after[0]).abs() < 1e-9);
        assert!((before[1] - after[1]).abs() < 1e-9);
    }

    #[test]
    fn centering_pan_puts_the_world_center_at_the_canvas_center() {
        let world_center = camera_center([-160.0, -120.0]); // -> [0, 0]
        assert_eq!(world_center, [0.0, 0.0]);
        let pan = centering_pan(400.0, 300.0, 2.0, [50.0, -25.0]);
        let screen = drag_ops::world_to_screen(
            50.0,
            -25.0,
            &View {
                zoom: 2.0,
                pan_x: pan[0],
                pan_y: pan[1],
                dpr: None,
            },
        );
        assert_eq!(screen, [200.0, 150.0]);
    }

    /// The classic red/blue swap bug: RGBA in, BGRA out, alpha untouched.
    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_only() {
        let mut data = vec![10, 20, 30, 40, 1, 2, 3, 4];
        rgba_to_bgra(&mut data);
        assert_eq!(data, vec![30, 20, 10, 40, 3, 2, 1, 4]);
    }

    #[test]
    fn to_render_image_produces_one_frame_of_the_right_size() {
        let img = RgbaImage {
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 255].into(),
            w: 2,
            h: 1,
        };
        let rendered = to_render_image(&img).unwrap();
        assert_eq!(rendered.frame_count(), 1);
        // Red pixel first: BGRA bytes [0, 0, 255, 255].
        assert_eq!(
            rendered.as_bytes(0).unwrap(),
            &[0, 0, 255, 255, 255, 0, 0, 255]
        );
    }

    #[test]
    fn build_image_cache_keys_ready_images_by_buffer_address() {
        let img = RgbaImage {
            rgba: vec![0, 0, 0, 0].into(),
            w: 1,
            h: 1,
        };
        let mut loads = AssetLoads::new();
        loads.insert("a".into(), Loadable::Ready(img.clone()));
        loads.insert("b".into(), Loadable::Loading);
        loads.insert("c".into(), Loadable::Error);
        let cache = build_image_cache(&[&loads]);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&image_key(&img)));
    }
}
