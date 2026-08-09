//! Which charts a selected run shows, and with what series -- a
//! one-for-one mirror of ggo-ide's Reports run-detail page
//! (`tools/ggo-ide/src/pages/reports.rs::charts_section`), so a run looks
//! the same in this panel as it does there.
//!
//! Kept separate from `chart_geom` (HOW a chart is drawn) and `loader`
//! (WHERE the numbers come from): this module is only the mapping from
//! sampled `FrameRow`/`ProfileRow`s to [`ChartSpec`]s, and is pure, so
//! the whole chart set is unit-testable without a window or a database.

use std::collections::HashMap;

use crate::chart_geom::{ChartKind, ChartSpec, Rgb, SeriesSpec};
use crate::loader::{FrameRow, ProfileRow, RunSamples};

// ---------------------------------------------------------------- palette

// ggo-ide's fixed chart palette (`reports.rs:186-227`), which is itself
// the Tauri IDE's `index.css` `--c1..--c6` roles. Carried over as packed
// hex: `reports.rs` stores them as `Color::from_rgb` floats, and these are
// those floats times 255, rounded (`c1 (0.29,0.56,0.89)` -> `0x4a8fe3`,
// `c2 (0.86,0.55,0.20)` -> `0xdb8c33`, `c3 (0.80,0.30,0.30)` -> `0xcc4d4d`);
// `c4`/`c5`/`c6` were already hex-exact there and are copied verbatim.
// Fixed, not theme-derived, for the same reason ggo-ide fixes them: a
// series' identity ("blit_wire is orange") has to survive a theme switch,
// or two runs compared side by side stop being comparable.

/// `--c1`: `wire_total`, `scanout_wire`, `i_misses`, `instrs`, and the
/// first band/series of every multi-series chart.
const C1: Rgb = Rgb(0x4a8fe3);
/// `--c2`: `blit_wire`, `d_misses`, `sc_oam`, `spr_tiles_distinct`, ...
const C2: Rgb = Rgb(0xdb8c33);
/// `--c3`: `miss_wire`, `sc_layer`, `spr_evictions`.
const C3: Rgb = Rgb(0xcc4d4d);
/// `--c4: #008300`.
const C4: Rgb = Rgb(0x008300);
/// `--c5: #4a3aa7`.
const C5: Rgb = Rgb(0x4a3aa7);
/// `--c6: #e34948`.
const C6: Rgb = Rgb(0xe34948);

/// `reports.rs`'s `PROF_COLORS`: five per-function slots plus a dedicated
/// sixth for the `"other"` bucket.
const PROF_COLORS: [Rgb; 6] = [C1, C2, C3, C4, C5, C6];
/// `reports.rs`'s `TOP_FUNCTIONS_TAKE` (`PROF_COLORS.len() - 1`).
const TOP_FUNCTIONS_TAKE: usize = PROF_COLORS.len() - 1;
const OTHER_FUNCTION_NAME: &str = "other";

/// `reports.rs`'s `kpi::TILE_CACHE_TILES`: the 8 KB / 128 B-per-tile cache
/// capacity the tile-working-set chart draws as its reference line. It is
/// a CAPACITY, not a budget, but ggo-ide draws it with the same dashed
/// danger-colored line, so this port does too.
const TILE_CACHE_TILES: f32 = 64.0;

/// `reports.rs`'s `DEFAULT_IGNORED_FRAME`: frame 0 is dropped from every
/// chart, because a cold-cache first frame (every tile a miss, every
/// asset a fresh upload) is an outlier that flattens the rest of the run.
/// ggo-ide lets a user edit this set with a chip editor; this panel has no
/// such editor yet, so the default is all there is -- see this task's
/// report.
const DEFAULT_IGNORED_FRAME: i64 = 0;

// ------------------------------------------------------------------ gates

/// `reports.rs`'s `chart_gates`, verbatim -- a chart whose columns are
/// zero across every frame is hidden rather than drawn flat.
mod gates {
    use crate::loader::FrameRow;

    pub fn has_syscalls(frames: &[FrameRow]) -> bool {
        frames
            .iter()
            .any(|f| f.sc_upload + f.sc_oam + f.sc_layer + f.sc_audio + f.sc_other > 0)
    }

    pub fn has_tile_working_set(frames: &[FrameRow]) -> bool {
        frames
            .iter()
            .any(|f| f.peak_spr_line + f.bg_tiles_distinct + f.spr_tiles_distinct > 0)
    }

    pub fn has_ppu(frames: &[FrameRow]) -> bool {
        frames
            .iter()
            .any(|f| f.bg_evictions + f.fg_evictions + f.spr_evictions + f.tile_load_wire > 0)
    }

    pub fn has_apu(frames: &[FrameRow]) -> bool {
        frames
            .iter()
            .any(|f| f.apu_fetch_wire > 0 || f.apu_underruns > 0)
    }
}

// ------------------------------------------------------------- assembly

fn series(name: &str, color: Rgb, frames: &[FrameRow], get: fn(&FrameRow) -> i64) -> SeriesSpec {
    SeriesSpec {
        name: name.to_string(),
        color,
        values: frames.iter().map(|f| get(f) as f32).collect(),
    }
}

fn line(title: &str, x: &[f32], budget: Option<f32>, series: Vec<SeriesSpec>) -> ChartSpec {
    ChartSpec {
        title: title.to_string(),
        kind: ChartKind::Line { budget },
        x: x.to_vec(),
        series,
    }
}

fn stacked(title: &str, x: &[f32], series: Vec<SeriesSpec>) -> ChartSpec {
    ChartSpec {
        title: title.to_string(),
        kind: ChartKind::Stacked,
        x: x.to_vec(),
        series,
    }
}

fn histogram(title: &str, name: &str, frames: &[FrameRow], get: fn(&FrameRow) -> i64) -> ChartSpec {
    ChartSpec {
        title: title.to_string(),
        kind: ChartKind::Histogram,
        // A histogram plots a DISTRIBUTION, not a time series: it has no
        // frame axis at all (`Histogram.tsx` never receives one).
        x: Vec::new(),
        series: vec![series(name, C1, frames, get)],
    }
}

/// Drops the ignored frames (currently just frame 0) --
/// `reports.rs`'s `ignore::apply`.
fn apply_ignore(frames: &[FrameRow]) -> Vec<FrameRow> {
    frames
        .iter()
        .filter(|f| f.n != DEFAULT_IGNORED_FRAME)
        .cloned()
        .collect()
}

/// How many of `frames` the ignore filter drops -- the panel captions
/// this ("1 frame ignored") so a user isn't left wondering why the x-axis
/// starts at 1. ggo-ide surfaces the same fact through its chip editor,
/// which this panel doesn't have yet.
pub fn ignored_count(frames: &[FrameRow]) -> usize {
    frames
        .iter()
        .filter(|f| f.n == DEFAULT_IGNORED_FRAME)
        .count()
}

/// `reports.rs`'s `ignore::apply_profile`.
fn apply_ignore_profile(rows: &[ProfileRow]) -> Vec<ProfileRow> {
    rows.iter()
        .filter(|r| r.frame != DEFAULT_IGNORED_FRAME)
        .cloned()
        .collect()
}

/// Every chart a selected run shows, in ggo-ide's exact top-to-bottom
/// order. Empty when the run has no frames left after the ignore filter
/// (the panel shows an explicit message instead).
///
/// The two per-function charts (`I$ misses by function` / `I$ eviction
/// victims by function`) appear only when the run carries profile rows,
/// i.e. only for a native `ggo-emu --profile <elf>` capture; the syscall,
/// tile-working-set, PPU and APU charts each have their own
/// zero-columns gate ([`gates`]). Everything else is unconditional.
pub fn build_charts(samples: &RunSamples) -> Vec<ChartSpec> {
    let frames = apply_ignore(&samples.frames);
    if frames.is_empty() {
        return Vec::new();
    }
    let profile = apply_ignore_profile(&samples.profile);

    let x: Vec<f32> = frames.iter().map(|f| f.n as f32).collect();
    let frame_axis: Vec<i64> = frames.iter().map(|f| f.n).collect();
    // Every frame row carries the joined `run.frame_budget_cycles`, so
    // the first row's copy is the run's budget (ggo-ide reads the same
    // value off `RunDetail`, from the same column -- one query fewer here).
    let budget = frames
        .first()
        .and_then(|f| f.frame_budget_cycles)
        .map(|b| b as f32);

    let mut charts = vec![
        line(
            "Wire cycles per frame vs budget",
            &x,
            budget,
            vec![series("wire_total", C1, &frames, |f| f.wire_total)],
        ),
        stacked(
            "Wire breakdown per frame",
            &x,
            vec![
                series("scanout_wire", C1, &frames, |f| f.scanout_wire),
                series("blit_wire", C2, &frames, |f| f.blit_wire),
                series("miss_wire", C3, &frames, |f| f.miss_wire),
            ],
        ),
        line(
            "Cache misses per frame",
            &x,
            None,
            vec![
                series("i_misses", C1, &frames, |f| f.i_misses),
                series("d_misses", C2, &frames, |f| f.d_misses),
            ],
        ),
    ];

    if gates::has_syscalls(&frames) {
        charts.push(stacked(
            "Syscalls per frame",
            &x,
            vec![
                series("sc_upload", C1, &frames, |f| f.sc_upload),
                series("sc_oam", C2, &frames, |f| f.sc_oam),
                series("sc_layer", C3, &frames, |f| f.sc_layer),
                series("sc_audio", C4, &frames, |f| f.sc_audio),
                series("sc_other", C5, &frames, |f| f.sc_other),
            ],
        ));
    }

    if gates::has_tile_working_set(&frames) {
        charts.push(line(
            "Tile working set vs cache capacity",
            &x,
            Some(TILE_CACHE_TILES),
            vec![
                series("bg_tiles_distinct", C1, &frames, |f| f.bg_tiles_distinct),
                series("spr_tiles_distinct", C2, &frames, |f| f.spr_tiles_distinct),
            ],
        ));
    }

    if !profile.is_empty() {
        let misses = top_functions(&profile, &frame_axis, TOP_FUNCTIONS_TAKE);
        let evicted = pivot_evicted(&misses, &profile, &frame_axis);
        charts.push(line("I$ misses by function", &x, None, misses));
        charts.push(line("I$ eviction victims by function", &x, None, evicted));
    }

    charts.push(histogram(
        "i_misses distribution",
        "i_misses",
        &frames,
        |f| f.i_misses,
    ));
    charts.push(histogram(
        "d_misses distribution",
        "d_misses",
        &frames,
        |f| f.d_misses,
    ));

    if gates::has_ppu(&frames) {
        charts.push(line(
            "PPU tile-cache evictions per frame",
            &x,
            None,
            vec![
                series("bg_evictions", C1, &frames, |f| f.bg_evictions),
                series("fg_evictions", C2, &frames, |f| f.fg_evictions),
                series("spr_evictions", C3, &frames, |f| f.spr_evictions),
            ],
        ));
        charts.push(line(
            "Tile-load wire per frame",
            &x,
            None,
            vec![series("tile_load_wire", C1, &frames, |f| f.tile_load_wire)],
        ));
    }

    if gates::has_apu(&frames) {
        charts.push(line(
            "APU fetch wire per frame",
            &x,
            None,
            vec![series("apu_fetch_wire", C1, &frames, |f| f.apu_fetch_wire)],
        ));
    }

    charts.push(line(
        "Instructions per frame",
        &x,
        None,
        vec![series("instrs", C1, &frames, |f| f.instrs)],
    ));

    charts
}

// ------------------------------------------------- per-function pivots

/// Functions ranked by total `misses + evicted`, descending, ties broken
/// by name ascending -- `reports.rs`'s `function_totals`, including its
/// deterministic tie-break (a `HashMap` iteration order would otherwise
/// make the chart's series order vary run to run).
fn function_totals(rows: &[ProfileRow]) -> Vec<(String, i64)> {
    let mut totals: HashMap<&str, i64> = HashMap::new();
    for r in rows {
        *totals.entry(r.func.as_str()).or_insert(0) += r.misses + r.evicted;
    }
    let mut ranked: Vec<(String, i64)> = totals
        .into_iter()
        .map(|(f, total)| (f.to_string(), total))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
}

/// The top `take` functions' per-frame `misses`, pivoted onto
/// `frame_axis` -- `reports.rs`'s `top_functions`.
///
/// Pivoting onto the FULL (ignore-filtered) frame axis rather than onto
/// the frame numbers `rows` happens to mention is load-bearing: a frame
/// with zero I$ misses emits no profile rows at all, so the other axis
/// would silently drop every quiet frame and misalign this chart against
/// the wire/cache-miss charts above it. Absent (function, frame) pairs
/// zero-pad. Functions past `take` are summed into a trailing `"other"`
/// series.
fn top_functions(rows: &[ProfileRow], frame_axis: &[i64], take: usize) -> Vec<SeriesSpec> {
    if rows.is_empty() {
        return Vec::new();
    }
    let ranked = function_totals(rows);
    let kept: Vec<&str> = ranked.iter().take(take).map(|(f, _)| f.as_str()).collect();
    let other_needed = ranked.len() > kept.len();

    let frame_idx: HashMap<i64, usize> = frame_axis
        .iter()
        .enumerate()
        .map(|(i, &n)| (n, i))
        .collect();

    let mut series: Vec<SeriesSpec> = kept
        .iter()
        .enumerate()
        .map(|(i, &name)| SeriesSpec {
            name: name.to_string(),
            color: PROF_COLORS[i % PROF_COLORS.len()],
            values: vec![0.0f32; frame_axis.len()],
        })
        .collect();
    let mut other_values = vec![0.0f32; frame_axis.len()];

    for r in rows {
        let Some(&idx) = frame_idx.get(&r.frame) else {
            continue;
        };
        if let Some(pos) = kept.iter().position(|&n| n == r.func) {
            series[pos].values[idx] += r.misses as f32;
        } else {
            other_values[idx] += r.misses as f32;
        }
    }
    if other_needed {
        series.push(SeriesSpec {
            name: OTHER_FUNCTION_NAME.to_string(),
            color: PROF_COLORS[kept.len() % PROF_COLORS.len()],
            values: other_values,
        });
    }
    series
}

/// The same names/colors [`top_functions`] chose, re-pivoted onto
/// `evicted` -- `reports.rs`'s `pivot_evicted`. The two charts MUST share
/// one name selection (not each re-rank by its own metric), or their
/// legends stop agreeing.
fn pivot_evicted(top: &[SeriesSpec], rows: &[ProfileRow], frame_axis: &[i64]) -> Vec<SeriesSpec> {
    let frame_idx: HashMap<i64, usize> = frame_axis
        .iter()
        .enumerate()
        .map(|(i, &n)| (n, i))
        .collect();
    let mut out: Vec<SeriesSpec> = top
        .iter()
        .map(|s| SeriesSpec {
            name: s.name.clone(),
            color: s.color,
            values: vec![0.0f32; frame_axis.len()],
        })
        .collect();
    let other_idx = top.iter().position(|s| s.name == OTHER_FUNCTION_NAME);
    for r in rows {
        let Some(&idx) = frame_idx.get(&r.frame) else {
            continue;
        };
        if let Some(pos) = top.iter().position(|s| s.name == r.func) {
            out[pos].values[idx] += r.evicted as f32;
        } else if let Some(oi) = other_idx {
            out[oi].values[idx] += r.evicted as f32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal run: frames 0..=2, only the always-on columns populated,
    /// so every gated chart stays hidden.
    fn plain_samples() -> RunSamples {
        RunSamples {
            frames: (0..3)
                .map(|n| FrameRow {
                    n,
                    instrs: 1_000 + n,
                    i_misses: 10 + n,
                    d_misses: 5 + n,
                    scanout_wire: 164_400,
                    blit_wire: 30,
                    miss_wire: 40,
                    wire_total: 164_470,
                    frame_budget_cycles: Some(555_549),
                    ..FrameRow::default()
                })
                .collect(),
            profile: Vec::new(),
        }
    }

    fn titles(charts: &[ChartSpec]) -> Vec<&str> {
        charts.iter().map(|c| c.title.as_str()).collect()
    }

    #[test]
    fn the_always_on_chart_set_matches_the_reports_page() {
        let charts = build_charts(&plain_samples());
        assert_eq!(
            titles(&charts),
            vec![
                "Wire cycles per frame vs budget",
                "Wire breakdown per frame",
                "Cache misses per frame",
                "i_misses distribution",
                "d_misses distribution",
                "Instructions per frame",
            ]
        );
    }

    #[test]
    fn ignored_count_reports_what_the_filter_drops() {
        assert_eq!(ignored_count(&plain_samples().frames), 1);
        let no_frame_zero: Vec<FrameRow> = plain_samples()
            .frames
            .into_iter()
            .filter(|f| f.n != 0)
            .collect();
        assert_eq!(ignored_count(&no_frame_zero), 0);
        assert_eq!(ignored_count(&[]), 0);
    }

    #[test]
    fn frame_zero_is_ignored_by_default() {
        let charts = build_charts(&plain_samples());
        // Frames 0, 1, 2 seeded; frame 0 dropped.
        assert_eq!(charts[0].x, vec![1.0, 2.0]);
        assert_eq!(charts[0].series[0].values, vec![164_470.0, 164_470.0]);
    }

    #[test]
    fn the_budget_line_comes_from_the_runs_frame_budget_column() {
        let charts = build_charts(&plain_samples());
        assert_eq!(
            charts[0].kind,
            ChartKind::Line {
                budget: Some(555_549.0)
            }
        );
    }

    /// A device run has no wire model: `frame_budget_cycles` is NULL, so
    /// the wire chart draws without a reference line rather than with one
    /// at zero.
    #[test]
    fn a_run_without_a_budget_draws_no_budget_line() {
        let mut samples = plain_samples();
        for f in &mut samples.frames {
            f.frame_budget_cycles = None;
        }
        let charts = build_charts(&samples);
        assert_eq!(charts[0].kind, ChartKind::Line { budget: None });
    }

    #[test]
    fn a_run_whose_only_frame_is_ignored_has_no_charts() {
        let samples = RunSamples {
            frames: vec![FrameRow {
                n: 0,
                ..FrameRow::default()
            }],
            profile: Vec::new(),
        };
        assert!(build_charts(&samples).is_empty());
    }

    #[test]
    fn a_run_with_no_frames_has_no_charts() {
        assert!(build_charts(&RunSamples::default()).is_empty());
    }

    /// Each gate, one at a time, on top of the always-on set.
    #[test]
    fn each_gate_adds_exactly_its_own_charts() {
        let mut syscalls = plain_samples();
        syscalls.frames[1].sc_upload = 3;
        assert!(titles(&build_charts(&syscalls)).contains(&"Syscalls per frame"));

        let mut tiles = plain_samples();
        tiles.frames[1].bg_tiles_distinct = 12;
        assert!(titles(&build_charts(&tiles)).contains(&"Tile working set vs cache capacity"));

        let mut ppu = plain_samples();
        ppu.frames[1].bg_evictions = 1;
        let ppu_charts = build_charts(&ppu);
        let ppu_titles = titles(&ppu_charts);
        assert!(ppu_titles.contains(&"PPU tile-cache evictions per frame"));
        assert!(ppu_titles.contains(&"Tile-load wire per frame"));

        let mut apu = plain_samples();
        apu.frames[1].apu_underruns = 2;
        assert!(titles(&build_charts(&apu)).contains(&"APU fetch wire per frame"));
    }

    /// A gate that only ever trips on the IGNORED frame must not fire --
    /// the gates read the filtered slice, like ggo-ide's do.
    #[test]
    fn a_gate_tripped_only_on_the_ignored_frame_does_not_fire() {
        let mut samples = plain_samples();
        samples.frames[0].sc_upload = 99;
        assert!(!titles(&build_charts(&samples)).contains(&"Syscalls per frame"));
    }

    #[test]
    fn the_tile_working_set_chart_draws_the_cache_capacity_line() {
        let mut samples = plain_samples();
        samples.frames[1].spr_tiles_distinct = 10;
        let charts = build_charts(&samples);
        let chart = charts
            .iter()
            .find(|c| c.title == "Tile working set vs cache capacity")
            .expect("gate should have added it");
        assert_eq!(
            chart.kind,
            ChartKind::Line {
                budget: Some(TILE_CACHE_TILES)
            }
        );
    }

    #[test]
    fn the_per_function_charts_appear_only_with_profile_rows() {
        let mut samples = plain_samples();
        assert!(!titles(&build_charts(&samples)).contains(&"I$ misses by function"));

        samples.profile = vec![ProfileRow {
            frame: 1,
            func: "update".to_string(),
            misses: 4,
            evicted: 1,
        }];
        let with_profile = build_charts(&samples);
        let t = titles(&with_profile);
        assert!(t.contains(&"I$ misses by function"));
        assert!(t.contains(&"I$ eviction victims by function"));
        // ...and they sit between the tile-working-set slot and the
        // histograms, matching the reports page's order.
        let misses_ix = t
            .iter()
            .position(|s| *s == "I$ misses by function")
            .unwrap();
        let hist_ix = t
            .iter()
            .position(|s| *s == "i_misses distribution")
            .unwrap();
        assert!(misses_ix < hist_ix);
    }

    /// Profile rows only for the IGNORED frame leave nothing to chart.
    #[test]
    fn profile_rows_only_on_the_ignored_frame_add_no_charts() {
        let mut samples = plain_samples();
        samples.profile = vec![ProfileRow {
            frame: 0,
            func: "boot".to_string(),
            misses: 4,
            evicted: 1,
        }];
        assert!(!titles(&build_charts(&samples)).contains(&"I$ misses by function"));
    }

    #[test]
    fn top_functions_ranks_by_total_and_folds_the_remainder_into_other() {
        let axis = [1i64, 2];
        let rows: Vec<ProfileRow> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .enumerate()
            .map(|(i, name)| ProfileRow {
                frame: 1,
                func: name.to_string(),
                // Descending totals, so rank == declaration order.
                misses: (10 - i) as i64,
                evicted: 0,
            })
            .collect();
        let series = top_functions(&rows, &axis, TOP_FUNCTIONS_TAKE);
        let names: Vec<&str> = series.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d", "e", OTHER_FUNCTION_NAME]);
        assert_eq!(series[5].values, vec![5.0, 0.0], "f folds into other");
        assert_eq!(series[5].color, PROF_COLORS[TOP_FUNCTIONS_TAKE]);
    }

    /// A frame that emitted NO profile rows (nothing missed) must still be
    /// a zero-padded point on the axis, not a missing sample -- otherwise
    /// this chart's x-domain drifts out of step with every other chart's.
    #[test]
    fn top_functions_zero_pads_frames_with_no_profile_rows() {
        let axis = [1i64, 2, 3];
        let rows = vec![ProfileRow {
            frame: 2,
            func: "a".to_string(),
            misses: 7,
            evicted: 3,
        }];
        let series = top_functions(&rows, &axis, TOP_FUNCTIONS_TAKE);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].values, vec![0.0, 7.0, 0.0]);
    }

    #[test]
    fn top_functions_breaks_total_ties_by_name() {
        let axis = [1i64];
        let rows: Vec<ProfileRow> = ["zeta", "alpha"]
            .iter()
            .map(|n| ProfileRow {
                frame: 1,
                func: n.to_string(),
                misses: 5,
                evicted: 0,
            })
            .collect();
        let series = top_functions(&rows, &axis, TOP_FUNCTIONS_TAKE);
        let names: Vec<&str> = series.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn pivot_evicted_reuses_the_same_names_and_colors_over_evicted() {
        let axis = [1i64, 2];
        let rows = vec![
            ProfileRow {
                frame: 1,
                func: "a".to_string(),
                misses: 9,
                evicted: 2,
            },
            ProfileRow {
                frame: 2,
                func: "a".to_string(),
                misses: 1,
                evicted: 5,
            },
        ];
        let misses = top_functions(&rows, &axis, TOP_FUNCTIONS_TAKE);
        let evicted = pivot_evicted(&misses, &rows, &axis);
        assert_eq!(evicted.len(), misses.len());
        assert_eq!(evicted[0].name, misses[0].name);
        assert_eq!(evicted[0].color, misses[0].color);
        assert_eq!(evicted[0].values, vec![2.0, 5.0]);
    }

    #[test]
    fn top_functions_of_no_rows_is_empty() {
        assert!(top_functions(&[], &[1, 2], TOP_FUNCTIONS_TAKE).is_empty());
    }

    /// Every chart the set produces must actually have plottable data --
    /// a gate that fired but produced an all-empty spec would render as a
    /// blank canvas.
    #[test]
    fn every_produced_chart_has_data() {
        let mut samples = plain_samples();
        samples.frames[1].sc_upload = 3;
        samples.frames[1].bg_tiles_distinct = 12;
        samples.frames[1].bg_evictions = 1;
        samples.frames[1].apu_fetch_wire = 8;
        samples.profile = vec![ProfileRow {
            frame: 1,
            func: "update".to_string(),
            misses: 4,
            evicted: 1,
        }];
        let charts = build_charts(&samples);
        assert_eq!(charts.len(), 13, "the full reports-page chart set");
        for c in &charts {
            assert!(c.has_data(), "{} has no data", c.title);
        }
    }
}
