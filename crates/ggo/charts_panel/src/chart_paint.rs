//! The gpui side of chart rendering: resolves a [`ChartScene`]'s symbolic
//! colors against the active theme and turns each [`Primitive`] into a
//! `window.paint_*` call. Deliberately decision-free -- every layout,
//! tick, hit-test and formatting choice already happened in `chart_geom`,
//! which is why that module is testable without a window and this one
//! needs no tests of its own beyond the palette conversion below (the
//! same split `ggo_world_panel` draws between `build_draw_list` and
//! `canvas::paint_scene`).

use gpui::{
    App, BorderStyle, Bounds, ContentMask, Hsla, Path as GpuiPath, Pixels, Point, SharedString,
    TextAlign, TextRun, Window, bounds, fill, outline, point, px, size,
};
use ui::prelude::*;

use crate::chart_geom::{ChartColor, ChartScene, Primitive, Rect, Rgb, TextAnchor, TextBaseline};

/// The theme-derived colors a scene's symbolic [`ChartColor`]s resolve
/// against, sampled once per render.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub grid: Hsla,
    pub text: Hsla,
    pub surface: Hsla,
    pub budget: Hsla,
}

impl Palette {
    pub fn from_theme(cx: &App) -> Self {
        let colors = cx.theme().colors();
        Self {
            grid: colors.border_variant,
            text: colors.text_muted,
            surface: colors.editor_background,
            // The dashed reference line reads as "you are over your
            // budget" -- ggo-ide uses iced's `palette.danger.base.color`
            // for it, and this is that role in Zed's theme.
            budget: cx.theme().status().error,
        }
    }
}

/// `StackedArea.tsx`'s band fill: 72% series, 28% surface. ggo-ide's port
/// notes it blends in linear RGB rather than oklab; this blends in HSLA
/// component space for the same "close enough, not colorimetric"
/// trade-off, which is all the softening is for (keeping large filled
/// regions quiet).
const BAND_FILL_SERIES_WEIGHT: f32 = 0.72;

fn hsla_from_rgb(Rgb(hex): Rgb) -> Hsla {
    gpui::rgb(hex).into()
}

fn mix(a: Hsla, b: Hsla, weight_a: f32) -> Hsla {
    let w = weight_a;
    let inv = 1.0 - w;
    // Blend through RGBA rather than HSLA components: averaging two hues
    // numerically can swing through unrelated colors (350 deg and 10 deg
    // average to 180 deg, i.e. red + red = cyan).
    let ra: gpui::Rgba = a.into();
    let rb: gpui::Rgba = b.into();
    gpui::Rgba {
        r: ra.r * w + rb.r * inv,
        g: ra.g * w + rb.g * inv,
        b: ra.b * w + rb.b * inv,
        a: ra.a * w + rb.a * inv,
    }
    .into()
}

fn resolve(color: ChartColor, palette: &Palette) -> Hsla {
    match color {
        ChartColor::Grid => palette.grid,
        ChartColor::Text => palette.text,
        ChartColor::Surface => palette.surface,
        ChartColor::Budget => palette.budget,
        ChartColor::Series(rgb) => hsla_from_rgb(rgb),
        ChartColor::SeriesAlpha(rgb, alpha) => hsla_from_rgb(rgb).opacity(alpha),
        ChartColor::Band(rgb) => mix(hsla_from_rgb(rgb), palette.surface, BAND_FILL_SERIES_WEIGHT),
    }
}

fn to_bounds(rect: Rect, origin: Point<Pixels>) -> Bounds<Pixels> {
    bounds(
        point(origin.x + px(rect.x), origin.y + px(rect.y)),
        size(px(rect.w), px(rect.h)),
    )
}

fn to_point(p: (f32, f32), origin: Point<Pixels>) -> Point<Pixels> {
    point(origin.x + px(p.0), origin.y + px(p.1))
}

/// Paints a built scene into `canvas_bounds`, clipped to it (a series
/// value that maps outside the plot -- possible only for a degenerate
/// scale -- must not bleed over the neighboring chart).
pub fn paint_scene(
    scene: &ChartScene,
    palette: &Palette,
    canvas_bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    window.with_content_mask(
        Some(ContentMask {
            bounds: canvas_bounds,
        }),
        |window| {
            window.paint_quad(fill(canvas_bounds, palette.surface));
            for primitive in &scene.primitives {
                paint_primitive(primitive, palette, canvas_bounds.origin, window, cx);
            }
        },
    );
}

fn paint_primitive(
    primitive: &Primitive,
    palette: &Palette,
    origin: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    match primitive {
        Primitive::Quad { rect, color } => {
            window.paint_quad(fill(to_bounds(*rect, origin), resolve(*color, palette)));
        }
        // `width` is ignored: gpui's `outline` helper always draws a 1px
        // border, and the only outline any scene emits is the readout
        // box's, whose width IS 1.0 (`TOOLTIP_BORDER_WIDTH`). A thicker
        // outline would need four quads; nothing asks for one.
        Primitive::Outline {
            rect,
            width: _,
            color,
        } => {
            window.paint_quad(outline(
                to_bounds(*rect, origin),
                resolve(*color, palette),
                BorderStyle::default(),
            ));
        }
        Primitive::Segment {
            from,
            to,
            width,
            color,
            dash,
        } => {
            let color = resolve(*color, palette);
            let a = to_point(*from, origin);
            let b = to_point(*to, origin);
            match dash {
                None => {
                    if let Some(path) = line_path(a, b, *width) {
                        window.paint_path(path, color);
                    }
                }
                Some((on, off)) => {
                    for (s, e) in dash_segments(a, b, *on, *off) {
                        if let Some(path) = line_path(s, e, *width) {
                            window.paint_path(path, color);
                        }
                    }
                }
            }
        }
        Primitive::Polyline {
            points,
            width,
            color,
        } => {
            if let Some(path) = ribbon_path(points, origin, *width) {
                window.paint_path(path, resolve(*color, palette));
            }
        }
        Primitive::Polygon { points, color } => {
            if let Some(path) = polygon_path(points, origin) {
                window.paint_path(path, resolve(*color, palette));
            }
        }
        Primitive::Text {
            at,
            content,
            size,
            anchor,
            baseline,
            color,
        } => paint_text(
            content,
            to_point(*at, origin),
            *size,
            *anchor,
            *baseline,
            resolve(*color, palette),
            window,
            cx,
        ),
    }
}

/// A `width`-px filled quad along `p0 -> p1` -- gpui paths are filled
/// regions, so a stroked line is a thin polygon (same helper
/// `ggo_world_panel::canvas` uses). `None` for a degenerate segment.
fn line_path(p0: Point<Pixels>, p1: Point<Pixels>, width: f32) -> Option<GpuiPath<Pixels>> {
    let (nx, ny) = normal(p0, p1, width)?;
    let mut path = GpuiPath::new(point(p0.x + px(nx), p0.y + px(ny)));
    path.line_to(point(p1.x + px(nx), p1.y + px(ny)));
    path.line_to(point(p1.x - px(nx), p1.y - px(ny)));
    path.line_to(point(p0.x - px(nx), p0.y - px(ny)));
    Some(path)
}

/// Half-width perpendicular offset of `p0 -> p1`, or `None` when the
/// segment has no length to be perpendicular to.
fn normal(p0: Point<Pixels>, p1: Point<Pixels>, width: f32) -> Option<(f32, f32)> {
    let dx = f32::from(p1.x - p0.x);
    let dy = f32::from(p1.y - p0.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len < f32::EPSILON {
        return None;
    }
    Some((-dy / len * width / 2.0, dx / len * width / 2.0))
}

/// One path covering every segment of a polyline, as a stroked ribbon:
/// two triangles per segment, pushed into a single `Path` so a
/// thousand-point series is one draw rather than a thousand.
/// `push_triangle`'s `st` coordinates are the solid-fill ones
/// `Path::line_to` itself uses.
fn ribbon_path(
    points: &[(f32, f32)],
    origin: Point<Pixels>,
    width: f32,
) -> Option<GpuiPath<Pixels>> {
    if points.len() < 2 {
        return None;
    }
    let solid = (point(0., 1.), point(0., 1.), point(0., 1.));
    let mut path: Option<GpuiPath<Pixels>> = None;
    for pair in points.windows(2) {
        let a = to_point(pair[0], origin);
        let b = to_point(pair[1], origin);
        let Some((nx, ny)) = normal(a, b, width) else {
            continue;
        };
        let a0 = point(a.x + px(nx), a.y + px(ny));
        let a1 = point(a.x - px(nx), a.y - px(ny));
        let b0 = point(b.x + px(nx), b.y + px(ny));
        let b1 = point(b.x - px(nx), b.y - px(ny));
        let path = path.get_or_insert_with(|| GpuiPath::new(a0));
        path.push_triangle((a0, b0, b1), solid);
        path.push_triangle((a0, b1, a1), solid);
    }
    path
}

/// A closed filled polygon (a stacked band) as a triangle fan from its
/// first vertex -- what `Path::line_to` builds internally.
fn polygon_path(points: &[(f32, f32)], origin: Point<Pixels>) -> Option<GpuiPath<Pixels>> {
    let (first, rest) = points.split_first()?;
    if rest.len() < 2 {
        return None;
    }
    let mut path = GpuiPath::new(to_point(*first, origin));
    for p in rest {
        path.line_to(to_point(*p, origin));
    }
    path.line_to(to_point(*first, origin));
    Some(path)
}

/// `(on, off)`-patterned sub-segments of `a -> b`, for the dashed budget
/// line. Pure geometry; walks in `on + off` strides and clips the final
/// dash to the segment's end.
fn dash_segments(
    a: Point<Pixels>,
    b: Point<Pixels>,
    on: f32,
    off: f32,
) -> Vec<(Point<Pixels>, Point<Pixels>)> {
    let dx = f32::from(b.x - a.x);
    let dy = f32::from(b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    let period = on + off;
    if len < f32::EPSILON || on <= 0.0 || period <= 0.0 {
        return Vec::new();
    }
    let (ux, uy) = (dx / len, dy / len);
    let mut out = Vec::new();
    let mut t = 0.0f32;
    while t < len {
        let end = (t + on).min(len);
        out.push((
            point(a.x + px(ux * t), a.y + px(uy * t)),
            point(a.x + px(ux * end), a.y + px(uy * end)),
        ));
        t += period;
    }
    out
}

/// One shaped line of text, anchored per the primitive's alignment. gpui
/// shapes left-to-right from an origin, so center/right anchoring and
/// vertical centering are applied by shifting that origin by the measured
/// line width / font size.
#[allow(clippy::too_many_arguments)]
fn paint_text(
    content: &str,
    at: Point<Pixels>,
    font_size: f32,
    anchor: TextAnchor,
    baseline: TextBaseline,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    // `shape_line` panics on a newline; no chart label ever contains one,
    // but take the first line defensively (same guard
    // `ggo_world_panel::canvas::paint_label` uses).
    let first_line = content.lines().next().unwrap_or("");
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
        .shape_line(text, px(font_size), &[run], None);
    let width = line.width;
    let x = match anchor {
        TextAnchor::Left => at.x,
        TextAnchor::Center => at.x - width / 2.,
        TextAnchor::Right => at.x - width,
    };
    let y = match baseline {
        TextBaseline::Top => at.y,
        TextBaseline::Middle => at.y - px(font_size) / 2.,
    };
    let _ = line.paint(
        point(x, y),
        px(font_size),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_segments_walk_the_line_in_on_off_strides() {
        let a = point(px(0.), px(0.));
        let b = point(px(20.), px(0.));
        let dashes = dash_segments(a, b, 6.0, 4.0);
        // Period 10 over length 20 -> dashes starting at 0 and 10.
        assert_eq!(dashes.len(), 2);
        assert_eq!(f32::from(dashes[0].0.x), 0.0);
        assert_eq!(f32::from(dashes[0].1.x), 6.0);
        assert_eq!(f32::from(dashes[1].0.x), 10.0);
        assert_eq!(f32::from(dashes[1].1.x), 16.0);
    }

    /// The last dash is clipped to the segment's end rather than
    /// overshooting it.
    #[test]
    fn dash_segments_clips_the_final_dash() {
        let a = point(px(0.), px(0.));
        let b = point(px(13.), px(0.));
        let dashes = dash_segments(a, b, 6.0, 4.0);
        assert_eq!(dashes.len(), 2);
        assert_eq!(f32::from(dashes[1].1.x), 13.0);
    }

    #[test]
    fn dash_segments_of_a_degenerate_line_is_empty() {
        let p = point(px(5.), px(5.));
        assert!(dash_segments(p, p, 6.0, 4.0).is_empty());
        assert!(dash_segments(p, point(px(50.), px(5.)), 0.0, 4.0).is_empty());
    }

    #[test]
    fn normal_is_perpendicular_and_half_the_stroke_width() {
        let (nx, ny) = normal(point(px(0.), px(0.)), point(px(10.), px(0.)), 2.0).unwrap();
        assert_eq!((nx, ny), (0.0, 1.0));
        assert!(normal(point(px(1.), px(1.)), point(px(1.), px(1.)), 2.0).is_none());
    }

    /// Mixing two reds must stay red -- the reason `mix` blends through
    /// RGBA rather than averaging hue numerically.
    #[test]
    fn mix_blends_through_rgba_not_hue() {
        let a: Hsla = gpui::rgb(0xff0000).into();
        let b: Hsla = gpui::rgb(0xee1111).into();
        let blended: gpui::Rgba = mix(a, b, 0.5).into();
        assert!(blended.r > 0.9, "a red/red blend must stay red");
        assert!(blended.g < 0.1);
        assert!(blended.b < 0.1);
    }
}
