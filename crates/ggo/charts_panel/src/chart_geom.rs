//! Pure chart geometry + a renderer-independent scene description.
//!
//! Everything here is plain `f32`/`String`/enum -- no gpui types at all --
//! so the layout, tick, hit-test and hover-readout math is unit-testable
//! without a window, and `render`'s paint closure stays a thin
//! `Primitive` -> `window.paint_*` translation (the same split
//! `ggo_world_panel` uses between `build_draw_list` and `paint_scene`).
//!
//! The math is ported from the Tauri/Iced IDE's chart widgets
//! (`tools/ggo-ide/src/charts/{line,histogram,stacked,frame_draw}.rs`) --
//! margins, tick placement, bin edges, x-tick striding, tooltip layout and
//! value formatting all mirror those files, which in turn ported
//! `LineChart.tsx`/`StackedArea.tsx`/`Histogram.tsx`. The iced
//! `canvas::Program` shells (hover/drag/zoom state machines, `canvas::Cache`
//! invalidation) are deliberately NOT ported: gpui re-renders from view
//! state, so this crate keeps hover in `ChartsPanel` and rebuilds the scene
//! per render.
//!
//! `LinearScale`/`nice_step` are NOT re-derived here -- they come from
//! `ggo_worldlib::charts::scale`, the shared crate ggo-ide's charts use
//! too, so both front ends produce byte-identical tick sets.
//!
//! Deliberate divergences from the iced originals, all noted in this
//! task's report:
//! * a legend band (swatch + series name above the plot). The iced charts
//!   have none -- series names only surface in the hover tooltip, and
//!   `StackedArea.tsx`'s direct end-of-band labels were skipped by that
//!   port -- but a 360px-wide dock panel needs the series named without
//!   requiring a hover, so [`legend_layout`] is new here.
//!
//! F5.4 R5 closed the two that used to be listed here: `line.rs`'s
//! `zoom_domain` (drag x-zoom + double-click reset) and its historic
//! overlay are both ported now -- see [`zoom_domain`], [`OverlaySpec`] and
//! [`ChartView`]. Where iced keeps hover/drag/zoom inside each
//! `canvas::Program`'s own retained `State`, this crate has no retained
//! per-widget state at all: the panel owns it and hands a [`ChartView`]
//! down per render, which is what keeps this layer pure.

use ggo_worldlib::charts::scale::{LinearScale, nice_step};

// ------------------------------------------------------------- primitives

/// A packed `0xRRGGBB` series color. The pure layer never sees a theme
/// color: those arrive as [`ChartColor`] variants the painter resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u32);

/// A primitive's color, resolved by the painter. Theme-derived roles
/// (`Grid`/`Text`/`Surface`/`Budget`) stay symbolic so the geometry layer
/// has no opinion about light/dark; only series colors are concrete.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChartColor {
    /// Gridlines, the x baseline, the hover crosshair.
    Grid,
    /// Tick labels, captions, tooltip text/border.
    Text,
    /// The chart's own background (tooltip fill, stacked-band separators).
    Surface,
    /// The dashed budget reference line.
    Budget,
    /// A series' own color, at full opacity.
    Series(Rgb),
    /// The theme's primary accent -- histogram bars. `Histogram.tsx` (and
    /// `histogram.rs`) fill bars from `palette.primary.base.color` and the
    /// widget takes no series color at all, unlike the line/stacked
    /// charts, so this stays theme-derived rather than borrowing the
    /// metric's fixed series hue.
    Accent,
    /// The theme's accent at reduced opacity -- a histogram bar's resting
    /// state, so the hovered bar's full-opacity redraw reads as a "lift"
    /// (`Histogram.tsx`'s `opacity={hover() === i() ? 1 : 0.85}`).
    AccentAlpha(f32),
    /// A series' color softened toward the surface -- `StackedArea.tsx`'s
    /// `color-mix(in oklab, <series> 72%, var(--surface))` band fill,
    /// blended by the painter (which is the only layer that knows what
    /// "surface" is).
    Band(Rgb),
    /// A historic-overlay line: the text color at the given opacity, which
    /// is the ONLY thing distinguishing one prior run from another
    /// (`line.rs`'s `Color { a: text_color.a * hs.opacity, ..text_color }`).
    /// Grey rather than the series' own hue on purpose -- an overlay that
    /// carried the live color would read as more data of the same run.
    Historic(f32),
    /// The translucent band under an in-progress drag-zoom --
    /// `line.rs`'s `DRAG_OVERLAY_ALPHA` over the budget/danger color.
    Selection,
}

/// Horizontal text anchoring for a [`Primitive::Text`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAnchor {
    Left,
    Center,
    Right,
}

/// Vertical text anchoring: `Top` puts `y` at the text's top edge,
/// `Middle` centers the line on `y` (y-axis tick labels, which sit ON
/// their gridline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextBaseline {
    Top,
    Middle,
}

/// One paintable item, in canvas-local coordinates (origin at the chart
/// canvas's top-left).
#[derive(Debug, Clone, PartialEq)]
pub enum Primitive {
    /// A filled rectangle (histogram bars, legend swatches, tooltip fill).
    Quad { rect: Rect, color: ChartColor },
    /// A rectangle outline (the tooltip box's border).
    Outline {
        rect: Rect,
        width: f32,
        color: ChartColor,
    },
    /// A straight segment (gridlines, baseline, crosshair, budget line).
    /// `dash` is `Some((on, off))` for the dashed budget line.
    Segment {
        from: (f32, f32),
        to: (f32, f32),
        width: f32,
        color: ChartColor,
        dash: Option<(f32, f32)>,
    },
    /// A stroked open polyline (a line-chart series, a stacked-band
    /// separator).
    Polyline {
        points: Vec<(f32, f32)>,
        width: f32,
        color: ChartColor,
    },
    /// A filled closed polygon (a stacked band).
    Polygon {
        points: Vec<(f32, f32)>,
        color: ChartColor,
    },
    Text {
        at: (f32, f32),
        content: String,
        size: f32,
        anchor: TextAnchor,
        baseline: TextBaseline,
        color: ChartColor,
    },
}

/// An axis-aligned rectangle in canvas-local coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.right() && py >= self.y && py <= self.bottom()
    }

    /// A non-degenerate plot area -- charts early-out on anything else
    /// (the iced ports' `plot.width <= 0.0 || plot.height <= 0.0` guard).
    pub fn is_drawable(&self) -> bool {
        self.w > 0.0 && self.h > 0.0
    }
}

// ------------------------------------------------------------------ layout

/// Plot-area insets, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

/// `line.rs`'s `MARGIN_*` (itself `LineChart.tsx`'s `m` memo proportions).
pub const LINE_MARGINS: Margins = Margins {
    top: 16.0,
    right: 24.0,
    bottom: 34.0,
    left: 56.0,
};

/// `stacked.rs`'s `MARGIN_*`: top/bottom/left from `StackedArea.tsx`'s `m`
/// memo, right narrowed to `LineChart`'s 24 (that port skipped the direct
/// end-of-band labels its 108px right gutter existed for, and so does
/// this one).
pub const STACKED_MARGINS: Margins = Margins {
    top: 16.0,
    right: 24.0,
    bottom: 34.0,
    left: 60.0,
};

/// `histogram.rs`'s `MARGIN_*` (`Histogram.tsx`'s `m` memo).
pub const HISTOGRAM_MARGINS: Margins = Margins {
    top: 14.0,
    right: 10.0,
    bottom: 32.0,
    left: 42.0,
};

/// The plot area for a canvas of `(w, h)` under `m`, shifted down by
/// `legend_h` (0 when the chart has no legend band). Width/height clamp at
/// zero so a canvas narrower than its own margins yields an empty --
/// rather than negative -- rect, same as the iced ports' `.max(0.0)`.
pub fn plot_rect(size: (f32, f32), m: Margins, legend_h: f32) -> Rect {
    Rect {
        x: m.left,
        y: m.top + legend_h,
        w: (size.0 - m.left - m.right).max(0.0),
        h: (size.1 - m.top - m.bottom - legend_h).max(0.0),
    }
}

// ------------------------------------------------------------------ scales

/// Roughly how many y ticks a line/stacked chart targets --
/// `frame_draw::Y_TICK_TARGET`, i.e. scale.ts's `ticksFor` default count.
pub const Y_TICK_TARGET: usize = 5;

/// `histogram.rs`'s `Y_TICK_TARGET`: fewer, whole-number ticks, since a
/// fractional frame count on the y-axis would be nonsense.
pub const HISTOGRAM_Y_TICK_TARGET: usize = 4;

/// `histogram.rs`'s `HISTOGRAM_BIN_TARGET` (`Histogram.tsx`'s hardcoded
/// ~20 bins).
pub const HISTOGRAM_BIN_TARGET: usize = 20;

/// The full x-domain of a shared frame axis: first/last of `xs`.
/// `(0.0, 1.0)` when empty, matching `line.rs`'s `full_x_domain`.
pub fn full_x_domain(xs: &[f32]) -> (f32, f32) {
    match (xs.first(), xs.last()) {
        (Some(&a), Some(&b)) => (a, b),
        _ => (0.0, 1.0),
    }
}

/// x maps left-to-right across the plot.
pub fn x_scale(plot: Rect, domain: (f32, f32)) -> LinearScale {
    LinearScale {
        domain,
        range: (plot.x, plot.right()),
    }
}

/// The x-domain a chart is CURRENTLY showing: the zoom window when one is
/// set, else the whole data domain.
///
/// The one place that choice is made. [`frame_at`] and every scene builder
/// resolve their x through [`x_scale_for`], which resolves through this --
/// R4's concern (1), which is that a zoom applied in the paint path but not
/// in the hit-test path puts a click on a different frame from the
/// crosshair over it. `the_click_and_the_hover_readout_resolve_the_same_
/// sample` sweeps a zoomed chart for exactly that.
pub fn x_domain(spec: &ChartSpec, view: &ChartView) -> (f32, f32) {
    view.zoom.unwrap_or_else(|| full_x_domain(&spec.x))
}

/// [`x_scale`] over [`x_domain`] -- what both the painter and the hit-test
/// call, so neither can be zoomed without the other.
pub fn x_scale_for(spec: &ChartSpec, plot: Rect, view: &ChartView) -> LinearScale {
    x_scale(plot, x_domain(spec, view))
}

/// Narrowest x-domain (in x units, i.e. frames) a drag may select --
/// `line.rs`'s `MIN_ZOOM_DOMAIN_WIDTH`, which stops a near-zero drag
/// zooming to a degenerate one-sample domain.
pub const MIN_ZOOM_DOMAIN_WIDTH: f32 = 2.0;

/// A press/release pair whose x endpoints are closer than this (px) is a
/// CLICK, not a drag -- `line.rs`'s `DRAG_MIN_PX`. Below it the pointer
/// just jittered, and the brief's "a drag shorter than a threshold is a
/// click, not a zoom-to-nothing" is this constant: without it, a click on
/// one of the four frame-selectable charts would zoom to a two-frame
/// window instead of opening the inspect pane.
pub const DRAG_MIN_PX: f32 = 3.0;

/// A left-drag from pixel `px_a` to `px_b` (in `scale`'s range space) ->
/// the new x-domain, clamped into `full_domain` and widened to
/// [`MIN_ZOOM_DOMAIN_WIDTH`] if the drag was too narrow. Ported from
/// `line.rs`'s `zoom_domain` verbatim, including the "slide the window
/// back inside rather than shrink it" rule, so a drag that starts or ends
/// past the data edge still zooms.
pub fn zoom_domain(
    px_a: f32,
    px_b: f32,
    scale: &LinearScale,
    full_domain: (f32, f32),
) -> (f32, f32) {
    let da = scale.invert(px_a);
    let db = scale.invert(px_b);
    let (mut lo, mut hi) = if da <= db { (da, db) } else { (db, da) };

    if hi - lo < MIN_ZOOM_DOMAIN_WIDTH {
        let mid = (lo + hi) / 2.0;
        lo = mid - MIN_ZOOM_DOMAIN_WIDTH / 2.0;
        hi = mid + MIN_ZOOM_DOMAIN_WIDTH / 2.0;
    }

    let (fd0, fd1) = full_domain;
    if lo < fd0 {
        hi += fd0 - lo;
        lo = fd0;
    }
    if hi > fd1 {
        lo -= hi - fd1;
        hi = fd1;
    }
    (lo.max(fd0), hi.min(fd1))
}

/// The "nice" tick at or above `max` -- the y-domain top every chart in
/// the iced port uses (`probe.ticks(..).next_back()`, falling back to 1.0).
/// `integer` selects `ticks_integer` (the histogram's whole-number step).
pub fn nice_top(max: f32, target: usize, integer: bool) -> f32 {
    let probe = LinearScale {
        domain: (0.0, max),
        range: (0.0, 1.0),
    };
    let ticks = if integer {
        probe.ticks_integer(target)
    } else {
        probe.ticks(target)
    };
    ticks.into_iter().next_back().unwrap_or(1.0)
}

/// A zero-based y-scale topping out at [`nice_top`], with an INVERTED
/// range so larger values draw higher (smaller y) -- every chart's
/// `y_scale` in the iced port.
pub fn y_scale(plot: Rect, max: f32, target: usize, integer: bool) -> LinearScale {
    LinearScale {
        domain: (0.0, nice_top(max, target, integer)),
        range: (plot.bottom(), plot.y),
    }
}

// --------------------------------------------------------------- hit-test

/// Cursor pixel `px` -> the index into `xs` whose mapped position is
/// closest -- `line.rs`'s `nearest_x`, verbatim. `xs` need not be evenly
/// spaced (a run with ignored frames leaves gaps); ties go to the earlier
/// index. Returns `None` (rather than `line.rs`'s "callers must not pass
/// an empty slice" contract) for an empty axis, so a no-sample chart's
/// hover path is a plain `let ... else` instead of a precondition.
pub fn nearest_x(px: f32, xs: &[f32], scale: &LinearScale) -> Option<usize> {
    let mut best = None;
    let mut best_dist = f32::INFINITY;
    for (i, &x) in xs.iter().enumerate() {
        let dist = (scale.map(x) - px).abs();
        if dist < best_dist {
            best_dist = dist;
            best = Some(i);
        }
    }
    best
}

/// Which frame a click at canvas-local `point` on a chart of `spec`
/// drawn at `size` lands on -- the frame NUMBER out of [`ChartSpec::x`],
/// not an index into it, because that is what the profile rows are keyed
/// by. `None` for a click outside the plot area (the margins, the legend
/// band, the x-axis caption strip) and for a chart with no frame axis at
/// all (a histogram plots a distribution -- its x is empty, so there is
/// no frame under the cursor to name).
///
/// Pure, and deliberately NOT a second geometry: it resolves through the
/// same [`plot_for`] and the same [`x_scale_for`] that
/// [`build_chart_scene`] paints the points with, so a click and the hover
/// readout under the same cursor can never disagree about which sample is
/// there (`the_click_and_the_hover_readout_resolve_the_same_sample` is
/// what holds that). That is why it takes the whole [`ChartView`] rather
/// than just a zoom: R5 threaded the domain through `x_scale_for`, which
/// both this and the painter go through, so there is no way to zoom one
/// without the other.
///
/// Ties go to the earlier frame ([`nearest_x`]'s rule): a click exactly
/// between two samples selects the left one, deterministically.
pub fn frame_at(
    spec: &ChartSpec,
    size: (f32, f32),
    point: (f32, f32),
    view: &ChartView,
) -> Option<i64> {
    // The SAME two preconditions `build_chart_scene` early-outs on
    // (`!spec.has_data() || !plot.is_drawable()`), and `has_data` is here
    // rather than left to the caller for a reason: a `Line` spec with an
    // x-axis but no series paints nothing and produces no readout, yet
    // `nearest_x` would happily name a frame for it -- a click resolving
    // a frame on a blank canvas, with no crosshair to agree with.
    // Nothing in `chart_set` can produce such a spec today
    // (`every_produced_chart_has_data` pins that), but that invariant
    // lives in another module and R5 builds `ChartSpec`s of its own for
    // historic overlays, so the function that depends on it enforces it.
    let plot = plot_for(spec, size);
    if !spec.has_data() || !plot.is_drawable() || !plot.contains(point.0, point.1) {
        return None;
    }
    let xs = x_scale_for(spec, plot, view);
    let idx = nearest_x(point.0, &spec.x, &xs)?;
    spec.x.get(idx).map(|&x| x.round() as i64)
}

/// The histogram slot (bin) pixel `px` falls into -- `histogram.rs`'s
/// `slot_index`, clamped into `[0, nbins)` the same way `computeBins`'s
/// own count-assignment loop clamps out-of-range values into the last bin.
pub fn slot_index(px: f32, x0: f32, slot_w: f32, nbins: usize) -> usize {
    if nbins == 0 || slot_w <= 0.0 {
        return 0;
    }
    let idx = ((px - x0) / slot_w).floor();
    idx.clamp(0.0, (nbins - 1) as f32) as usize
}

// ------------------------------------------------------------------ ticks

/// `frame_draw::X_TICK_STEP_DIVISOR` -- `LineChart.tsx`'s/`StackedArea.tsx`'s
/// own `xTickIdx` divisor, not this port's choice.
const X_TICK_STEP_DIVISOR: f64 = 7.0;

/// Which indices into a shared x-axis of `count` entries get a tick label
/// -- `frame_draw::x_tick_indices`, verbatim.
pub fn x_tick_indices(count: usize) -> Vec<usize> {
    if count <= 1 {
        return vec![0];
    }
    let step = (nice_step((count - 1) as f64 / X_TICK_STEP_DIVISOR).round() as usize).max(1);
    (0..count).step_by(step).collect()
}

/// Which bin-EDGE indices (`0..=nbins`) get an x tick label --
/// `histogram.rs`'s `edge_marks` (`Histogram.tsx`'s `xTicks` memo): first,
/// roughly the middle, and last edge, deduped so a tiny bin count doesn't
/// repeat an edge.
pub fn edge_marks(nbins: usize) -> Vec<usize> {
    let raw = if nbins <= 4 {
        vec![0, nbins]
    } else {
        vec![0, (nbins as f32 / 2.0).round() as usize, nbins]
    };
    let mut out = Vec::with_capacity(raw.len());
    for m in raw {
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

// -------------------------------------------------------------- formatting

const TICK_INT_EPS: f64 = 1e-6;
const THOUSANDS_GROUP: usize = 3;
const COMPACT_MILLION: f64 = 1_000_000.0;
const COMPACT_THOUSAND: f64 = 1_000.0;

/// Axis tick label -- `frame_draw::format_tick`, verbatim: whole numbers
/// go through `fmt.ts`'s `fmtCompact` (`"20k"`, `"1.2M"`), anything
/// fractional prints via Rust's shortest round-tripping `Display`
/// (`"0.001"`, `"2.5"`).
pub fn format_tick(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let rounded = v.round();
    if (v - rounded).abs() < TICK_INT_EPS {
        compact(rounded)
    } else {
        format!("{v}")
    }
}

fn compact(n: f64) -> String {
    let a = n.abs();
    if a >= COMPACT_MILLION {
        format!("{}M", trim1(n / COMPACT_MILLION))
    } else if a >= COMPACT_THOUSAND {
        format!("{}k", trim1(n / COMPACT_THOUSAND))
    } else {
        trim1(n)
    }
}

fn trim1(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// Exact, comma-grouped integer -- `frame_draw::with_thousands`
/// (`fmt.ts`'s `fmtInt`). Every hover-readout VALUE uses this, not
/// [`format_tick`]: the iced tooltips are exact, only the axes are
/// compacted.
pub fn with_thousands(n: i64) -> String {
    let negative = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / THOUSANDS_GROUP);
    for (i, c) in digits.chars().rev().enumerate() {
        if i > 0 && i % THOUSANDS_GROUP == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let mut out: String = grouped.chars().rev().collect();
    if negative {
        out.insert(0, '-');
    }
    out
}

// ------------------------------------------------------------------- bins

/// One histogram bin: half-open `[lo, hi)` plus its count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bin {
    pub lo: f32,
    pub hi: f32,
    pub count: u32,
}

/// Auto-bins `values` into `bin_count`-ish nice-width bins --
/// `histogram.rs`'s `bins`, verbatim (itself `Histogram.tsx`'s
/// `computeBins`). Non-finite values are dropped first: Rust's
/// float-to-`usize` cast saturates `NaN` to `0`, which would otherwise
/// silently inflate bin 0.
pub fn bins(values: &[f32], bin_count: usize) -> Vec<Bin> {
    let values: Vec<f32> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if values.is_empty() {
        return Vec::new();
    }
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    let mut all_int = true;
    for &v in &values {
        lo = lo.min(v);
        hi = hi.max(v);
        if v.fract() != 0.0 {
            all_int = false;
        }
    }
    let range = hi - lo;
    let mut step = if range == 0.0 {
        1.0
    } else {
        nice_step(range as f64 / bin_count.max(1) as f64) as f32
    };
    if all_int && step.fract() != 0.0 {
        step = step.ceil();
    }
    let start = (lo / step).floor() * step;
    let nbins = (((hi - start) / step).floor() as usize + 1).max(1);
    let mut counts = vec![0u32; nbins];
    for &v in &values {
        let idx = (((v - start) / step).floor() as usize).min(nbins - 1);
        counts[idx] += 1;
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(i, count)| {
            let lo_edge = start + i as f32 * step;
            Bin {
                lo: lo_edge,
                hi: lo_edge + step,
                count,
            }
        })
        .collect()
}

/// A bin's readout title -- `histogram.rs`'s `bin_label`: a single value
/// when the step is 1, else a `lo`–`hi` range, both ends exact
/// ([`with_thousands`], not [`format_tick`]).
pub fn bin_label(bin: &Bin) -> String {
    const UNIT_STEP_EPS: f32 = 1e-6;
    if ((bin.hi - bin.lo) - 1.0).abs() < UNIT_STEP_EPS {
        with_thousands(bin.lo.round() as i64)
    } else {
        format!(
            "{}\u{2013}{}",
            with_thousands(bin.lo.round() as i64),
            with_thousands(bin.hi.round() as i64)
        )
    }
}

// ------------------------------------------------------------- stacking

/// Prefix sums per x: `accumulate(series)[k][i]` is the height of the
/// stack up through band `k` -- `stacked.rs`'s `accumulate`, verbatim
/// (`StackedArea.tsx`'s `cum` memo). Rows are sized to the longest input
/// series; a shorter one contributes `0` past its end.
pub fn accumulate(series: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let n = series.iter().map(Vec::len).max().unwrap_or(0);
    let mut prev = vec![0.0f32; n];
    let mut out = Vec::with_capacity(series.len());
    for s in series {
        let cur: Vec<f32> = (0..n)
            .map(|i| prev[i] + s.get(i).copied().unwrap_or(0.0))
            .collect();
        prev = cur.clone();
        out.push(cur);
    }
    out
}

// -------------------------------------------------------------- decimation

/// Cap on points per painted polyline / band edge.
///
/// A native-only addition: ggo-ide's iced charts decimate nothing (they
/// hand every frame to a `canvas::Path`), but this painter turns each
/// polyline into two triangles per segment, and ingest allows a run up to
/// 100,000 frames -- 200,000 triangles per series, rebuilt on every hover
/// move. 2048 points is already ~6 per pixel column at this panel's
/// widest, so nothing visible is lost. Runs below this cap (i.e. almost
/// all of them) are untouched.
pub const MAX_PLOT_POINTS: usize = 2048;

/// Spike-preserving decimation: splits `points` into `max_points / 2`
/// equal index buckets and keeps each bucket's lowest and highest point
/// (by y), emitted in index order.
///
/// Min/max rather than a plain stride BECAUSE this is a perf chart: a
/// one-frame budget overrun is exactly the sample a user is looking for,
/// and stride sampling is free to drop it. Returns `points` unchanged
/// when it is already short enough.
///
/// Known, accepted imprecision: the first and last points are NOT pinned.
/// Each bucket contributes its own extremes, and index 0 / index n-1 are
/// only among them by luck, so a series longer than `max_points` can
/// start and end up to one bucket short of the plot edges -- at the 2048
/// cap that is at most ~0.05% of the x range, i.e. sub-pixel at this
/// panel's widths. Pinning them would be two extra pushes, but it also
/// makes the output length `max_points + 2`; not worth the special case
/// until something visibly clips.
pub fn envelope(points: &[(f32, f32)], max_points: usize) -> Vec<(f32, f32)> {
    let buckets = max_points / 2;
    if points.len() <= max_points || buckets == 0 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(max_points);
    for b in 0..buckets {
        let lo = b * points.len() / buckets;
        let hi = ((b + 1) * points.len() / buckets)
            .max(lo + 1)
            .min(points.len());
        let mut min_ix = lo;
        let mut max_ix = lo;
        for (i, p) in points[lo..hi].iter().enumerate() {
            if p.1 < points[min_ix].1 {
                min_ix = lo + i;
            }
            if p.1 > points[max_ix].1 {
                max_ix = lo + i;
            }
        }
        let (first, second) = if min_ix <= max_ix {
            (min_ix, max_ix)
        } else {
            (max_ix, min_ix)
        };
        out.push(points[first]);
        if second != first {
            out.push(points[second]);
        }
    }
    out
}

/// Evenly-strided subset of `0..n`, always ending on `n - 1`.
///
/// Used for stacked bands, which (unlike independent polylines) MUST all
/// sample the same indices or adjacent bands stop lining up -- so a
/// per-series [`envelope`] is not an option there, and an area fill loses
/// far less to a stride than a line does.
pub fn stride_indices(n: usize, max: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    if n <= max || max < 2 {
        return (0..n).collect();
    }
    let step = n.div_ceil(max);
    let mut out: Vec<usize> = (0..n).step_by(step).collect();
    if out.last() != Some(&(n - 1)) {
        out.push(n - 1);
    }
    out
}

// ------------------------------------------------------------------ legend

pub const LEGEND_SWATCH: f32 = 8.0;
pub const LEGEND_ROW_HEIGHT: f32 = 14.0;
pub const LEGEND_FONT_SIZE: f32 = 10.0;
/// Gap between a swatch and its label.
const LEGEND_SWATCH_GAP: f32 = 4.0;
/// Gap between one entry's label and the next entry's swatch.
const LEGEND_ENTRY_GAP: f32 = 12.0;
/// Approximate advance width per character at [`LEGEND_FONT_SIZE`]. The
/// pure layer has no text shaper, so entry widths are estimated (the
/// painter's real glyph run may be a hair narrower/wider); the estimate
/// only decides where entries WRAP, so being slightly generous just wraps
/// a touch early rather than overlapping.
const LEGEND_CHAR_WIDTH: f32 = 5.5;

/// One laid-out legend entry: where its swatch and its label go.
#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    pub label: String,
    pub color: Rgb,
    pub swatch: Rect,
    /// Left edge of the label text; its baseline is [`TextBaseline::Middle`]
    /// on the swatch's vertical center.
    pub text_at: (f32, f32),
}

/// Wraps `(label, color)` pairs into rows of at most `width` px starting
/// at `(x, y)`. The first entry on a row is always placed even if it alone
/// overflows `width` (a very long series name gets its own row and is
/// clipped by the painter rather than dropped).
pub fn legend_layout(entries: &[(String, Rgb)], x: f32, y: f32, width: f32) -> Vec<LegendEntry> {
    let mut out = Vec::with_capacity(entries.len());
    let mut cursor_x = x;
    let mut row_y = y;
    for (label, color) in entries {
        let entry_w =
            LEGEND_SWATCH + LEGEND_SWATCH_GAP + label.chars().count() as f32 * LEGEND_CHAR_WIDTH;
        if cursor_x > x && cursor_x + entry_w > x + width {
            cursor_x = x;
            row_y += LEGEND_ROW_HEIGHT;
        }
        let mid = row_y + LEGEND_ROW_HEIGHT / 2.0;
        out.push(LegendEntry {
            label: label.clone(),
            color: *color,
            swatch: Rect {
                x: cursor_x,
                y: mid - LEGEND_SWATCH / 2.0,
                w: LEGEND_SWATCH,
                h: LEGEND_SWATCH,
            },
            text_at: (cursor_x + LEGEND_SWATCH + LEGEND_SWATCH_GAP, mid),
        });
        cursor_x += entry_w + LEGEND_ENTRY_GAP;
    }
    out
}

/// Total height of a laid-out legend (0 for an empty one) -- what
/// [`plot_rect`]'s `legend_h` wants.
pub fn legend_height(laid_out: &[LegendEntry]) -> f32 {
    let (Some(first), Some(last)) = (laid_out.first(), laid_out.last()) else {
        return 0.0;
    };
    // Entries are placed row by row, so the vertical span between the
    // first and last swatch is a whole number of row heights.
    let rows = ((last.swatch.y - first.swatch.y) / LEGEND_ROW_HEIGHT).round() + 1.0;
    rows * LEGEND_ROW_HEIGHT
}

// -------------------------------------------------------------- chart spec

/// One plotted series.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesSpec {
    pub name: String,
    pub color: Rgb,
    pub values: Vec<f32>,
}

/// Which of the three shapes a chart draws.
#[derive(Debug, Clone, PartialEq)]
pub enum ChartKind {
    /// Multi-series polylines with an optional dashed budget reference.
    Line { budget: Option<f32> },
    /// Filled cumulative bands, `series[0]` at the baseline.
    Stacked,
    /// A distribution over `series[0].values` (the frame axis is unused).
    Histogram,
}

/// One de-emphasized prior-run overlay line: values only, plus how far
/// back the run is, expressed as the opacity it draws at.
///
/// The geometry layer's shape of `ggo_worldlib::charts::reports::historic::
/// HistoricSeries`, which is what actually produces these (the alignment
/// and the age ramp are worldlib's; `chart_set` re-shapes, it does not
/// re-derive -- the same split `SeriesSpec` has from `NamedSeries`).
///
/// **Values pair with [`ChartSpec::x`] BY INDEX POSITION**, not by frame
/// number: a prior run's own frame numbering is never consulted (R1's
/// concern (2)). The renderer clips to `min(values.len(), x.len())`, so a
/// shorter prior run stops partway across the chart and a longer one is
/// truncated -- never an out-of-bounds index either way.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlaySpec {
    pub values: Vec<f32>,
    /// `historic::HISTORIC_OPACITY[k]` for the k-th nearest prior run.
    pub opacity: f32,
}

/// The interaction state one chart is painted under, and the reason it is
/// ONE struct rather than three arguments.
///
/// iced keeps hover/drag/zoom in each `canvas::Program`'s own retained
/// `State`; gpui re-renders from view state, so the panel owns all three
/// and passes them down per render. Bundling them means [`frame_at`] and
/// [`build_chart_scene`] take the *same* value, and a future fourth piece
/// of interaction state cannot be threaded into one and forgotten in the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChartView {
    /// Cursor position in canvas-local px, when it is over THIS chart.
    pub hover: Option<(f32, f32)>,
    /// The zoomed x-domain, or `None` for the full data domain.
    pub zoom: Option<(f32, f32)>,
    /// An in-progress drag's canvas-local x endpoints (press, cursor).
    /// Paints the selection band; the zoom itself is only committed on
    /// release, by the panel.
    pub drag: Option<(f32, f32)>,
    /// Whether the historic overlay is switched on. The overlays live on
    /// the SPEC (derived off-thread with everything else) and this decides
    /// whether they are painted -- **and therefore whether they count
    /// toward the y-scale**, which is the only honest coupling: an
    /// overlay that moved the scale while hidden would rescale a chart
    /// around a line the reader cannot see.
    pub historic: bool,
}

impl ChartView {
    /// The view of a chart with the cursor over it and nothing else set --
    /// the common shape in the hover tests, which is the only place it is
    /// wanted: the panel always has a zoom and a toggle to report too, so
    /// it builds the whole struct (`ChartsPanel::view_for`).
    #[cfg(test)]
    pub fn hovering(at: (f32, f32)) -> Self {
        Self {
            hover: Some(at),
            ..Self::default()
        }
    }
}

/// Everything one chart needs: title, kind, the shared frame axis, and
/// its series.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSpec {
    pub title: String,
    pub kind: ChartKind,
    /// Frame numbers, shared by every series. Empty for [`ChartKind::Histogram`].
    pub x: Vec<f32>,
    pub series: Vec<SeriesSpec>,
    /// Prior-run overlay lines, drawn beneath `series` and only when
    /// [`ChartView::historic`] is set. Empty on every chart but the four
    /// `charts_section` opts into the "Historic" toggle -- see
    /// [`OverlaySpec`].
    pub historic: Vec<OverlaySpec>,
    /// Whether clicking a point on this chart selects that frame for the
    /// inspect pane -- `reports.rs`'s `LineChart::on_select` hook, which
    /// `RunPage.tsx` passes (`onSelect={pickFrame}`) on exactly four
    /// charts: cache-misses, tile-working-set, and the two per-function
    /// ones. `chart_set` is what decides; [`frame_at`] is pure geometry
    /// and answers for any chart with a frame axis, selectable or not.
    pub selectable: bool,
}

impl ChartSpec {
    /// Whether this chart has anything to draw at all -- the panel shows
    /// an explicit empty message instead of a blank canvas otherwise, and
    /// [`build_chart_scene`] and [`frame_at`] both early-out on it.
    ///
    /// **`historic` deliberately does not count.** R5 gave this fn a
    /// second job -- it now also decides whether a chart's overlays get
    /// painted, since nothing is painted at all when it returns false --
    /// and the rule it applies is that an overlay is context for a run,
    /// not a run: a chart with prior-run ghosts and no live series of its
    /// own has nothing to be the history OF, and would show a reader
    /// nothing but grey. Unreachable from `chart_set` today (every spec it
    /// builds pairs the overlay with the live series it belongs to), so
    /// this is a rule being stated rather than a case being handled --
    /// pinned by `a_chart_with_only_overlays_has_no_data`.
    pub fn has_data(&self) -> bool {
        match self.kind {
            ChartKind::Histogram => self.series.iter().any(|s| !s.values.is_empty()),
            _ => !self.x.is_empty() && !self.series.is_empty(),
        }
    }
}

// ------------------------------------------------------------ hover readout

/// One line of the hover readout.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadoutRow {
    pub label: String,
    /// Already formatted (exact, comma-grouped -- see [`with_thousands`]).
    pub value: String,
    /// `None` for a row with no series of its own (`StackedArea`'s
    /// "total", `Histogram`'s frame count), which draws in the text color.
    pub color: Option<Rgb>,
}

/// The hover overlay's contents: a title (`"frame 12"`, or a histogram bin
/// label) plus one row per series. Structured rather than pre-painted so a
/// test can assert the readout without inspecting glyphs.
#[derive(Debug, Clone, PartialEq)]
pub struct Readout {
    pub title: String,
    pub rows: Vec<ReadoutRow>,
    /// The sample this readout describes: a frame index into `spec.x` for
    /// line/stacked, a bin index for a histogram.
    pub index: usize,
    /// Where the crosshair/box anchors, in canvas-local px.
    pub anchor_x: f32,
}

// ---------------------------------------------------------------- scene

/// A renderer-independent description of one painted chart.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartScene {
    pub primitives: Vec<Primitive>,
    /// `Some` only when the cursor is inside the plot area over a sample.
    pub readout: Option<Readout>,
}

const AXIS_TICK_FONT_SIZE: f32 = 11.0;
const AXIS_LABEL_GAP_PX: f32 = 6.0;
const GRID_STROKE_WIDTH: f32 = 1.0;
const X_TICK_LABEL_GAP_PX: f32 = 14.0;
const AXIS_CAPTION_FONT_SIZE: f32 = 10.0;
const AXIS_CAPTION_BOTTOM_GAP_PX: f32 = 3.0;

const SERIES_STROKE_WIDTH: f32 = 2.0;
/// `line.rs`'s `HISTORIC_STROKE_WIDTH` (`LineChart.tsx`'s historic-context
/// `<polyline stroke-width="1.5">`): thinner than a live series, so a
/// ghost never competes with the run being read.
const HISTORIC_STROKE_WIDTH: f32 = 1.5;
const BUDGET_STROKE_WIDTH: f32 = 1.5;
const BUDGET_DASH: (f32, f32) = (6.0, 4.0);
const BAND_SEPARATOR_STROKE_WIDTH: f32 = 1.5;

const BAR_MAX_WIDTH: f32 = 24.0;
const BAR_MIN_WIDTH: f32 = 1.0;
const BAR_GAP_PX: f32 = 2.0;
const BAR_BASE_ALPHA: f32 = 0.85;

const TOOLTIP_PADDING: f32 = 6.0;
const TOOLTIP_ROW_HEIGHT: f32 = 14.0;
const TOOLTIP_FONT_SIZE: f32 = 11.0;
const TOOLTIP_WIDTH: f32 = 140.0;
const TOOLTIP_OFFSET_PX: f32 = 10.0;
const TOOLTIP_BORDER_WIDTH: f32 = 1.0;

/// `histogram.rs`'s `bar_width`.
fn bar_width(slot_w: f32) -> f32 {
    (slot_w - BAR_GAP_PX).clamp(BAR_MIN_WIDTH, BAR_MAX_WIDTH)
}

/// The margins a spec's kind uses.
pub fn margins_for(kind: &ChartKind) -> Margins {
    match kind {
        ChartKind::Line { .. } => LINE_MARGINS,
        ChartKind::Stacked => STACKED_MARGINS,
        ChartKind::Histogram => HISTOGRAM_MARGINS,
    }
}

/// Builds the full paint description for one chart at `size`, with the
/// cursor (canvas-local) at `hover`. Pure: same inputs -> same
/// primitives, no window/theme needed, so a test can assert both the
/// primitive list and the hover readout directly.
///
/// An empty spec or a degenerate plot area yields an EMPTY scene -- the
/// panel checks [`ChartSpec::has_data`] first and renders a message
/// instead, so a blank canvas is never what a user sees.
/// The legend band a chart of `spec` draws at `size`, laid out.
///
/// A histogram plots one unnamed distribution; naming it in a legend
/// would just repeat the chart title, so it gets none.
fn legend_for(spec: &ChartSpec, size: (f32, f32)) -> Vec<LegendEntry> {
    if matches!(spec.kind, ChartKind::Histogram) {
        return Vec::new();
    }
    let m = margins_for(&spec.kind);
    legend_layout(
        &spec
            .series
            .iter()
            .map(|s| (s.name.clone(), s.color))
            .collect::<Vec<_>>(),
        m.left,
        0.0,
        (size.0 - m.left - m.right).max(0.0),
    )
}

/// The plot area a chart of `spec` gets at `size`, legend band included.
///
/// The ONE place that derivation lives: [`build_chart_scene`] paints into
/// this rect and [`frame_at`] hit-tests against it, so a click cannot
/// resolve against a plot area the points were never drawn in. Cheap
/// (a legend layout, no series work), which is what lets the click path
/// call it without going anywhere near a scene build.
pub fn plot_for(spec: &ChartSpec, size: (f32, f32)) -> Rect {
    plot_rect(
        size,
        margins_for(&spec.kind),
        legend_height(&legend_for(spec, size)),
    )
}

// How many scenes have been built ON THIS THREAD. Test-only, and the same
// shape (and the same reason for being thread-local rather than static)
// as `detail::BUILDS`: `cargo test` runs the suite concurrently.
//
// What it is for: `ChartsPanel`'s scene cache is only worth its
// complexity if a hover move really does rebuild one chart instead of
// eleven, and "how many times was this function called during that draw"
// is the only way to assert that from outside -- the cheap and the
// expensive path return the same scene by construction.
#[cfg(test)]
thread_local! {
    static SCENE_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many scenes this thread has built so far.
#[cfg(test)]
pub fn scene_builds() -> usize {
    SCENE_BUILDS.with(std::cell::Cell::get)
}

pub fn build_chart_scene(spec: &ChartSpec, size: (f32, f32), view: &ChartView) -> ChartScene {
    #[cfg(test)]
    SCENE_BUILDS.with(|n| n.set(n.get() + 1));
    let legend = legend_for(spec, size);
    let plot = plot_for(spec, size);
    let hover = view.hover;

    let mut scene = ChartScene {
        primitives: Vec::new(),
        readout: None,
    };
    if !spec.has_data() || !plot.is_drawable() {
        return scene;
    }

    for entry in &legend {
        scene.primitives.push(Primitive::Quad {
            rect: entry.swatch,
            color: ChartColor::Series(entry.color),
        });
        scene.primitives.push(Primitive::Text {
            at: entry.text_at,
            content: entry.label.clone(),
            size: LEGEND_FONT_SIZE,
            anchor: TextAnchor::Left,
            baseline: TextBaseline::Middle,
            color: ChartColor::Text,
        });
    }

    match &spec.kind {
        ChartKind::Line { budget } => build_line(&mut scene, spec, plot, size, *budget, view),
        ChartKind::Stacked => build_stacked(&mut scene, spec, plot, size, view),
        ChartKind::Histogram => build_histogram(&mut scene, spec, plot, hover),
    }

    // The drag-zoom selection band, over the series and under the
    // tooltip -- `line.rs` paints it into its own uncached dynamic layer
    // for the same reason (it changes on every mouse-move). An
    // axis-aligned rect, so it is a `Quad`: the concave-band fan hazard
    // `chart_paint::polygon_path` documents cannot arise for it.
    if let Some((from, to)) = view.drag {
        let (x0, x1) = (from.min(to), from.max(to));
        let x0 = x0.clamp(plot.x, plot.right());
        let x1 = x1.clamp(plot.x, plot.right());
        if x1 > x0 {
            scene.primitives.push(Primitive::Quad {
                rect: Rect {
                    x: x0,
                    y: plot.y,
                    w: x1 - x0,
                    h: plot.h,
                },
                color: ChartColor::Selection,
            });
        }
    }

    if let Some(readout) = &scene.readout {
        let readout = readout.clone();
        push_tooltip(&mut scene, plot, &readout);
    }
    scene
}

/// y gridlines + right-aligned tick labels -- `frame_draw::draw_y_gridlines`.
fn push_y_gridlines(scene: &mut ChartScene, plot: Rect, ticks: &[f32], ys: &LinearScale) {
    for &tick in ticks {
        let y = ys.map(tick);
        scene.primitives.push(Primitive::Segment {
            from: (plot.x, y),
            to: (plot.right(), y),
            width: GRID_STROKE_WIDTH,
            color: ChartColor::Grid,
            dash: None,
        });
        scene.primitives.push(Primitive::Text {
            at: (plot.x - AXIS_LABEL_GAP_PX, y),
            content: format_tick(tick as f64),
            size: AXIS_TICK_FONT_SIZE,
            anchor: TextAnchor::Right,
            baseline: TextBaseline::Middle,
            color: ChartColor::Text,
        });
    }
}

fn push_x_baseline(scene: &mut ChartScene, plot: Rect) {
    scene.primitives.push(Primitive::Segment {
        from: (plot.x, plot.bottom()),
        to: (plot.right(), plot.bottom()),
        width: GRID_STROKE_WIDTH,
        color: ChartColor::Grid,
        dash: None,
    });
}

fn push_x_tick_labels(scene: &mut ChartScene, plot: Rect, labels: &[(f32, String)]) {
    let y = plot.bottom() + X_TICK_LABEL_GAP_PX;
    for (px, label) in labels {
        scene.primitives.push(Primitive::Text {
            at: (*px, y),
            content: label.clone(),
            size: AXIS_TICK_FONT_SIZE,
            anchor: TextAnchor::Center,
            baseline: TextBaseline::Top,
            color: ChartColor::Text,
        });
    }
}

/// The centered `"frame"` caption -- `frame_draw::draw_x_axis_caption`
/// (`canvas_h - 3`, the canvas's own bottom, not the plot's).
fn push_x_caption(scene: &mut ChartScene, plot: Rect, canvas_h: f32) {
    scene.primitives.push(Primitive::Text {
        at: (
            plot.x + plot.w / 2.0,
            canvas_h - AXIS_CAPTION_BOTTOM_GAP_PX - AXIS_CAPTION_FONT_SIZE,
        ),
        content: "frame".to_string(),
        size: AXIS_CAPTION_FONT_SIZE,
        anchor: TextAnchor::Center,
        baseline: TextBaseline::Top,
        color: ChartColor::Text,
    });
}

/// Shared frame-axis tick labels, positioned through `xs` (so the label
/// set is the same one `frame_draw::x_tick_indices` picks).
fn push_frame_axis(
    scene: &mut ChartScene,
    spec: &ChartSpec,
    plot: Rect,
    xs: &LinearScale,
    canvas_h: f32,
) {
    let labels: Vec<(f32, String)> = x_tick_indices(spec.x.len())
        .into_iter()
        .filter_map(|i| spec.x.get(i).copied().map(|x| (xs.map(x), format!("{x}"))))
        .filter(|(px, _)| *px >= plot.x && *px <= plot.right())
        .collect();
    push_x_tick_labels(scene, plot, &labels);
    push_x_caption(scene, plot, canvas_h);
}

/// The overlays a chart actually draws under `view`, each truncated to the
/// frame axis: `(values, opacity)`. Empty when the Historic toggle is off.
///
/// Built ONCE and read by BOTH the y-scale fold and the polyline loop,
/// which is the whole point -- `line.rs`'s `y_scale` folds `props.context`
/// into its max, and R1's concern (2) is that nothing in worldlib will stop
/// a renderer forgetting to. One list means the scale and the paint cannot
/// disagree about which overlays exist.
///
/// **An overlay with fewer than two points on this axis is dropped from
/// that list rather than folded into the scale**, because a one-point
/// polyline paints nothing (`points.len() >= 2` below, and `Polyline` is a
/// stroked ribbon -- there is no segment to stroke). R5 round 1 folded it
/// and then failed to draw it: a prior run left with a single frame by the
/// ignore filter -- a two-frame capture from an aborted run, entirely
/// ordinary -- silently rescaled the current run's chart around a ghost
/// that was never painted, collapsing the live series to a fraction of the
/// plot with no visible cause. Skipping it in one place fixes both halves
/// at once.
///
/// The truncation to `spec.x.len()` is the alignment contract
/// ([`OverlaySpec`]): a longer prior run is cut at the axis's end, and the
/// y-scale must not see the part that falls off it.
///
/// One residual, deliberately matched rather than fixed: under a ZOOM the
/// x-clip in [`plot_points`] can drop further points that this list still
/// folds, so a sample outside the window can still move the scale. The
/// LIVE series behaves identically, and so does `line.rs:241-259`, so
/// changing it here would make the overlay disagree with the line it sits
/// under. It is also unreachable on a contiguous frame axis:
/// [`MIN_ZOOM_DOMAIN_WIDTH`] guarantees a window at least two frames wide.
fn drawn_overlays<'a>(spec: &'a ChartSpec, view: &ChartView) -> Vec<(&'a [f32], f32)> {
    if !view.historic {
        return Vec::new();
    }
    spec.historic
        .iter()
        .filter_map(|hs| {
            let n = hs.values.len().min(spec.x.len());
            (n >= 2).then(|| (&hs.values[..n], hs.opacity))
        })
        .collect()
}

/// Series points under the current x-domain: `(pixel, pixel)` pairs for
/// the samples still inside it. A zoomed-out chart keeps every sample; a
/// zoomed-in one drops the rest rather than mapping them past the plot
/// edge (`line.rs`'s `if x < d0 || x > d1 { continue }`).
///
/// `values` is indexed BY POSITION against `spec.x` -- true of a live
/// series and of an [`OverlaySpec`] alike, which is what lets both go
/// through one function. They differ only in what a SHORT `values` means,
/// which is `pad`: a live series shorter than the frame axis reads as zero
/// past its end (the pre-R5 behaviour, and what `chart_set`'s own pivot
/// onto the frame axis already guarantees), while a prior run with fewer
/// frames simply stops partway across the chart rather than dropping to
/// the baseline -- a ghost that dived to zero would read as a measurement.
fn plot_points(
    spec: &ChartSpec,
    values: &[f32],
    xs: &LinearScale,
    ys: &LinearScale,
    pad: bool,
) -> Vec<(f32, f32)> {
    let (d0, d1) = xs.domain;
    let mut points = Vec::with_capacity(spec.x.len());
    for (i, &x) in spec.x.iter().enumerate() {
        if x < d0 || x > d1 {
            continue;
        }
        let v = match values.get(i) {
            Some(&v) => v,
            None if pad => 0.0,
            None => break,
        };
        points.push((xs.map(x), ys.map(v)));
    }
    points
}

fn build_line(
    scene: &mut ChartScene,
    spec: &ChartSpec,
    plot: Rect,
    size: (f32, f32),
    budget: Option<f32>,
    view: &ChartView,
) {
    let historic = drawn_overlays(spec, view);

    // y-domain tops out at a nice tick above the largest value AND the
    // budget line, so a budget above every sample still fits --
    // `line.rs`'s `y_scale`.
    let mut max = 0f32;
    for s in &spec.series {
        for &v in &s.values {
            max = max.max(v);
        }
    }
    // **Historic values participate in the y-scale.** This is the brief's
    // load-bearing clause: an overlay taller than the live run would
    // otherwise map above `plot.y` and be clipped away by the content mask
    // -- silently absent precisely when it has the most to say.
    // `line.rs`'s `y_scale` folds `self.historic` in for the same reason.
    // The list folded here is the same list painted below, already
    // truncated to the axis and already missing anything that cannot draw
    // -- see `drawn_overlays`, which is where that rule lives.
    for (values, _) in &historic {
        for &v in *values {
            max = max.max(v);
        }
    }
    if let Some(b) = budget {
        max = max.max(b);
    }
    let ys = y_scale(plot, max, Y_TICK_TARGET, false);
    let xs = x_scale_for(spec, plot, view);

    push_y_gridlines(scene, plot, &ys.ticks(Y_TICK_TARGET), &ys);
    push_x_baseline(scene, plot);
    push_frame_axis(scene, spec, plot, &xs, size.1);

    if let Some(b) = budget {
        let y = ys.map(b);
        scene.primitives.push(Primitive::Segment {
            from: (plot.x, y),
            to: (plot.right(), y),
            width: BUDGET_STROKE_WIDTH,
            color: ChartColor::Budget,
            dash: Some(BUDGET_DASH),
        });
    }

    // Overlays first, so the live run draws OVER its own history.
    //
    // Clipped to the visible domain, which `line.rs` deliberately does NOT
    // do (its comment reasons that an index-plotted overlay has no
    // "outside the domain" test). It does: an overlay is plotted at
    // `xs.map(spec.x[i])`, the same expression the live series is clipped
    // on, so the same test applies exactly. Not clipping would paint a
    // zoomed-out chart's ghosts across this chart's margins and y-axis
    // labels, since gpui masks to the CANVAS, not to the plot rect.
    for (values, opacity) in &historic {
        let points = plot_points(spec, values, &xs, &ys, false);
        let points = envelope(&points, MAX_PLOT_POINTS);
        // Still guarded, for the zoom case `drawn_overlays`' doc names:
        // an x-clip can leave one point where the axis had two.
        if points.len() >= 2 {
            scene.primitives.push(Primitive::Polyline {
                points,
                width: HISTORIC_STROKE_WIDTH,
                color: ChartColor::Historic(*opacity),
            });
        }
    }

    for s in &spec.series {
        let points = plot_points(spec, &s.values, &xs, &ys, true);
        let points = envelope(&points, MAX_PLOT_POINTS);
        if points.len() >= 2 {
            scene.primitives.push(Primitive::Polyline {
                points,
                width: SERIES_STROKE_WIDTH,
                color: ChartColor::Series(s.color),
            });
        }
    }

    scene.readout = shared_x_readout(spec, plot, &xs, view.hover, false);
    if let Some(readout) = &scene.readout {
        push_crosshair(scene, plot, readout.anchor_x);
    }
}

/// Stacked bands. No historic overlay and no drag-zoom affordance --
/// `StackedArea.tsx`/`stacked.rs` have neither (the panel only installs
/// the drag handlers on [`ChartKind::Line`]) -- but the x-domain still
/// resolves through [`x_scale_for`] rather than `full_x_domain` directly,
/// so this chart cannot end up with a second answer to "which x am I
/// showing" if that ever changes.
fn build_stacked(
    scene: &mut ChartScene,
    spec: &ChartSpec,
    plot: Rect,
    size: (f32, f32),
    view: &ChartView,
) {
    let hover = view.hover;
    let values: Vec<Vec<f32>> = spec.series.iter().map(|s| s.values.clone()).collect();
    let cum = accumulate(&values);
    let mut max = 0f32;
    if let Some(top) = cum.last() {
        for &v in top {
            max = max.max(v);
        }
    }
    let ys = y_scale(plot, max, Y_TICK_TARGET, false);
    let xs = x_scale_for(spec, plot, view);
    let n = spec.x.len();

    push_y_gridlines(scene, plot, &ys.ticks(Y_TICK_TARGET), &ys);
    push_x_baseline(scene, plot);
    push_frame_axis(scene, spec, plot, &xs, size.1);

    // Every band and separator samples the SAME indices, or adjacent
    // bands stop sharing an edge -- see `stride_indices`' doc.
    let ix: Vec<usize> = stride_indices(n, MAX_PLOT_POINTS / 2);

    // Bands, baseline slot first: the band's own cumulative top edge
    // forward, then the band below it (or zero) backward -- `stacked.rs`.
    for (k, s) in spec.series.iter().enumerate() {
        let top = &cum[k];
        let mut points: Vec<(f32, f32)> = ix
            .iter()
            .map(|&i| {
                (
                    xs.map(spec.x[i]),
                    ys.map(top.get(i).copied().unwrap_or(0.0)),
                )
            })
            .collect();
        points.extend(ix.iter().rev().map(|&i| {
            let v = if k > 0 {
                cum[k - 1].get(i).copied().unwrap_or(0.0)
            } else {
                0.0
            };
            (xs.map(spec.x[i]), ys.map(v))
        }));
        scene.primitives.push(Primitive::Polygon {
            points,
            color: ChartColor::Band(s.color),
        });
    }

    // Surface-colored separators between adjacent bands.
    for boundary in cum.iter().take(spec.series.len().saturating_sub(1)) {
        let points: Vec<(f32, f32)> = ix
            .iter()
            .map(|&i| {
                (
                    xs.map(spec.x[i]),
                    ys.map(boundary.get(i).copied().unwrap_or(0.0)),
                )
            })
            .collect();
        if points.len() >= 2 {
            scene.primitives.push(Primitive::Polyline {
                points,
                width: BAND_SEPARATOR_STROKE_WIDTH,
                color: ChartColor::Surface,
            });
        }
    }

    scene.readout = shared_x_readout(spec, plot, &xs, hover, true);
    if let Some(readout) = &scene.readout {
        push_crosshair(scene, plot, readout.anchor_x);
    }
}

fn build_histogram(
    scene: &mut ChartScene,
    spec: &ChartSpec,
    plot: Rect,
    hover: Option<(f32, f32)>,
) {
    let Some(series) = spec.series.first() else {
        return;
    };
    let bin_list = bins(&series.values, HISTOGRAM_BIN_TARGET);
    if bin_list.is_empty() {
        return;
    }
    let max = bin_list.iter().map(|b| b.count).max().unwrap_or(0);
    let ys = y_scale(plot, max as f32, HISTOGRAM_Y_TICK_TARGET, true);
    let nbins = bin_list.len();
    let slot_w = plot.w / nbins as f32;

    push_y_gridlines(scene, plot, &ys.ticks_integer(HISTOGRAM_Y_TICK_TARGET), &ys);
    push_x_baseline(scene, plot);

    // x tick labels at a few bin edges (`Histogram.tsx`'s `xTicks` memo).
    // No axis caption -- `Histogram.tsx` has none.
    let labels: Vec<(f32, String)> = edge_marks(nbins)
        .into_iter()
        .map(|i| {
            let px = plot.x + i as f32 * slot_w;
            let value = if i < nbins {
                bin_list[i].lo
            } else {
                bin_list[nbins - 1].hi
            };
            (px, format_tick(value as f64))
        })
        .collect();
    push_x_tick_labels(scene, plot, &labels);

    let hovered = hover
        .filter(|(px, py)| plot.contains(*px, *py))
        .map(|(px, _)| slot_index(px, plot.x, slot_w, nbins));

    let bar_w = bar_width(slot_w);
    for (i, bin) in bin_list.iter().enumerate() {
        if bin.count == 0 {
            continue;
        }
        let y_top = ys.map(bin.count as f32);
        let rect = Rect {
            x: plot.x + i as f32 * slot_w + (slot_w - bar_w) / 2.0,
            y: y_top,
            w: bar_w,
            h: (plot.bottom() - y_top).max(0.0),
        };
        // Bars rest translucent so the hovered one reads as a "lift" --
        // `Histogram.tsx`'s `opacity={hover() === i() ? 1 : 0.85}`. The
        // bar color is the theme accent, not `series.color`: see
        // `ChartColor::Accent`.
        let color = if hovered == Some(i) {
            ChartColor::Accent
        } else {
            ChartColor::AccentAlpha(BAR_BASE_ALPHA)
        };
        scene.primitives.push(Primitive::Quad { rect, color });
    }

    if let Some(i) = hovered
        && let Some(bin) = bin_list.get(i)
    {
        scene.readout = Some(Readout {
            title: bin_label(bin),
            rows: vec![ReadoutRow {
                label: if bin.count == 1 { "frame" } else { "frames" }.to_string(),
                value: with_thousands(bin.count as i64),
                color: None,
            }],
            index: i,
            anchor_x: plot.x + (i as f32 + 0.5) * slot_w,
        });
    }
}

/// The shared-x hover readout for a line/stacked chart: nearest sample by
/// pixel distance, one row per series (exact values), plus a `total` row
/// for stacked -- `line.rs`'s/`stacked.rs`'s tooltip bodies.
fn shared_x_readout(
    spec: &ChartSpec,
    plot: Rect,
    xs: &LinearScale,
    hover: Option<(f32, f32)>,
    with_total: bool,
) -> Option<Readout> {
    let (hx, hy) = hover?;
    if !plot.contains(hx, hy) {
        return None;
    }
    let idx = nearest_x(hx, &spec.x, xs)?;
    let x = *spec.x.get(idx)?;
    let mut rows: Vec<ReadoutRow> = spec
        .series
        .iter()
        .map(|s| ReadoutRow {
            label: s.name.clone(),
            value: with_thousands(s.values.get(idx).copied().unwrap_or(0.0).round() as i64),
            color: Some(s.color),
        })
        .collect();
    if with_total {
        let total: f32 = spec
            .series
            .iter()
            .map(|s| s.values.get(idx).copied().unwrap_or(0.0))
            .sum();
        rows.push(ReadoutRow {
            label: "total".to_string(),
            value: with_thousands(total.round() as i64),
            color: None,
        });
    }
    Some(Readout {
        title: format!("frame {x:.0}"),
        rows,
        index: idx,
        anchor_x: xs.map(x),
    })
}

fn push_crosshair(scene: &mut ChartScene, plot: Rect, px: f32) {
    scene.primitives.push(Primitive::Segment {
        from: (px, plot.y),
        to: (px, plot.bottom()),
        width: GRID_STROKE_WIDTH,
        color: ChartColor::Grid,
        dash: None,
    });
}

/// The readout box: filled + outlined, title line then one row per series
/// -- `frame_draw::draw_tooltip`'s layout, kept inside the plot area.
fn push_tooltip(scene: &mut ChartScene, plot: Rect, readout: &Readout) {
    let h = TOOLTIP_PADDING * 2.0 + TOOLTIP_ROW_HEIGHT * (readout.rows.len() as f32 + 1.0);
    let x = (readout.anchor_x + TOOLTIP_OFFSET_PX)
        .min(plot.right() - TOOLTIP_WIDTH)
        .max(plot.x);
    let rect = Rect {
        x,
        y: plot.y,
        w: TOOLTIP_WIDTH,
        h,
    };
    scene.primitives.push(Primitive::Quad {
        rect,
        color: ChartColor::Surface,
    });
    scene.primitives.push(Primitive::Outline {
        rect,
        width: TOOLTIP_BORDER_WIDTH,
        color: ChartColor::Text,
    });
    scene.primitives.push(Primitive::Text {
        at: (x + TOOLTIP_PADDING, plot.y + TOOLTIP_PADDING),
        content: readout.title.clone(),
        size: TOOLTIP_FONT_SIZE,
        anchor: TextAnchor::Left,
        baseline: TextBaseline::Top,
        color: ChartColor::Text,
    });
    for (i, row) in readout.rows.iter().enumerate() {
        scene.primitives.push(Primitive::Text {
            at: (
                x + TOOLTIP_PADDING,
                plot.y + TOOLTIP_PADDING + TOOLTIP_ROW_HEIGHT * (i as f32 + 1.0),
            ),
            content: format!("{}: {}", row.label, row.value),
            size: TOOLTIP_FONT_SIZE,
            anchor: TextAnchor::Left,
            baseline: TextBaseline::Top,
            color: match row.color {
                Some(c) => ChartColor::Series(c),
                None => ChartColor::Text,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The REAL age ramp, swept by
    // `the_age_ramp_gives_each_prior_run_a_distinguishable_opacity`. A
    // copy of its values would guard the copy -- see that test.
    use ggo_worldlib::charts::reports::historic::HISTORIC_OPACITY;

    const CANVAS: (f32, f32) = (400.0, 240.0);

    fn line_spec() -> ChartSpec {
        ChartSpec {
            title: "Wire cycles per frame vs budget".to_string(),
            kind: ChartKind::Line { budget: Some(80.0) },
            x: vec![0.0, 1.0, 2.0, 3.0],
            series: vec![SeriesSpec {
                name: "wire_total".to_string(),
                color: Rgb(0x4a8fe3),
                values: vec![10.0, 20.0, 30.0, 40.0],
            }],
            historic: Vec::new(),
            selectable: true,
        }
    }

    // ------------------------------------------------------------- layout

    #[test]
    fn plot_rect_insets_by_the_margins_and_legend() {
        let plot = plot_rect((400.0, 240.0), LINE_MARGINS, 14.0);
        assert_eq!(plot.x, LINE_MARGINS.left);
        assert_eq!(plot.y, LINE_MARGINS.top + 14.0);
        assert_eq!(plot.w, 400.0 - LINE_MARGINS.left - LINE_MARGINS.right);
        assert_eq!(
            plot.h,
            240.0 - LINE_MARGINS.top - LINE_MARGINS.bottom - 14.0
        );
    }

    /// A canvas narrower/shorter than its own margins must not produce a
    /// negative-size rect (the iced ports' `.max(0.0)`), and must report
    /// itself undrawable so `build_chart_scene` early-outs.
    #[test]
    fn plot_rect_clamps_a_canvas_smaller_than_its_margins() {
        let plot = plot_rect((10.0, 10.0), LINE_MARGINS, 0.0);
        assert_eq!(plot.w, 0.0);
        assert_eq!(plot.h, 0.0);
        assert!(!plot.is_drawable());
    }

    // -------------------------------------------------------------- ticks

    /// Degenerate y-domain: every sample zero. `ticks_for`'s `!(max > 0)`
    /// fallback gives `[0, 1]`, so the domain tops out at 1 rather than
    /// collapsing to a zero-height scale.
    #[test]
    fn nice_top_of_an_all_zero_series_is_one() {
        assert_eq!(nice_top(0.0, Y_TICK_TARGET, false), 1.0);
        assert_eq!(nice_top(0.0, HISTOGRAM_Y_TICK_TARGET, true), 1.0);
    }

    #[test]
    fn nice_top_rounds_up_to_a_nice_tick() {
        // ticks(5) over max=97 steps by 20 -> tops out at 100.
        assert_eq!(nice_top(97.0, 5, false), 100.0);
    }

    /// `ticks_integer` forces a fractional step up to a whole number: a
    /// frame-count axis never shows `2.5 frames`.
    #[test]
    fn nice_top_integer_never_yields_a_fractional_top() {
        let top = nice_top(9.0, 4, true);
        assert_eq!(top, 9.0);
        assert_eq!(top.fract(), 0.0);
    }

    #[test]
    fn y_scale_inverts_so_larger_values_draw_higher() {
        let plot = Rect {
            x: 0.0,
            y: 10.0,
            w: 100.0,
            h: 200.0,
        };
        let ys = y_scale(plot, 100.0, Y_TICK_TARGET, false);
        assert_eq!(ys.map(0.0), plot.bottom());
        assert_eq!(ys.map(100.0), plot.y);
        assert!(ys.map(100.0) < ys.map(50.0));
    }

    #[test]
    fn x_tick_indices_matches_the_ide_x_tick_memo() {
        assert_eq!(x_tick_indices(0), vec![0]);
        assert_eq!(x_tick_indices(1), vec![0]);
        assert_eq!(x_tick_indices(2), vec![0, 1]);
        assert_eq!(x_tick_indices(10), vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn edge_marks_dedupes_a_tiny_bin_count() {
        assert_eq!(edge_marks(1), vec![0, 1]);
        assert_eq!(edge_marks(4), vec![0, 4]);
        assert_eq!(edge_marks(10), vec![0, 5, 10]);
    }

    /// `nbins == 0` would index `[0, 0]` twice without the dedupe.
    #[test]
    fn edge_marks_of_zero_bins_is_a_single_edge() {
        assert_eq!(edge_marks(0), vec![0]);
    }

    // ---------------------------------------------------------- hit-test

    #[test]
    fn nearest_x_picks_the_closest_mapped_sample() {
        let xs = [0.0, 10.0, 20.0, 30.0];
        let scale = LinearScale {
            domain: (0.0, 30.0),
            range: (0.0, 300.0),
        };
        assert_eq!(nearest_x(105.0, &xs, &scale), Some(1));
        assert_eq!(nearest_x(200.0, &xs, &scale), Some(2));
    }

    /// Both plot edges, and past them: a cursor left of the first sample
    /// or right of the last still resolves to that end sample rather than
    /// to nothing.
    #[test]
    fn nearest_x_at_and_past_the_boundaries_clamps_to_the_end_samples() {
        let xs = [0.0, 10.0, 20.0, 30.0];
        let scale = LinearScale {
            domain: (0.0, 30.0),
            range: (0.0, 300.0),
        };
        assert_eq!(nearest_x(0.0, &xs, &scale), Some(0));
        assert_eq!(nearest_x(300.0, &xs, &scale), Some(3));
        assert_eq!(nearest_x(-500.0, &xs, &scale), Some(0));
        assert_eq!(nearest_x(9_999.0, &xs, &scale), Some(3));
    }

    #[test]
    fn nearest_x_breaks_ties_toward_the_earlier_index() {
        let xs = [0.0, 20.0];
        let scale = LinearScale {
            domain: (0.0, 20.0),
            range: (0.0, 20.0),
        };
        assert_eq!(nearest_x(10.0, &xs, &scale), Some(0));
    }

    /// A single-sample run: the x-domain collapses to a point, so
    /// `LinearScale::map` returns `range.0` for every input. The hit-test
    /// must still answer index 0, not `None` or a panic.
    #[test]
    fn nearest_x_on_a_single_sample_always_hits_index_zero() {
        let xs = [7.0];
        let scale = LinearScale {
            domain: (7.0, 7.0),
            range: (56.0, 356.0),
        };
        assert_eq!(nearest_x(56.0, &xs, &scale), Some(0));
        assert_eq!(nearest_x(356.0, &xs, &scale), Some(0));
    }

    #[test]
    fn nearest_x_of_an_empty_axis_is_none() {
        let scale = LinearScale {
            domain: (0.0, 1.0),
            range: (0.0, 100.0),
        };
        assert_eq!(nearest_x(50.0, &[], &scale), None);
    }

    // ------------------------------------------------ click -> frame

    /// A run whose ignore filter dropped frame 0, so the axis starts at 1
    /// and a frame NUMBER is never a frame INDEX -- the confusion the
    /// return type exists to prevent.
    fn selectable_spec() -> ChartSpec {
        ChartSpec {
            title: "Cache misses per frame".to_string(),
            kind: ChartKind::Line { budget: None },
            x: vec![1.0, 2.0, 3.0, 4.0],
            series: vec![SeriesSpec {
                name: "i_misses".to_string(),
                color: Rgb(0x4a8fe3),
                values: vec![10.0, 20.0, 30.0, 40.0],
            }],
            historic: Vec::new(),
            selectable: true,
        }
    }

    /// The x pixel a given frame's point was drawn at, derived the same
    /// way the chart draws it.
    fn px_of(spec: &ChartSpec, size: (f32, f32), frame: f32) -> f32 {
        let plot = plot_for(spec, size);
        x_scale(plot, full_x_domain(&spec.x)).map(frame)
    }

    #[test]
    fn a_click_on_a_sample_resolves_to_that_frames_number() {
        let spec = selectable_spec();
        let plot = plot_for(&spec, CANVAS);
        let mid_y = plot.y + plot.h / 2.0;
        for frame in [1.0, 2.0, 3.0, 4.0] {
            assert_eq!(
                frame_at(
                    &spec,
                    CANVAS,
                    (px_of(&spec, CANVAS, frame), mid_y),
                    &ChartView::default()
                ),
                Some(frame as i64),
                "frame {frame}"
            );
        }
    }

    /// The x-scale is a function of the canvas the chart actually got,
    /// and this panel lives in a resizable dock -- the same frame sits at
    /// a different pixel in a 360 px dock than in a 900 px one, and the
    /// hit-test has to follow the scale rather than a remembered layout.
    #[test]
    fn the_same_frame_is_hit_at_whatever_pixel_the_current_scale_puts_it() {
        let spec = selectable_spec();
        let narrow = (200.0, 180.0);
        let wide = (1200.0, 400.0);
        let narrow_px = px_of(&spec, narrow, 3.0);
        let wide_px = px_of(&spec, wide, 3.0);
        assert!(
            (narrow_px - wide_px).abs() > 100.0,
            "the two scales must actually differ for this to prove anything"
        );
        for (size, px) in [(narrow, narrow_px), (wide, wide_px)] {
            let plot = plot_for(&spec, size);
            assert_eq!(
                frame_at(
                    &spec,
                    size,
                    (px, plot.y + plot.h / 2.0),
                    &ChartView::default()
                ),
                Some(3),
                "canvas {size:?}"
            );
        }
        // ...and the wide canvas's pixel for frame 3 is nowhere near
        // frame 3 on the narrow one.
        let narrow_plot = plot_for(&spec, narrow);
        assert_ne!(
            frame_at(
                &spec,
                narrow,
                (wide_px, narrow_plot.y + narrow_plot.h / 2.0),
                &ChartView::default()
            ),
            Some(3),
            "a stale pixel from another layout must not still name frame 3"
        );
    }

    /// A click between two samples takes the nearer one, and a tie takes
    /// the earlier frame -- [`nearest_x`]'s rule, so a click and a hover
    /// at the same pixel cannot disagree.
    #[test]
    fn a_click_between_samples_takes_the_nearer_frame_and_ties_go_left() {
        let spec = selectable_spec();
        let plot = plot_for(&spec, CANVAS);
        let mid_y = plot.y + plot.h / 2.0;
        let (a, b) = (px_of(&spec, CANVAS, 2.0), px_of(&spec, CANVAS, 3.0));
        assert_eq!(
            frame_at(
                &spec,
                CANVAS,
                (a + (b - a) * 0.4, mid_y),
                &ChartView::default()
            ),
            Some(2)
        );
        assert_eq!(
            frame_at(
                &spec,
                CANVAS,
                (a + (b - a) * 0.6, mid_y),
                &ChartView::default()
            ),
            Some(3)
        );
        assert_eq!(
            frame_at(&spec, CANVAS, ((a + b) / 2.0, mid_y), &ChartView::default()),
            Some(2),
            "an exact tie resolves to the earlier frame, deterministically"
        );
    }

    /// Outside the plot area there is no sample to name: the legend band
    /// above it, the y-tick gutter left of it, and the x-caption strip
    /// below it are all margins, not data.
    #[test]
    fn a_click_outside_the_plot_area_selects_nothing() {
        let spec = selectable_spec();
        let plot = plot_for(&spec, CANVAS);
        let mid_x = plot.x + plot.w / 2.0;
        assert_eq!(
            frame_at(&spec, CANVAS, (mid_x, plot.y - 1.0), &ChartView::default()),
            None
        );
        assert_eq!(
            frame_at(
                &spec,
                CANVAS,
                (mid_x, plot.bottom() + 1.0),
                &ChartView::default()
            ),
            None
        );
        assert_eq!(
            frame_at(
                &spec,
                CANVAS,
                (plot.x - 1.0, plot.y + plot.h / 2.0),
                &ChartView::default()
            ),
            None
        );
        assert_eq!(
            frame_at(
                &spec,
                CANVAS,
                (plot.right() + 1.0, plot.y + plot.h / 2.0),
                &ChartView::default()
            ),
            None
        );
        // And a canvas smaller than its own margins has no plot at all.
        assert_eq!(
            frame_at(&spec, (4.0, 4.0), (2.0, 2.0), &ChartView::default()),
            None
        );
    }

    /// A chart that paints nothing has no frame under the cursor either,
    /// even though its x-axis would happily name one: `has_data` is false
    /// for a `Line` spec with an axis and no series, `build_chart_scene`
    /// early-outs on exactly that, and a click must early-out with it --
    /// otherwise a blank canvas answers a click with a frame and no
    /// crosshair to corroborate it.
    #[test]
    fn a_chart_that_paints_nothing_resolves_no_frame() {
        let blank = ChartSpec {
            series: Vec::new(),
            ..selectable_spec()
        };
        assert!(!blank.has_data());
        assert!(
            build_chart_scene(&blank, CANVAS, &ChartView::default())
                .primitives
                .is_empty(),
            "the precondition frame_at has to share"
        );
        let plot = plot_for(&blank, CANVAS);
        assert_eq!(
            frame_at(
                &blank,
                CANVAS,
                (plot.x + plot.w / 2.0, plot.y + plot.h / 2.0),
                &ChartView::default()
            ),
            None
        );
    }

    /// A histogram's x is empty (it plots a distribution, not a time
    /// series), so there is no frame under the cursor at any pixel.
    #[test]
    fn a_chart_with_no_frame_axis_never_resolves_a_frame() {
        let spec = ChartSpec {
            title: "i_misses distribution".to_string(),
            kind: ChartKind::Histogram,
            x: Vec::new(),
            series: vec![SeriesSpec {
                name: "i_misses".to_string(),
                color: Rgb(0x4a8fe3),
                values: vec![1.0, 2.0, 3.0],
            }],
            historic: Vec::new(),
            selectable: false,
        };
        let plot = plot_for(&spec, CANVAS);
        assert_eq!(
            frame_at(
                &spec,
                CANVAS,
                (plot.x + plot.w / 2.0, plot.y + plot.h / 2.0),
                &ChartView::default()
            ),
            None
        );
    }

    /// The anti-drift guard between the two geometries. `frame_at` and
    /// `build_chart_scene`'s hover readout resolve the same pixel through
    /// the same `plot_for` + `x_scale_for`; if a future change moves one of
    /// them, the click would select a frame the crosshair is not on, and
    /// nothing else in the suite would notice. Swept across the plot so an
    /// off-by-one at a single probe point cannot hide.
    ///
    /// **Now swept under a ZOOM as well**, which is the case R4 built this
    /// test for ("when R5 gives the panel a zoomed domain, changing only
    /// one of the two fails this"): with a zoom applied only inside
    /// `build_line`, the crosshair lands on the zoomed sample while the
    /// click still answers from the full domain. Drill: reverting
    /// `frame_at`'s `x_scale_for` to `x_scale(plot, full_x_domain(..))`
    /// fails this test and nothing else.
    #[test]
    fn the_click_and_the_hover_readout_resolve_the_same_sample() {
        let zoomed = ChartView {
            zoom: Some((1.0, 3.0)),
            ..ChartView::default()
        };
        let mut checked = 0;
        for spec in [selectable_spec(), line_spec()] {
            for base in [ChartView::default(), zoomed] {
                let plot = plot_for(&spec, CANVAS);
                let y = plot.y + plot.h / 2.0;
                for step in 0..=40 {
                    let x = plot.x + plot.w * (step as f32 / 40.0);
                    let hovering = ChartView {
                        hover: Some((x, y)),
                        ..base
                    };
                    let readout = build_chart_scene(&spec, CANVAS, &hovering)
                        .readout
                        .expect("inside the plot, over a sample");
                    let clicked =
                        frame_at(&spec, CANVAS, (x, y), &base).expect("same point, same answer");
                    assert_eq!(
                        clicked, spec.x[readout.index] as i64,
                        "click and crosshair disagree at x={x} on {} (zoom {:?})",
                        spec.title, base.zoom
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 4 * 41);
    }

    // ------------------------------------------------------ zoom + drag

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "{a} !~= {b}");
    }

    /// `line.rs`'s four `zoom_domain` tests, ported with it.
    #[test]
    fn zoom_domain_maps_a_drag_to_domain_values() {
        let scale = LinearScale {
            domain: (0.0, 100.0),
            range: (0.0, 500.0),
        };
        let (lo, hi) = zoom_domain(100.0, 200.0, &scale, (0.0, 100.0));
        approx(lo, 20.0);
        approx(hi, 40.0);
    }

    /// A drag of almost no width is widened to [`MIN_ZOOM_DOMAIN_WIDTH`]
    /// around its midpoint rather than collapsing the chart onto one
    /// sample. (The panel never gets here for a *click* -- `DRAG_MIN_PX`
    /// catches that first -- but a slow 4 px drag is a real gesture.)
    #[test]
    fn zoom_domain_widens_a_near_zero_drag() {
        let scale = LinearScale {
            domain: (0.0, 100.0),
            range: (0.0, 500.0),
        };
        let (lo, hi) = zoom_domain(100.0, 101.0, &scale, (0.0, 100.0));
        approx(hi - lo, MIN_ZOOM_DOMAIN_WIDTH);
        approx((lo + hi) / 2.0, 20.1);
    }

    #[test]
    fn zoom_domain_slides_back_inside_the_full_domain() {
        let scale = LinearScale {
            domain: (0.0, 100.0),
            range: (0.0, 500.0),
        };
        let (lo, hi) = zoom_domain(-50.0, 50.0, &scale, (0.0, 100.0));
        approx(lo, 0.0);
        approx(hi, 20.0);
    }

    #[test]
    fn zoom_domain_clamps_a_drag_wider_than_the_data() {
        let scale = LinearScale {
            domain: (0.0, 100.0),
            range: (0.0, 500.0),
        };
        let (lo, hi) = zoom_domain(-250.0, 750.0, &scale, (0.0, 100.0));
        approx(lo, 0.0);
        approx(hi, 100.0);
    }

    /// A zoomed chart plots ONLY the samples inside the window, spread
    /// across the whole plot -- and its x tick labels follow, because they
    /// are positioned through the same scale.
    #[test]
    fn a_zoomed_chart_maps_and_clips_to_its_window() {
        let spec = ChartSpec {
            x: (0..=10).map(|i| i as f32).collect(),
            series: vec![SeriesSpec {
                name: "wire_total".to_string(),
                color: Rgb(0x4a8fe3),
                values: (0..=10).map(|i| i as f32).collect(),
            }],
            kind: ChartKind::Line { budget: None },
            ..line_spec()
        };
        let view = ChartView {
            zoom: Some((4.0, 6.0)),
            ..ChartView::default()
        };
        let scene = build_chart_scene(&spec, CANVAS, &view);
        let plot = plot_for(&spec, CANVAS);
        let series = scene
            .primitives
            .iter()
            .find_map(|p| match p {
                Primitive::Polyline { points, width, .. } if *width == SERIES_STROKE_WIDTH => {
                    Some(points.clone())
                }
                _ => None,
            })
            .expect("the series is painted");
        assert_eq!(series.len(), 3, "frames 4, 5 and 6 only");
        approx(series[0].0, plot.x);
        approx(series[2].0, plot.right());
        // Frame 10 is outside the window, so nothing is drawn past the
        // plot's right edge (a clip, not an overflow).
        assert!(series.iter().all(|p| p.0 <= plot.right() + 1e-3));
    }

    /// The drag band spans the dragged pixels, clamped to the plot, and is
    /// there only while a drag is in flight.
    #[test]
    fn the_drag_band_covers_the_dragged_span() {
        let spec = line_spec();
        let plot = plot_for(&spec, CANVAS);
        let band = |view: &ChartView| {
            build_chart_scene(&spec, CANVAS, view)
                .primitives
                .iter()
                .find_map(|p| match p {
                    Primitive::Quad {
                        rect,
                        color: ChartColor::Selection,
                    } => Some(*rect),
                    _ => None,
                })
        };
        assert_eq!(band(&ChartView::default()), None, "no drag, no band");

        let dragging = ChartView {
            // Backwards (right to left) and past the left edge: the band
            // still reads left-to-right and stays inside the plot.
            drag: Some((plot.x + 100.0, plot.x - 40.0)),
            ..ChartView::default()
        };
        let rect = band(&dragging).expect("a drag paints its band");
        approx(rect.x, plot.x);
        approx(rect.right(), plot.x + 100.0);
        approx(rect.y, plot.y);
        approx(rect.h, plot.h);
    }

    // ------------------------------------------------- historic overlay

    /// A spec with one live series topping out at 40 and one overlay
    /// topping out at 400.
    fn overlaid_spec() -> ChartSpec {
        ChartSpec {
            historic: vec![OverlaySpec {
                values: vec![100.0, 200.0, 300.0, 400.0],
                opacity: 0.5,
            }],
            kind: ChartKind::Line { budget: None },
            ..line_spec()
        }
    }

    fn overlay_polyline(scene: &ChartScene) -> Option<Vec<(f32, f32)>> {
        scene.primitives.iter().find_map(|p| match p {
            Primitive::Polyline {
                points,
                color: ChartColor::Historic(_),
                ..
            } => Some(points.clone()),
            _ => None,
        })
    }

    /// The largest y-axis tick label a scene painted -- what the chart is
    /// scaled to, read back off the picture rather than recomputed.
    fn top_y_tick(scene: &ChartScene) -> f32 {
        scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Text {
                    content,
                    color: ChartColor::Text,
                    anchor: TextAnchor::Right,
                    ..
                } => content.replace(',', "").parse::<f32>().ok(),
                _ => None,
            })
            .fold(0.0f32, f32::max)
    }

    /// **The load-bearing clause.** An overlay four times taller than the
    /// live run has to move the y-scale, or every point of it maps above
    /// the plot and the reader is shown a blank where the comparison
    /// should be.
    ///
    /// Asserted two ways, because either alone is weak: the top y tick has
    /// to reach the overlay's own maximum, AND every painted overlay point
    /// has to land inside the plot rect. Drill: dropping the overlay fold
    /// out of `build_line`'s max fails both halves -- top tick 40, and the
    /// four overlay points at y = -234, -674, -1114, -1554.
    #[test]
    fn the_y_scale_includes_the_historic_overlays_extrema() {
        let spec = overlaid_spec();
        let view = ChartView {
            historic: true,
            ..ChartView::default()
        };
        let scene = build_chart_scene(&spec, CANVAS, &view);
        let plot = plot_for(&spec, CANVAS);

        let top_tick = top_y_tick(&scene);
        assert!(
            top_tick >= 400.0,
            "the y-axis must reach the overlay's 400, topped out at {top_tick}"
        );

        let points = overlay_polyline(&scene).expect("the overlay is painted");
        assert!(
            points
                .iter()
                .all(|(_, y)| *y >= plot.y - 1e-3 && *y <= plot.bottom() + 1e-3),
            "every overlay point must land inside the plot: {points:?}"
        );
    }

    /// ...and an overlay that is NOT being shown must not move the scale
    /// either -- rescaling a chart around a line the reader cannot see is
    /// the same bug from the other side.
    #[test]
    fn a_hidden_overlay_neither_paints_nor_moves_the_scale() {
        let spec = overlaid_spec();
        let scene = build_chart_scene(&spec, CANVAS, &ChartView::default());
        assert!(overlay_polyline(&scene).is_none(), "toggle off, no ghost");

        let plot = plot_for(&spec, CANVAS);
        let live = scene
            .primitives
            .iter()
            .find_map(|p| match p {
                Primitive::Polyline { points, width, .. } if *width == SERIES_STROKE_WIDTH => {
                    Some(points.clone())
                }
                _ => None,
            })
            .expect("the live series is painted");
        // Live max 40 against a nice top of 40 -> the last sample sits on
        // the plot's top edge. With the overlay folded in it would be at
        // 10% of the height instead.
        approx(live[3].1, plot.y);
    }

    /// Overlays are index-paired against the PRIMARY run's x axis: the
    /// k-th overlay value is plotted at the k-th frame's pixel, whatever
    /// frame numbers the prior run had. A shorter overlay stops partway
    /// across; a longer one is truncated at the axis's end.
    #[test]
    fn an_overlay_aligns_to_the_primary_runs_x_by_index() {
        // Primary x is 0..=3 (see `line_spec`). The overlay is shorter,
        // and its own frame numbering is deliberately unrelated.
        let spec = ChartSpec {
            historic: vec![
                OverlaySpec {
                    values: vec![10.0, 20.0],
                    opacity: 0.5,
                },
                OverlaySpec {
                    values: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
                    opacity: 0.36,
                },
            ],
            kind: ChartKind::Line { budget: None },
            ..line_spec()
        };
        let view = ChartView {
            historic: true,
            ..ChartView::default()
        };
        let scene = build_chart_scene(&spec, CANVAS, &view);
        let plot = plot_for(&spec, CANVAS);
        let xs = x_scale(plot, full_x_domain(&spec.x));
        let painted: Vec<Vec<(f32, f32)>> = scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Polyline {
                    points,
                    color: ChartColor::Historic(_),
                    ..
                } => Some(points.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(painted.len(), 2);

        let short = &painted[0];
        assert_eq!(short.len(), 2, "a shorter prior run stops partway");
        approx(short[0].0, xs.map(0.0));
        approx(short[1].0, xs.map(1.0));

        let long = &painted[1];
        assert_eq!(long.len(), 4, "a longer prior run is truncated at the axis");
        approx(long[3].0, xs.map(3.0));
    }

    /// The age ramp has to be VISIBLE, not merely present: R5's ancestor
    /// in F3 shipped two ghosts at the same alpha, which looks fine in the
    /// code and is invisible on screen. So: one distinct opacity per prior
    /// run, strictly dimmer as the runs get older.
    /// **Over the real ramp, not a copy of it.** Round 1 built this
    /// fixture from a hand-copied `[0.5, 0.36, 0.26, 0.18, 0.12]`, so it
    /// guarded the copy: worldlib could drift steps 3 and 4 to the same
    /// value -- the literal bug this test names -- and the whole suite
    /// stayed green. `HISTORIC_OPACITY` is what `chart_set` actually hangs
    /// on the specs, so `HISTORIC_OPACITY` is what has to be swept. (The
    /// "no worldlib import for five floats" excuse was also just wrong:
    /// this module already imports `charts::scale` from it.)
    ///
    /// Every step is covered, not just the first pair: with fewer than
    /// five prior runs the ramp is indexed from the bright end, so a
    /// collision anywhere in it is reachable.
    #[test]
    fn the_age_ramp_gives_each_prior_run_a_distinguishable_opacity() {
        let spec = ChartSpec {
            historic: HISTORIC_OPACITY
                .iter()
                .map(|&opacity| OverlaySpec {
                    values: vec![1.0, 2.0, 3.0, 4.0],
                    opacity,
                })
                .collect(),
            kind: ChartKind::Line { budget: None },
            ..line_spec()
        };
        let view = ChartView {
            historic: true,
            ..ChartView::default()
        };
        let scene = build_chart_scene(&spec, CANVAS, &view);
        let alphas: Vec<f32> = scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Polyline {
                    color: ChartColor::Historic(a),
                    ..
                } => Some(*a),
                _ => None,
            })
            .collect();
        assert_eq!(alphas.len(), HISTORIC_OPACITY.len());
        for pair in alphas.windows(2) {
            assert!(
                pair[0] > pair[1],
                "each older run must be strictly dimmer than the one before it: {alphas:?}"
            );
        }
        // Distinguishable on screen, not merely unequal: the painter maps
        // these through `Hsla::opacity`, and 8-bit alpha is what a reader
        // gets. A ramp whose adjacent steps round to the same byte would
        // pass the loop above and still show two identical ghosts.
        let bytes: Vec<u8> = alphas.iter().map(|a| (a * 255.0).round() as u8).collect();
        for pair in bytes.windows(2) {
            assert!(
                pair[0] > pair[1],
                "the ramp has to survive 8-bit alpha, not just f32: {bytes:?}"
            );
        }
    }

    /// **A ghost that cannot be drawn must not move the scale either.**
    ///
    /// One frame survives the ignore filter of a two-frame capture -- an
    /// aborted run, entirely ordinary -- and a one-point polyline strokes
    /// nothing. R5 round 1 folded that single value into the y-max and
    /// then painted no line, so the current run's chart silently rescaled
    /// around something invisible: `top_tick=500` with the live series
    /// maxing at 40, collapsing it to ~8% of the plot with no cause on
    /// screen. `drawn_overlays` is the one list both halves read, so the
    /// two cannot part company again.
    #[test]
    fn an_overlay_too_short_to_draw_is_left_out_of_the_scale_too() {
        let one_sample = ChartSpec {
            historic: vec![OverlaySpec {
                values: vec![500.0],
                opacity: 0.5,
            }],
            kind: ChartKind::Line { budget: None },
            ..line_spec()
        };
        let view = ChartView {
            historic: true,
            ..ChartView::default()
        };
        let scene = build_chart_scene(&one_sample, CANVAS, &view);
        assert!(
            overlay_polyline(&scene).is_none(),
            "one point strokes no line -- that is the premise"
        );
        assert_eq!(
            top_y_tick(&scene),
            40.0,
            "so the scale must still be the live run's, not the ghost's 500"
        );

        // ...and an overlay with two points does both.
        let two_samples = ChartSpec {
            historic: vec![OverlaySpec {
                values: vec![500.0, 500.0],
                opacity: 0.5,
            }],
            ..one_sample
        };
        let scene = build_chart_scene(&two_samples, CANVAS, &view);
        assert!(overlay_polyline(&scene).is_some());
        assert!(top_y_tick(&scene) >= 500.0);
    }

    /// The rule `has_data`'s doc states, pinned: an overlay is context for
    /// a run, not a run. A spec carrying ghosts and no live series draws
    /// nothing and answers no click -- and `frame_at` shares that
    /// early-out, so it cannot resolve a frame on a canvas full of grey
    /// either (R4's F5, now with a second way in).
    #[test]
    fn a_chart_with_only_overlays_has_no_data() {
        let ghosts_only = ChartSpec {
            series: Vec::new(),
            historic: vec![OverlaySpec {
                values: vec![1.0, 2.0, 3.0, 4.0],
                opacity: 0.5,
            }],
            ..line_spec()
        };
        assert!(!ghosts_only.has_data());
        let view = ChartView {
            historic: true,
            ..ChartView::default()
        };
        assert!(
            build_chart_scene(&ghosts_only, CANVAS, &view)
                .primitives
                .is_empty()
        );
        let plot = plot_for(&ghosts_only, CANVAS);
        assert_eq!(
            frame_at(
                &ghosts_only,
                CANVAS,
                (plot.x + plot.w / 2.0, plot.y + plot.h / 2.0),
                &view
            ),
            None
        );
    }

    /// Overlays are drawn UNDER the live series, and thinner -- a ghost
    /// that painted over the run being read would be worse than no ghost.
    #[test]
    fn overlays_are_painted_beneath_the_live_series() {
        let spec = overlaid_spec();
        let view = ChartView {
            historic: true,
            ..ChartView::default()
        };
        let scene = build_chart_scene(&spec, CANVAS, &view);
        let ghost = scene
            .primitives
            .iter()
            .position(|p| {
                matches!(
                    p,
                    Primitive::Polyline {
                        color: ChartColor::Historic(_),
                        ..
                    }
                )
            })
            .expect("the overlay is painted");
        let live = scene
            .primitives
            .iter()
            .position(|p| {
                matches!(
                    p,
                    Primitive::Polyline {
                        color: ChartColor::Series(_),
                        ..
                    }
                )
            })
            .expect("the live series is painted");
        assert!(ghost < live, "the overlay must be painted first");
        assert!(HISTORIC_STROKE_WIDTH < SERIES_STROKE_WIDTH);
    }

    /// An overlay is clipped to the zoom window like everything else, so a
    /// zoomed-in chart does not paint ghosts across its own axis labels.
    #[test]
    fn a_zoomed_chart_clips_its_overlays_too() {
        let spec = overlaid_spec();
        let view = ChartView {
            historic: true,
            zoom: Some((1.0, 2.0)),
            ..ChartView::default()
        };
        let scene = build_chart_scene(&spec, CANVAS, &view);
        let plot = plot_for(&spec, CANVAS);
        let points = overlay_polyline(&scene).expect("the overlay is still painted");
        assert_eq!(points.len(), 2, "frames 1 and 2 only");
        assert!(
            points
                .iter()
                .all(|(x, _)| *x >= plot.x - 1e-3 && *x <= plot.right() + 1e-3)
        );
    }

    #[test]
    fn slot_index_clamps_into_the_bin_range() {
        assert_eq!(slot_index(0.0, 0.0, 10.0, 5), 0);
        assert_eq!(slot_index(25.0, 0.0, 10.0, 5), 2);
        assert_eq!(slot_index(-100.0, 0.0, 10.0, 5), 0);
        assert_eq!(slot_index(1_000.0, 0.0, 10.0, 5), 4);
        // Degenerate inputs must not divide by zero or panic.
        assert_eq!(slot_index(5.0, 0.0, 0.0, 5), 0);
        assert_eq!(slot_index(5.0, 0.0, 10.0, 0), 0);
    }

    // ----------------------------------------------------------- formatting

    #[test]
    fn format_tick_mirrors_fmt_compact() {
        assert_eq!(format_tick(0.0), "0");
        assert_eq!(format_tick(20.0), "20");
        assert_eq!(format_tick(20_000.0), "20k");
        assert_eq!(format_tick(1_234_567.0), "1.2M");
        assert_eq!(format_tick(0.001), "0.001");
        assert_eq!(format_tick(2.5), "2.5");
    }

    #[test]
    fn with_thousands_mirrors_fmt_int() {
        assert_eq!(with_thousands(555), "555");
        assert_eq!(with_thousands(1_234_567), "1,234,567");
        assert_eq!(with_thousands(-12_000), "-12,000");
    }

    // ----------------------------------------------------------------- bins

    #[test]
    fn bins_of_identical_values_is_one_unit_wide_bin() {
        let b = bins(&[5.0, 5.0, 5.0], HISTOGRAM_BIN_TARGET);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].count, 3);
        assert_eq!(b[0].hi - b[0].lo, 1.0);
    }

    #[test]
    fn bins_drops_non_finite_values() {
        assert_eq!(
            bins(&[1.0, f32::NAN, 2.0, f32::INFINITY, 3.0], 5),
            bins(&[1.0, 2.0, 3.0], 5)
        );
        assert!(bins(&[f32::NAN], 5).is_empty());
    }

    #[test]
    fn bins_of_nothing_is_empty() {
        assert!(bins(&[], HISTOGRAM_BIN_TARGET).is_empty());
    }

    #[test]
    fn bin_label_is_a_single_value_for_a_unit_step() {
        assert_eq!(
            bin_label(&Bin {
                lo: 5.0,
                hi: 6.0,
                count: 1
            }),
            "5"
        );
        assert_eq!(
            bin_label(&Bin {
                lo: 1_000.0,
                hi: 1_100.0,
                count: 1
            }),
            "1,000\u{2013}1,100"
        );
    }

    // ------------------------------------------------------------ stacking

    #[test]
    fn accumulate_builds_prefix_sums_and_pads_short_series() {
        let cum = accumulate(&[vec![1.0, 2.0], vec![10.0]]);
        assert_eq!(cum, vec![vec![1.0, 2.0], vec![11.0, 2.0]]);
    }

    #[test]
    fn accumulate_of_no_series_is_empty() {
        assert_eq!(accumulate(&[]), Vec::<Vec<f32>>::new());
    }

    // ---------------------------------------------------------- decimation

    #[test]
    fn envelope_leaves_a_short_series_untouched() {
        let points = [(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)];
        assert_eq!(envelope(&points, MAX_PLOT_POINTS), points.to_vec());
    }

    #[test]
    fn envelope_caps_a_long_series_at_max_points() {
        let points: Vec<(f32, f32)> = (0..10_000).map(|i| (i as f32, (i % 97) as f32)).collect();
        let out = envelope(&points, 64);
        assert!(out.len() <= 64, "got {}", out.len());
        assert!(out.len() >= 32);
    }

    /// The whole point of min/max over a stride: a single-frame spike --
    /// the over-budget frame a user is hunting -- must survive
    /// decimation.
    #[test]
    fn envelope_preserves_an_isolated_spike() {
        let mut points: Vec<(f32, f32)> = (0..1_000).map(|i| (i as f32, 1.0)).collect();
        points[437] = (437.0, 999.0);
        let out = envelope(&points, 16);
        assert!(
            out.iter().any(|p| p.1 == 999.0),
            "the spike must survive decimation"
        );
    }

    /// Points stay in x order, so the polyline doesn't zigzag backwards.
    #[test]
    fn envelope_keeps_points_in_index_order() {
        let points: Vec<(f32, f32)> = (0..500)
            .map(|i| (i as f32, ((i * 7) % 31) as f32))
            .collect();
        let out = envelope(&points, 32);
        assert!(out.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn stride_indices_keeps_short_axes_whole_and_always_ends_on_the_last() {
        assert_eq!(stride_indices(0, 8), Vec::<usize>::new());
        assert_eq!(stride_indices(4, 8), vec![0, 1, 2, 3]);
        let out = stride_indices(1_000, 10);
        assert!(out.len() <= 11, "got {}", out.len());
        assert_eq!(out[0], 0);
        assert_eq!(*out.last().unwrap(), 999);
        assert!(out.windows(2).all(|w| w[0] < w[1]));
    }

    // -------------------------------------------------------------- legend

    #[test]
    fn legend_lays_entries_left_to_right_on_one_row_when_they_fit() {
        let entries = [
            ("a".to_string(), Rgb(0x111111)),
            ("b".to_string(), Rgb(0x222222)),
        ];
        let laid = legend_layout(&entries, 10.0, 0.0, 500.0);
        assert_eq!(laid.len(), 2);
        assert_eq!(laid[0].swatch.x, 10.0);
        assert!(laid[1].swatch.x > laid[0].swatch.x);
        assert_eq!(laid[0].swatch.y, laid[1].swatch.y, "same row");
        assert_eq!(legend_height(&laid), LEGEND_ROW_HEIGHT);
    }

    #[test]
    fn legend_wraps_onto_a_second_row_when_out_of_width() {
        let entries = [
            ("scanout_wire".to_string(), Rgb(0x111111)),
            ("blit_wire".to_string(), Rgb(0x222222)),
            ("miss_wire".to_string(), Rgb(0x333333)),
        ];
        let laid = legend_layout(&entries, 0.0, 0.0, 120.0);
        assert_eq!(laid.len(), 3);
        assert!(
            laid.iter().any(|e| e.swatch.y > laid[0].swatch.y),
            "a 120px-wide legend must wrap three ~80px entries"
        );
        assert!(legend_height(&laid) > LEGEND_ROW_HEIGHT);
    }

    /// The first entry on a row is placed even if it alone overflows --
    /// a very long series name gets its own row rather than vanishing.
    #[test]
    fn legend_never_drops_an_over_wide_entry() {
        let entries = [("a_very_long_series_name_indeed".to_string(), Rgb(0x111111))];
        let laid = legend_layout(&entries, 0.0, 0.0, 10.0);
        assert_eq!(laid.len(), 1);
        assert_eq!(laid[0].swatch.x, 0.0);
    }

    #[test]
    fn legend_height_of_nothing_is_zero() {
        assert_eq!(legend_height(&[]), 0.0);
    }

    // --------------------------------------------------------------- scenes

    fn text_contents(scene: &ChartScene) -> Vec<&str> {
        scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Text { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn line_scene_has_axes_series_legend_and_a_budget_line() {
        let scene = build_chart_scene(&line_spec(), CANVAS, &ChartView::default());
        assert!(!scene.primitives.is_empty());
        assert!(
            scene
                .primitives
                .iter()
                .any(|p| matches!(p, Primitive::Polyline { .. })),
            "the series polyline must be painted"
        );
        assert!(
            scene.primitives.iter().any(|p| matches!(
                p,
                Primitive::Segment {
                    color: ChartColor::Budget,
                    dash: Some(_),
                    ..
                }
            )),
            "the budget reference must be a dashed line"
        );
        let texts = text_contents(&scene);
        assert!(texts.contains(&"frame"), "x-axis caption");
        assert!(texts.contains(&"wire_total"), "legend label");
        assert!(texts.contains(&"0"), "y tick label at zero");
        assert!(scene.readout.is_none(), "no hover -> no readout");
    }

    #[test]
    fn stacked_scene_paints_one_band_per_series() {
        let spec = ChartSpec {
            title: "Wire breakdown per frame".to_string(),
            kind: ChartKind::Stacked,
            x: vec![0.0, 1.0, 2.0],
            series: vec![
                SeriesSpec {
                    name: "scanout_wire".to_string(),
                    color: Rgb(0x4a8fe3),
                    values: vec![1.0, 2.0, 3.0],
                },
                SeriesSpec {
                    name: "blit_wire".to_string(),
                    color: Rgb(0xdb8c33),
                    values: vec![4.0, 5.0, 6.0],
                },
            ],
            historic: Vec::new(),
            selectable: false,
        };
        let scene = build_chart_scene(&spec, CANVAS, &ChartView::default());
        let bands = scene
            .primitives
            .iter()
            .filter(|p| matches!(p, Primitive::Polygon { .. }))
            .count();
        assert_eq!(bands, 2);
        // One separator between two adjacent bands.
        let separators = scene
            .primitives
            .iter()
            .filter(|p| {
                matches!(
                    p,
                    Primitive::Polyline {
                        color: ChartColor::Surface,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(separators, 1);
    }

    #[test]
    fn histogram_scene_paints_one_quad_per_nonempty_bin() {
        let spec = ChartSpec {
            title: "i_misses distribution".to_string(),
            kind: ChartKind::Histogram,
            x: Vec::new(),
            series: vec![SeriesSpec {
                name: "i_misses".to_string(),
                color: Rgb(0x4a8fe3),
                values: vec![1.0, 1.0, 2.0, 3.0],
            }],
            historic: Vec::new(),
            selectable: false,
        };
        let scene = build_chart_scene(&spec, CANVAS, &ChartView::default());
        let bars = scene
            .primitives
            .iter()
            .filter(|p| matches!(p, Primitive::Quad { .. }))
            .count();
        assert!(bars > 0, "at least one bar must be painted");
        // A histogram has no legend band and no "frame" caption.
        assert!(!text_contents(&scene).contains(&"frame"));
    }

    /// Bars are the THEME accent, not the metric's fixed series hue --
    /// `Histogram.tsx`/`histogram.rs` fill from
    /// `palette.primary.base.color` and their widget takes no series
    /// color at all. The resting bars are the translucent variant so the
    /// hovered one lifts.
    #[test]
    fn histogram_bars_use_the_theme_accent_not_the_series_color() {
        let spec = ChartSpec {
            title: "i_misses distribution".to_string(),
            kind: ChartKind::Histogram,
            x: Vec::new(),
            series: vec![SeriesSpec {
                name: "i_misses".to_string(),
                color: Rgb(0x4a8fe3),
                values: vec![1.0, 1.0, 2.0, 3.0],
            }],
            historic: Vec::new(),
            selectable: false,
        };
        let scene = build_chart_scene(&spec, CANVAS, &ChartView::default());
        let bar_colors: Vec<ChartColor> = scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Quad { color, .. } => Some(*color),
                _ => None,
            })
            .collect();
        assert!(!bar_colors.is_empty());
        assert!(
            bar_colors
                .iter()
                .all(|c| matches!(c, ChartColor::AccentAlpha(_))),
            "un-hovered bars must be the translucent accent, got {bar_colors:?}"
        );

        // The hovered bar lifts to the full-opacity accent.
        let plot = plot_rect(CANVAS, HISTOGRAM_MARGINS, 0.0);
        let slot_w = plot.w / bins(&[1.0, 1.0, 2.0, 3.0], HISTOGRAM_BIN_TARGET).len() as f32;
        let hovered = build_chart_scene(
            &spec,
            CANVAS,
            &ChartView::hovering((plot.x + slot_w * 0.5, plot.y + plot.h / 2.0)),
        );
        assert!(
            hovered.primitives.iter().any(|p| matches!(
                p,
                Primitive::Quad {
                    color: ChartColor::Accent,
                    ..
                }
            )),
            "the hovered bar must paint at full accent opacity"
        );
    }

    /// The empty/degenerate guards: no samples, or a canvas too small for
    /// its own margins, yield an EMPTY scene so the panel's explicit
    /// message shows instead of a blank canvas.
    #[test]
    fn a_spec_with_no_samples_or_a_tiny_canvas_builds_an_empty_scene() {
        let empty = ChartSpec {
            x: Vec::new(),
            series: Vec::new(),
            ..line_spec()
        };
        assert!(!empty.has_data());
        assert!(
            build_chart_scene(&empty, CANVAS, &ChartView::default())
                .primitives
                .is_empty()
        );
        assert!(
            build_chart_scene(&line_spec(), (8.0, 8.0), &ChartView::default())
                .primitives
                .is_empty()
        );
    }

    // ---------------------------------------------------------------- hover

    /// Hover at the pixel the 3rd sample maps to: the readout names that
    /// sample, its exact value, and the frame number as its title.
    #[test]
    fn hover_over_a_line_chart_reads_out_the_nearest_sample() {
        let spec = line_spec();
        let plot = plot_rect(CANVAS, LINE_MARGINS, LEGEND_ROW_HEIGHT);
        let xs = x_scale(plot, full_x_domain(&spec.x));
        let px = xs.map(2.0);
        let scene = build_chart_scene(
            &spec,
            CANVAS,
            &ChartView::hovering((px, plot.y + plot.h / 2.0)),
        );
        let readout = scene.readout.expect("hover inside the plot must read out");
        assert_eq!(readout.index, 2);
        assert_eq!(readout.title, "frame 2");
        assert_eq!(readout.rows.len(), 1);
        assert_eq!(readout.rows[0].label, "wire_total");
        assert_eq!(readout.rows[0].value, "30");
        // The tooltip box + crosshair are painted too.
        assert!(
            scene
                .primitives
                .iter()
                .any(|p| matches!(p, Primitive::Outline { .. })),
            "the readout box border must be painted"
        );
    }

    #[test]
    fn hover_outside_the_plot_area_reads_out_nothing() {
        let scene = build_chart_scene(&line_spec(), CANVAS, &ChartView::hovering((2.0, 2.0)));
        assert!(scene.readout.is_none());
    }

    /// A stacked chart's readout appends the `total` row after the
    /// per-series ones -- `StackedArea.tsx`'s tooltip.
    #[test]
    fn hover_over_a_stacked_chart_appends_a_total_row() {
        let spec = ChartSpec {
            title: "Wire breakdown per frame".to_string(),
            kind: ChartKind::Stacked,
            x: vec![0.0, 1.0],
            series: vec![
                SeriesSpec {
                    name: "scanout_wire".to_string(),
                    color: Rgb(0x4a8fe3),
                    values: vec![1_000.0, 2_000.0],
                },
                SeriesSpec {
                    name: "blit_wire".to_string(),
                    color: Rgb(0xdb8c33),
                    values: vec![3.0, 4.0],
                },
            ],
            historic: Vec::new(),
            selectable: false,
        };
        let legend = legend_layout(
            &[
                ("scanout_wire".to_string(), Rgb(0x4a8fe3)),
                ("blit_wire".to_string(), Rgb(0xdb8c33)),
            ],
            STACKED_MARGINS.left,
            0.0,
            (CANVAS.0 - STACKED_MARGINS.left - STACKED_MARGINS.right).max(0.0),
        );
        let plot = plot_rect(CANVAS, STACKED_MARGINS, legend_height(&legend));
        let xs = x_scale(plot, full_x_domain(&spec.x));
        let scene = build_chart_scene(
            &spec,
            CANVAS,
            &ChartView::hovering((xs.map(1.0), plot.y + plot.h / 2.0)),
        );
        let readout = scene.readout.expect("hover must read out");
        assert_eq!(readout.index, 1);
        let labels: Vec<&str> = readout.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["scanout_wire", "blit_wire", "total"]);
        assert_eq!(readout.rows[0].value, "2,000", "exact, comma-grouped");
        assert_eq!(readout.rows[2].value, "2,004");
        assert_eq!(readout.rows[2].color, None, "the total row has no series");
    }

    /// A histogram's readout names the bin and its frame count, singular
    /// for a count of 1 -- `Histogram.tsx`'s `frame`/`frames` row label.
    #[test]
    fn hover_over_a_histogram_reads_out_the_hovered_bin() {
        let spec = ChartSpec {
            title: "i_misses distribution".to_string(),
            kind: ChartKind::Histogram,
            x: Vec::new(),
            series: vec![SeriesSpec {
                name: "i_misses".to_string(),
                color: Rgb(0x4a8fe3),
                values: vec![0.0, 0.0, 1.0],
            }],
            historic: Vec::new(),
            selectable: false,
        };
        let plot = plot_rect(CANVAS, HISTOGRAM_MARGINS, 0.0);
        let bin_list = bins(&[0.0, 0.0, 1.0], HISTOGRAM_BIN_TARGET);
        let slot_w = plot.w / bin_list.len() as f32;
        // Hover the first bin's slot.
        let scene = build_chart_scene(
            &spec,
            CANVAS,
            &ChartView::hovering((plot.x + slot_w * 0.5, plot.y + plot.h / 2.0)),
        );
        let readout = scene.readout.expect("hover inside the plot must read out");
        assert_eq!(readout.index, 0);
        assert_eq!(readout.rows[0].label, "frames");
        assert_eq!(readout.rows[0].value, "2");

        // The second bin holds exactly one frame -> singular label.
        let scene = build_chart_scene(
            &spec,
            CANVAS,
            &ChartView::hovering((plot.x + slot_w * 1.5, plot.y + plot.h / 2.0)),
        );
        let readout = scene.readout.expect("hover inside the plot must read out");
        assert_eq!(readout.index, 1);
        assert_eq!(readout.rows[0].label, "frame");
        assert_eq!(readout.rows[0].value, "1");
    }

    /// A single-sample run still renders and still reads out -- the
    /// degenerate x-domain path (`LinearScale::map` returns `range.0` for
    /// a zero-span domain).
    #[test]
    fn a_single_sample_run_renders_and_reads_out() {
        let spec = ChartSpec {
            x: vec![5.0],
            series: vec![SeriesSpec {
                name: "wire_total".to_string(),
                color: Rgb(0x4a8fe3),
                values: vec![42.0],
            }],
            kind: ChartKind::Line { budget: None },
            ..line_spec()
        };
        let plot = plot_rect(CANVAS, LINE_MARGINS, LEGEND_ROW_HEIGHT);
        let scene = build_chart_scene(
            &spec,
            CANVAS,
            &ChartView::hovering((plot.x + plot.w / 2.0, plot.y + plot.h / 2.0)),
        );
        assert!(!scene.primitives.is_empty());
        let readout = scene.readout.expect("a one-sample run still reads out");
        assert_eq!(readout.index, 0);
        assert_eq!(readout.title, "frame 5");
        assert_eq!(readout.rows[0].value, "42");
    }
}
