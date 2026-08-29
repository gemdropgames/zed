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

use ggo_common::to_render_image;
use ggo_worldlib::drag_ops::{self, View};
use ggo_worldlib::render::{
    AssetLoads, DEVICE_SCREEN_H, DEVICE_SCREEN_W, DrawItem, DrawKind, Loadable, RgbaImage,
};
use ggo_worldlib::sprites::tileset_doc::TILE_PX;

// ------------------------------------------------------------ camera math

pub const ZOOM_MIN: f64 = 0.25;
pub const ZOOM_MAX: f64 = 8.0;

/// Integer-ish zoom ladder (task brief: wheel zoom in steps, clamped
/// 0.25x..8x) -- deliberately NOT ggo-ide's multiplicative 1.1 wheel
/// factor.
pub const ZOOM_LEVELS: &[f64] = &[0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0];

/// The zoom a freshly-loaded world -- and the "Reset" button -- frames at.
/// Paired with [`reset_camera`]; see there for why reset does not compute
/// a pan.
pub const ZOOM_DEFAULT: f64 = 1.0;

/// The camera "Reset" restores: default zoom and NO pan. A `None` pan is
/// how `render_canvas`'s prepaint recognizes "never laid out", so the very
/// next paint re-runs the initial centering
/// (`centering_pan(bounds, camera_center(active_camera_origin(...)))`).
/// That is deliberately the whole of it: recomputing a pan here would need
/// canvas bounds the reset click does not have, and would be a SECOND copy
/// of the framing rule that could drift from the one that opens a world.
///
/// ggo-ide's own `ResetView` instead assigns a fixed `ZOOM_INITIAL` plus
/// `initial_pan(CANVAS_DEFAULT_W, CANVAS_DEFAULT_H)` -- a hardcoded canvas
/// size, because its canvas was a fixed-size widget. This panel's canvas
/// fills a resizable dock, so "re-center on the real bounds" is the same
/// intent expressed against a variable-size canvas.
pub fn reset_camera() -> (f64, Option<[f64; 2]>) {
    (ZOOM_DEFAULT, None)
}

/// Grid line spacing, world px -- one tile, so the grid reads as a tile
/// guide at 1x. ggo-ide `pages::world::canvas::GRID_STEP_PX`.
pub const GRID_STEP_PX: f64 = 16.0;

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

/// Where a left-drag places the dragged entity/instance: the drag-start
/// position plus the world-space cursor delta, snapped to the tile grid
/// when the editor's snap toggle is on -- ggo-ide's `CanvasGesture::Drag`
/// move math (snap applies to the RESULT, not the delta, via
/// `drag_ops::snap_to_tile`).
pub fn dragged_pos(
    start_pos: [f64; 2],
    start_world: [f64; 2],
    world: [f64; 2],
    snap: bool,
) -> [f64; 2] {
    let mut pos = [
        start_pos[0] + world[0] - start_world[0],
        start_pos[1] + world[1] - start_world[1],
    ];
    if snap {
        pos = drag_ops::snap_to_tile(pos);
    }
    pos
}

/// The map cell a world-space point lands in, for a map whose top-left
/// pixel sits at `anchor` (world origin for a background slot,
/// `Transform.pos + (col, row) * TILE_PX` for a `Tilemap` entity).
///
/// Floors per axis, so a point ABOVE or LEFT of the anchor yields a
/// negative cell rather than clamping onto row/column 0 -- an off-map
/// click has to read as off-map, not as an edit of the first cell.
pub fn paint_cell_at(world: [f64; 2], anchor: [f64; 2]) -> (i32, i32) {
    let cell = |w: f64, a: f64| ((w - a) / TILE_PX as f64).floor() as i32;
    (cell(world[0], anchor[0]), cell(world[1], anchor[1]))
}

// -------------------------------------------------------- BGRA conversion

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
    build_image_cache_reusing(loads, &HashMap::new())
}

/// [`build_image_cache`] that keeps `previous`'s `RenderImage` for every
/// key still present, so a rebuild neither re-uploads unchanged images
/// nor mints a new atlas identity for them -- which is what lets the
/// panel retire images by key.
pub fn build_image_cache_reusing(
    loads: &[&AssetLoads],
    previous: &HashMap<usize, Arc<RenderImage>>,
) -> HashMap<usize, Arc<RenderImage>> {
    let mut cache = HashMap::new();
    for map in loads {
        for load in map.values() {
            let Loadable::Ready(img) = load else {
                continue;
            };
            let key = image_key(img);
            if let Some(existing) = previous.get(&key) {
                cache.insert(key, existing.clone());
            } else if let Some(render_image) = to_render_image(&img.rgba, img.w, img.h) {
                cache.insert(key, render_image);
            }
        }
    }
    cache
}

// ---------------------------------------------------------------- colors

fn color(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
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
    /// Draw the tile grid under the items ([`paint_grid`]).
    pub grid: bool,
    pub background: Hsla,
    pub text_color: Hsla,
    /// An in-flight rubber-band, `[x, y, w, h]` in world px.
    pub marquee: Option<[f64; 4]>,
    /// Paint mode's target image ([`image_key`]), which draws at full
    /// strength while everything else dims. `None` in entity mode.
    pub paint_focus: Option<usize>,
}

/// Opacity of the paint-mode wash: how much canvas background is laid over
/// the scene the brush is NOT editing.
const PAINT_DIM_ALPHA: f32 = 0.45;

/// The image an item draws, if it draws one -- the only [`DrawKind`] a
/// paint target can be.
fn item_image_key(item: &DrawItem) -> Option<usize> {
    match &item.kind {
        DrawKind::Image { image } => Some(image_key(image)),
        _ => None,
    }
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
            if scene.grid {
                paint_grid(&view, canvas_bounds, window);
            }
            for item in &scene.items {
                paint_item(scene, item, &view, canvas_bounds, window, cx);
            }
            // Dimming, wash-and-redraw rather than per-item opacity:
            // gpui's `with_element_opacity` is crate-private and
            // `paint_image` takes no alpha, so the only way to hold ONE
            // item at full strength is to fade the finished scene behind
            // it and draw it again on top.
            if let Some(focus) = scene.paint_focus {
                let mut wash = scene.background;
                wash.a = PAINT_DIM_ALPHA;
                window.paint_quad(fill(canvas_bounds, wash));
                for item in &scene.items {
                    if item_image_key(item) == Some(focus) {
                        paint_item(scene, item, &view, canvas_bounds, window, cx);
                    }
                }
            }
            paint_device_screen(scene, &view, canvas_bounds, window, cx);
            if let Some([x, y, w, h]) = scene.marquee {
                let b = item_bounds(&view, canvas_bounds.origin, x, y, w, h);
                window.paint_quad(outline(b, color(MARQUEE_COLOR), BorderStyle::default()));
            }
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
        DrawKind::Text { content } => {
            window.paint_quad(fill(b, gpui::rgba(0x00000066)));
            paint_world_text(content, b, view.zoom, scene.text_color, window, cx);
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
                let _ = window.paint_image(
                    b,
                    b,
                    Corners::default(),
                    render_image.clone(),
                    0,
                    false,
                    true,
                );
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

/// Defensive ceiling on grid lines per axis: a very low zoom over a very
/// wide canvas is a legitimate (if dense) case, but a degenerate `view`
/// must not spin the loop.
const GRID_MAX_LINES: usize = 4096;

const GRID_LINE_COLOR: u32 = 0x80808040;
/// The rubber-band outline.
const MARQUEE_COLOR: u32 = 0x88c0d0;

/// World-space coordinates of the grid lines a `w` x `h` px canvas shows
/// at `view`, as `(xs, ys)` -- ggo-ide's `draw_grid` walk, split out from
/// the painting so "which lines" is testable without a window.
pub fn grid_lines(view: &View, w: f64, h: f64) -> (Vec<f64>, Vec<f64>) {
    let top_left = drag_ops::screen_to_world(0.0, 0.0, view);
    let bottom_right = drag_ops::screen_to_world(w, h, view);
    (
        axis_lines(top_left[0], bottom_right[0]),
        axis_lines(top_left[1], bottom_right[1]),
    )
}

fn axis_lines(lo: f64, hi: f64) -> Vec<f64> {
    if !lo.is_finite() || !hi.is_finite() || hi < lo {
        return Vec::new();
    }
    let mut v = (lo / GRID_STEP_PX).floor() * GRID_STEP_PX;
    let mut out = Vec::new();
    while v <= hi && out.len() < GRID_MAX_LINES {
        out.push(v);
        v += GRID_STEP_PX;
    }
    out
}

/// The tile grid, under everything else. 1px quads rather than paths: the
/// lines are axis-aligned, which is exactly what `paint_quad` draws
/// cheapest (the `InstanceOrigin` crosshair does the same).
fn paint_grid(view: &View, canvas_bounds: Bounds<Pixels>, window: &mut Window) {
    let origin = canvas_bounds.origin;
    let (xs, ys) = grid_lines(
        view,
        f64::from(canvas_bounds.size.width),
        f64::from(canvas_bounds.size.height),
    );
    let stroke: Hsla = gpui::rgba(GRID_LINE_COLOR).into();
    for x in xs {
        let sx = to_screen(view, origin, x, 0.0).x;
        window.paint_quad(fill(
            bounds(point(sx, origin.y), size(px(1.), canvas_bounds.size.height)),
            stroke,
        ));
    }
    for y in ys {
        let sy = to_screen(view, origin, 0.0, y).y;
        window.paint_quad(fill(
            bounds(point(origin.x, sy), size(canvas_bounds.size.width, px(1.))),
            stroke,
        ));
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

/// Glyph cell of the engine's fixed 8x8 bitmap font, world px -- must
/// match worldlib's `TEXT_GLYPH_PX`, which sizes the Text item's
/// (hit-tested) bounding box.
const WORLD_GLYPH_PX: f64 = 8.0;

/// Paint a `DrawKind::Text` on the engine's glyph grid: one glyph per
/// 8-world-px cell, scaled with zoom and clipped to the item box, so the
/// painted text lines up with the bounding box the hit test uses. The
/// editor's UI font stands in for the device font (spec: text is
/// approximated), but each glyph's PLACEMENT is grid-exact.
fn paint_world_text(
    text: &str,
    b: Bounds<Pixels>,
    zoom: f64,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let cell = px((WORLD_GLYPH_PX * zoom) as f32);
    if f32::from(cell) <= 0.0 {
        return;
    }
    window.with_content_mask(Some(ContentMask { bounds: b }), |window| {
        for (row, line) in text.lines().enumerate() {
            let y = b.origin.y + cell * row as f32;
            for (col, ch) in line.chars().enumerate() {
                if ch.is_whitespace() {
                    continue;
                }
                let glyph: SharedString = ch.to_string().into();
                let run = TextRun {
                    len: glyph.len(),
                    font: window.text_style().font(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window.text_system().shape_line(glyph, cell, &[run], None);
                let _ = shaped.paint(
                    point(b.origin.x + cell * col as f32, y),
                    cell,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }
    });
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

    #[test]
    fn dragged_pos_applies_world_delta_from_the_drag_start() {
        // Start pos [4, 4], cursor went from world [10, 10] to [26, 13].
        assert_eq!(
            dragged_pos([4.0, 4.0], [10.0, 10.0], [26.0, 13.0], false),
            [20.0, 7.0]
        );
    }

    #[test]
    fn dragged_pos_snaps_the_result_not_the_delta() {
        // Off-grid start [4, 4] + delta [17, 0] = [21, 4] -> snapped [16, 0]:
        // the dragged item lands ON grid even from an off-grid start.
        assert_eq!(
            dragged_pos([4.0, 4.0], [10.0, 10.0], [27.0, 10.0], true),
            [16.0, 0.0]
        );
    }

    fn view_at(zoom: f64, pan: [f64; 2]) -> View {
        View {
            zoom,
            pan_x: pan[0],
            pan_y: pan[1],
            dpr: None,
        }
    }

    /// Reset hands back the default zoom and NO pan, and that pair is
    /// exactly what the first-layout path consumes -- so a reset frames
    /// the world the same way opening it does, by construction.
    #[test]
    fn reset_camera_reproduces_the_initial_framing() {
        let (zoom, pan) = reset_camera();
        assert_eq!(zoom, ZOOM_DEFAULT);
        assert_eq!(pan, None, "None pan is what re-triggers the centering");

        // What prepaint then computes, for a world whose camera sits at
        // the origin-centering screen rect.
        let world_center = camera_center([-DEVICE_SCREEN_W / 2.0, -DEVICE_SCREEN_H / 2.0]);
        let pan = centering_pan(400.0, 300.0, zoom, world_center);
        let screen = drag_ops::world_to_screen(0.0, 0.0, &view_at(zoom, pan));
        assert_eq!(screen, [200.0, 150.0], "the camera lands on canvas center");
    }

    #[test]
    fn grid_lines_cover_the_visible_world_rect_on_tile_boundaries() {
        // Identity view, 64x32 canvas: world [0, 64] x [0, 32].
        let (xs, ys) = grid_lines(&view_at(1.0, [0.0, 0.0]), 64.0, 32.0);
        assert_eq!(xs, vec![0.0, 16.0, 32.0, 48.0, 64.0]);
        assert_eq!(ys, vec![0.0, 16.0, 32.0]);
        assert!(
            xs.iter().chain(ys.iter()).all(|v| v % GRID_STEP_PX == 0.0),
            "every line is on a tile boundary"
        );
    }

    #[test]
    fn grid_lines_start_at_or_before_the_visible_edge_when_panned_off_grid() {
        // Pan by 4px: the world edge is at -4, so the first line is -16
        // (floor to the tile below), never inside the visible rect.
        let (xs, _) = grid_lines(&view_at(1.0, [4.0, 0.0]), 32.0, 32.0);
        assert_eq!(xs, vec![-16.0, 0.0, 16.0]);
        assert!(
            *xs.first().unwrap() <= -4.0,
            "the first line is at or before the visible edge"
        );
        assert!(
            *xs.last().unwrap() > 28.0 - GRID_STEP_PX,
            "and the last is within a tile of the far edge"
        );
    }

    #[test]
    fn grid_lines_scale_with_zoom_and_survive_degenerate_views() {
        // Zoomed 4x: the same canvas shows a quarter of the world, so a
        // quarter of the lines.
        let (xs, _) = grid_lines(&view_at(4.0, [0.0, 0.0]), 64.0, 64.0);
        assert_eq!(xs, vec![0.0, 16.0]);
        // Zero zoom would divide by zero -- no lines, no panic, no spin.
        let (xs, ys) = grid_lines(&view_at(0.0, [0.0, 0.0]), 64.0, 64.0);
        assert!(xs.is_empty() && ys.is_empty());
        // And a pathologically low zoom is capped rather than unbounded.
        let (xs, _) = grid_lines(&view_at(1e-9, [0.0, 0.0]), 4096.0, 4096.0);
        assert_eq!(xs.len(), GRID_MAX_LINES);
    }

    #[test]
    fn paint_cell_at_floors_into_the_anchored_tile_grid() {
        assert_eq!(paint_cell_at([0.0, 0.0], [0.0, 0.0]), (0, 0));
        assert_eq!(paint_cell_at([31.9, 16.0], [0.0, 0.0]), (1, 1));
        // Above the anchor is a NEGATIVE cell, not cell 0: a map painted
        // through a floor-to-zero cast would take an off-map click as an
        // edit of its first row.
        assert_eq!(paint_cell_at([-0.1, 0.0], [0.0, 0.0]), (-1, 0));
        assert_eq!(paint_cell_at([40.0, 40.0], [32.0, 32.0]), (0, 0));
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
