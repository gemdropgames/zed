//! Which charts a selected run shows, and with what series -- a
//! one-for-one mirror of ggo-ide's Reports run-detail page
//! (`tools/ggo-ide/src/pages/reports.rs::charts_section`), so a run looks
//! the same in this panel as it does there.
//!
//! Kept separate from `chart_geom` (HOW a chart is drawn) and `loader`
//! (WHERE the numbers come from): this module is only the mapping from
//! sampled `FrameRow`/`ProfileRow`s to [`ChartSpec`]s, and is pure, so
//! the whole chart set is unit-testable without a window or a database.

use std::collections::HashSet;

use ggo_worldlib::charts::reports::profile::NamedSeries;
use ggo_worldlib::charts::reports::{gates, ignore, kpi, profile};

use crate::chart_geom::{ChartKind, ChartSpec, Rgb, SeriesSpec};
use crate::loader::{FrameRow, RunSamples};

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

/// The `"other"` fold's bucket name. **Derived from worldlib's, not
/// re-spelled** -- `profile::top_function_series` emits that exact string
/// and `PROF_COLORS`' dedicated last slot is reserved for it.
///
/// R2 routed this constant here ahead of R4's collapse so the collapse
/// could not introduce a mismatch on its way through. R4 checked that
/// rather than trusting it, and it holds: this is a direct reference to
/// `profile::OTHER_FUNCTION_NAME`, not a copy of its value.
///
/// Drift is now **structurally impossible rather than guarded**: with the
/// local ranking gone, there is no second spelling of the fold's name
/// anywhere in this crate to drift from, and this line cannot disagree
/// with a constant it *is*. The [`colored`] assertion below is about the
/// fold's palette SLOT, which is a different question and a much weaker
/// guarantee -- see its comment for exactly how weak.
const OTHER_FUNCTION_NAME: &str = profile::OTHER_FUNCTION_NAME;

/// The 8 KB / 128 B-per-tile cache capacity the tile-working-set chart
/// draws as its reference line. It is a CAPACITY, not a budget, but
/// ggo-ide draws it with the same dashed danger-colored line, so this port
/// does too.
///
/// **Derived from `kpi::TILE_CACHE_TILES`, never re-spelled.** The same
/// number is the `> 64` threshold behind the two working-set KPI tiles
/// (`kpi::{spr,bg}_working_set_tile`), and those tiles sit directly above
/// this chart. A hand-copied literal here would let the chart draw its
/// capacity line at one value while the tile above it hid at another, with
/// nothing in either crate's tests noticing -- the exact drift the
/// single-source rule exists to stop, and the last hand-copied cross-layer
/// constant this module had.
const TILE_CACHE_TILES: f32 = kpi::TILE_CACHE_TILES as f32;

/// The one ignore set this panel has. `ignore::default_set()` is `{0}`:
/// a cold-cache first frame (every tile a miss, every asset a fresh
/// upload) is an outlier that flattens the rest of the run. ggo-ide lets
/// a user edit this set with a chip editor; this panel has no such editor
/// yet, so the default is all there is.
///
/// Shared with `report::build` on purpose -- the KPI tiles and the charts
/// beneath them must be summarising the SAME frames, and R1's concern (1)
/// is that no derivation in worldlib applies the filter for you.
pub fn ignore_set() -> HashSet<i64> {
    ignore::default_set()
}

// The chart gates are `ggo_worldlib::charts::reports::gates` (F5.4 Task
// R1) rather than a copy: a chart whose columns are zero across every
// frame is hidden rather than drawn flat, and which columns count is
// ggo-ide's decision, not this panel's. Note `gates::has_ppu` checks 4
// columns, not `RunPage.tsx`'s 7 -- `FrameRow` never carried
// `bg_loads`/`fg_loads`/`spr_loads`. Pre-existing and documented on that
// fn (R1's concern (3)); matched here rather than "fixed".

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
        selectable: false,
    }
}

fn stacked(title: &str, x: &[f32], series: Vec<SeriesSpec>) -> ChartSpec {
    ChartSpec {
        title: title.to_string(),
        kind: ChartKind::Stacked,
        x: x.to_vec(),
        series,
        selectable: false,
    }
}

/// Marks a chart click-to-inspect enabled.
///
/// `RunPage.tsx` passes `onSelect={pickFrame}` to exactly four charts and
/// `reports.rs`'s `charts_section` mirrors that set (`on_select:
/// Some(frame_select_hook(..))` at four call sites, `None` everywhere
/// else): the cache-misses chart, the tile-working-set chart, and the two
/// per-function charts. Those are the four whose x-position a reader is
/// pointing at *because of* what the I$ profile says about that frame --
/// which is what the inspect pane then shows. Every other chart stays
/// hover-only, here as there.
fn frame_selectable(mut spec: ChartSpec) -> ChartSpec {
    spec.selectable = true;
    spec
}

fn histogram(title: &str, name: &str, frames: &[FrameRow], get: fn(&FrameRow) -> i64) -> ChartSpec {
    ChartSpec {
        title: title.to_string(),
        kind: ChartKind::Histogram,
        // A histogram plots a DISTRIBUTION, not a time series: it has no
        // frame axis at all (`Histogram.tsx` never receives one), which
        // is also why it can never be frame-selectable.
        x: Vec::new(),
        series: vec![series(name, C1, frames, get)],
        selectable: false,
    }
}

/// How many of `frames` the ignore filter drops -- the panel captions
/// this ("1 frame ignored") so a user isn't left wondering why the x-axis
/// starts at 1. ggo-ide surfaces the same fact through its chip editor,
/// which this panel doesn't have yet.
pub fn ignored_count(frames: &[FrameRow]) -> usize {
    let ignored = ignore_set();
    frames.iter().filter(|f| ignored.contains(&f.n)).count()
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
    let ignored = ignore_set();
    let frames = ignore::apply(&samples.frames, &ignored);
    if frames.is_empty() {
        return Vec::new();
    }
    let profile = ignore::apply_profile(&samples.profile, &ignored);

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
        frame_selectable(line(
            "Cache misses per frame",
            &x,
            None,
            vec![
                series("i_misses", C1, &frames, |f| f.i_misses),
                series("d_misses", C2, &frames, |f| f.d_misses),
            ],
        )),
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
        charts.push(frame_selectable(line(
            "Tile working set vs cache capacity",
            &x,
            Some(TILE_CACHE_TILES),
            vec![
                series("bg_tiles_distinct", C1, &frames, |f| f.bg_tiles_distinct),
                series("spr_tiles_distinct", C2, &frames, |f| f.spr_tiles_distinct),
            ],
        )));
    }

    if !profile.is_empty() {
        let misses = profile::top_function_series(&profile, &frame_axis, TOP_FUNCTIONS_TAKE);
        let evicted = profile::pivot_evicted(&misses, &profile, &frame_axis);
        charts.push(frame_selectable(line(
            "I$ misses by function",
            &x,
            None,
            colored(&misses),
        )));
        charts.push(frame_selectable(line(
            "I$ eviction victims by function",
            &x,
            None,
            colored(&evicted),
        )));
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

/// worldlib's colourless [`NamedSeries`] with this panel's palette
/// attached **by rank position** -- the colour half of the split R1 made
/// when `top_functions`/`pivot_evicted` moved
/// (`profile::top_function_series`/`profile::pivot_evicted` own the
/// ranking, the `"other"` fold and the frame-axis pivot; only
/// `iced::Color` could not travel, and this panel's `Rgb` is its
/// replacement).
///
/// Rank order IS the colour contract (R1's concern (4)): worldlib emits
/// the kept functions in ranked order with the `"other"` fold last, so
/// slot `i` of `PROF_COLORS` belongs to rank `i` and `"other"` lands on
/// the palette's dedicated final slot exactly when it is present. That
/// is the same mapping the deleted local `top_functions` made with
/// `PROF_COLORS[i % len]` for the kept names and
/// `PROF_COLORS[kept.len() % len]` for the fold -- identical, because
/// the fold is at index `kept.len()` of what worldlib returns.
///
/// Applying it positionally to BOTH charts is what keeps them agreeing,
/// and the result is IDENTICAL to the old code rather than merely
/// equivalent: `profile::pivot_evicted` clones `top`'s names in `top`'s
/// own order, so colouring the two by position assigns each name the
/// colour the old code copied across from its misses-chart twin. Same
/// colours, one fewer thing to keep in step.
fn colored(series: &[NamedSeries]) -> Vec<SeriesSpec> {
    // The fold, when present, must land on the palette's dedicated final
    // slot.
    //
    // Be precise about what this does and does not buy, because it is
    // easy to overstate. It **cannot fire today**: worldlib appends the
    // fold only once the kept set is full, so it sits at index
    // `TOP_FUNCTIONS_TAKE`, and `TOP_FUNCTIONS_TAKE` is *defined* as
    // `PROF_COLORS.len() - 1`. It is also `debug_assert!`, so it is not
    // evaluated in a release build at all (Zed's `[profile.release]`
    // does not turn debug assertions on). It is therefore a
    // DEVELOPMENT-TIME statement of the invariant, and its one real job
    // is to fail if someone decouples those two constants -- set
    // `TOP_FUNCTIONS_TAKE` to 3 and the fold lands on slot 3 while slot
    // 5 stays reserved for it, which is exactly the silent recolouring
    // R1's concern (4) is about. Kept debug-only deliberately: a
    // mis-slotted colour is cosmetic and must not take a user's run
    // report down with it.
    debug_assert!(
        series
            .iter()
            .position(|s| s.name == OTHER_FUNCTION_NAME)
            .is_none_or(|i| i == PROF_COLORS.len() - 1),
        "the \"other\" fold must land on PROF_COLORS' dedicated last slot"
    );
    series
        .iter()
        .enumerate()
        .map(|(i, s)| SeriesSpec {
            name: s.name.clone(),
            color: PROF_COLORS[i % PROF_COLORS.len()],
            values: s.values.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::ProfileRow;

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
            ..RunSamples::default()
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
            ..RunSamples::default()
        };
        assert!(build_charts(&samples).is_empty());
    }

    #[test]
    fn a_run_with_no_frames_has_no_charts() {
        assert!(build_charts(&RunSamples::default()).is_empty());
    }

    /// The anti-drift guard for the last cross-layer constant this module
    /// hand-copied. The tile-working-set chart's reference line and the
    /// two working-set KPI tiles' hide-below threshold
    /// (`kpi::{spr,bg}_working_set_tile`) are the SAME cache capacity, and
    /// the tiles render directly above the chart. Before this was derived
    /// from `kpi::TILE_CACHE_TILES`, setting the local literal to 80.0
    /// left the chart drawing its line at 80 while the tiles still hid at
    /// 64, and the whole suite passed. Now the two cannot disagree, and
    /// this test is what fails if anyone re-spells the number.
    #[test]
    fn the_cache_capacity_line_is_worldlibs_constant_not_a_local_copy() {
        let mut samples = plain_samples();
        samples.frames[1].bg_tiles_distinct = 12;
        let charts = build_charts(&samples);
        let chart = charts
            .iter()
            .find(|c| c.title == "Tile working set vs cache capacity")
            .expect("the gate is tripped");
        assert_eq!(
            chart.kind,
            ChartKind::Line {
                budget: Some(kpi::TILE_CACHE_TILES as f32)
            },
            "the chart's capacity line must BE kpi::TILE_CACHE_TILES, so it \
             cannot drift from the KPI tiles' threshold"
        );
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
            caller: String::new(),
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
            caller: String::new(),
            func: "boot".to_string(),
            misses: 4,
            evicted: 1,
        }];
        assert!(!titles(&build_charts(&samples)).contains(&"I$ misses by function"));
    }

    /// The collapsed path: worldlib ranks and pivots, [`colored`] paints.
    fn top_functions(rows: &[ProfileRow], frame_axis: &[i64], take: usize) -> Vec<SeriesSpec> {
        colored(&profile::top_function_series(rows, frame_axis, take))
    }

    #[test]
    fn top_functions_ranks_by_total_and_folds_the_remainder_into_other() {
        let axis = [1i64, 2];
        let rows: Vec<ProfileRow> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .enumerate()
            .map(|(i, name)| ProfileRow {
                frame: 1,
                caller: String::new(),
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
            caller: String::new(),
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
                caller: String::new(),
                func: n.to_string(),
                misses: 5,
                evicted: 0,
            })
            .collect();
        let series = top_functions(&rows, &axis, TOP_FUNCTIONS_TAKE);
        let names: Vec<&str> = series.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    /// The eviction chart's colours used to be COPIED off the misses
    /// chart's series; now both are painted positionally and the sharing
    /// rests on `profile::pivot_evicted` returning the same names in the
    /// same order. Same guarantee, different mechanism -- so this stays.
    #[test]
    fn pivot_evicted_reuses_the_same_names_and_colors_over_evicted() {
        let axis = [1i64, 2];
        let rows = vec![
            ProfileRow {
                frame: 1,
                caller: String::new(),
                func: "a".to_string(),
                misses: 9,
                evicted: 2,
            },
            ProfileRow {
                frame: 2,
                caller: String::new(),
                func: "a".to_string(),
                misses: 1,
                evicted: 5,
            },
        ];
        let named = profile::top_function_series(&rows, &axis, TOP_FUNCTIONS_TAKE);
        let misses = colored(&named);
        let evicted = colored(&profile::pivot_evicted(&named, &rows, &axis));
        assert_eq!(evicted.len(), misses.len());
        assert_eq!(evicted[0].name, misses[0].name);
        assert_eq!(evicted[0].color, misses[0].color);
        assert_eq!(evicted[0].values, vec![2.0, 5.0]);
    }

    #[test]
    fn top_functions_of_no_rows_is_empty() {
        assert!(top_functions(&[], &[1, 2], TOP_FUNCTIONS_TAKE).is_empty());
    }

    // ------------------------------- the R4 collapse, checked not assumed

    /// The local `top_functions`/`function_totals`/`pivot_evicted` this
    /// module carried until F5.4 R4, verbatim, kept ONLY as this test's
    /// reference. R1's concern (4) is that rank order is the whole colour
    /// contract, so a collapse that re-ranked would silently recolour
    /// both per-function charts with nothing failing; the brief's
    /// instruction was to test the new path against the old behaviour
    /// rather than assume the two agree. This is that old behaviour.
    mod legacy {
        use super::*;
        use std::collections::HashMap;

        pub fn function_totals(rows: &[ProfileRow]) -> Vec<(String, i64)> {
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

        pub fn top_functions(
            rows: &[ProfileRow],
            frame_axis: &[i64],
            take: usize,
        ) -> Vec<SeriesSpec> {
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

        pub fn pivot_evicted(
            top: &[SeriesSpec],
            rows: &[ProfileRow],
            frame_axis: &[i64],
        ) -> Vec<SeriesSpec> {
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
    }

    /// A profile with more distinct functions than palette slots, an
    /// exact tie STRADDLING the rank boundary, a quiet frame on the axis
    /// and rows on a frame off it -- every case the ranking, the
    /// `"other"` fold and the zero-pad have to get right at once.
    ///
    /// Each function's ranking total is `4 * (misses + evicted)` (four
    /// frames' worth of identical rows), so the totals are readable
    /// straight off the table below: `zzz_tie` and `aaa_tie` are both
    /// 40, which is the boundary -- four functions rank above them and
    /// only one of the two can be kept.
    /// `the_collapse_fixture_reaches_the_fold_and_the_tie_break` asserts
    /// that tie is real rather than taking this comment's word for it.
    fn collapse_fixture() -> (Vec<ProfileRow>, Vec<i64>) {
        let mut rows = Vec::new();
        // Eight distinct functions, only five palette slots before the
        // fold. `zzz_tie`/`aaa_tie` tie exactly on total (4 * (9 + 1) =
        // 40 each) at the rank boundary, so the name tie-break alone
        // decides which is kept and which is folded -- the ranking's
        // sharpest edge. No per-function offset is applied to the values:
        // one would perturb the totals and quietly dissolve that tie.
        let spec: [(&str, i64, i64); 8] = [
            ("render", 50, 5),    // 220
            ("update", 40, 10),   // 200
            ("collide", 30, 3),   // 132
            ("audio_mix", 20, 1), // 84
            ("zzz_tie", 9, 1),    // 40  <- tie, folded (name desc)
            ("aaa_tie", 9, 1),    // 40  <- tie, kept   (name asc)
            ("despawn", 1, 7),    // 32
            ("spawn", 4, 0),      // 16
        ];
        for (func, misses, evicted) in spec {
            // Spread the rows over frames 1, 3 and 7 -- frame 5 is on the
            // axis but emits nothing (a quiet frame) and frame 9 is off it.
            for frame in [1, 3, 7, 9] {
                rows.push(ProfileRow {
                    frame,
                    caller: "main".to_string(),
                    func: func.to_string(),
                    misses,
                    evicted,
                });
            }
        }
        (rows, vec![1, 3, 5, 7])
    }

    #[test]
    fn the_collapse_onto_worldlib_reproduces_the_old_local_ranking_exactly() {
        let (rows, axis) = collapse_fixture();
        let named = profile::top_function_series(&rows, &axis, TOP_FUNCTIONS_TAKE);
        assert_eq!(
            colored(&named),
            legacy::top_functions(&rows, &axis, TOP_FUNCTIONS_TAKE),
            "same names, same rank ORDER, same colour per rank, same \
             zero-padded values -- a collapse that re-ranked would recolour \
             both per-function charts with nothing else noticing"
        );
        assert_eq!(
            colored(&profile::pivot_evicted(&named, &rows, &axis)),
            legacy::pivot_evicted(
                &legacy::top_functions(&rows, &axis, TOP_FUNCTIONS_TAKE),
                &rows,
                &axis
            ),
            "and the eviction chart still mirrors the misses chart's \
             selection, now by shared ordering rather than by copied colour"
        );
    }

    /// The fixture is only worth what it exercises: it must actually
    /// trip the `"other"` fold and the name tie-break, or the equality
    /// above would hold for two functions that agree on the easy cases.
    #[test]
    fn the_collapse_fixture_reaches_the_fold_and_the_tie_break() {
        let (rows, axis) = collapse_fixture();
        let named = profile::top_function_series(&rows, &axis, TOP_FUNCTIONS_TAKE);
        let names: Vec<&str> = named.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names.last(),
            Some(&OTHER_FUNCTION_NAME),
            "more functions than slots, so the fold is present"
        );
        // The tie is asserted, not asserted-about: a fixture edit that
        // perturbed either function's totals would leave the name check
        // below passing while testing nothing at all about tie-breaking.
        let totals = profile::function_totals(&rows);
        let total_of = |func: &str| {
            totals
                .iter()
                .find(|(f, _)| f == func)
                .unwrap_or_else(|| panic!("{func} must be in the fixture"))
                .1
        };
        assert_eq!(
            total_of("aaa_tie"),
            total_of("zzz_tie"),
            "the two must actually TIE, or the name tie-break is never consulted"
        );
        let rank_of = |func: &str| {
            totals
                .iter()
                .position(|(f, _)| f == func)
                .unwrap_or_else(|| panic!("{func} must be in the fixture"))
        };
        assert_eq!(
            (rank_of("aaa_tie"), rank_of("zzz_tie")),
            (TOP_FUNCTIONS_TAKE - 1, TOP_FUNCTIONS_TAKE),
            "and the tied pair must STRADDLE the kept/folded boundary -- \
             last kept and first folded: {totals:?}"
        );
        assert!(
            names.contains(&"aaa_tie") && !names.contains(&"zzz_tie"),
            "so the tie at the rank boundary breaks by name ascending: {names:?}"
        );
        // ...and a quiet frame (5) is still a point on the axis.
        assert_eq!(named[0].values.len(), axis.len());
        assert_eq!(named[0].values[2], 0.0, "frame 5 emitted nothing");
    }

    // -------------------------------------------------- click-to-inspect

    /// `RunPage.tsx`/`reports.rs` pass `onSelect` to exactly four charts.
    /// Marking more would make a click on, say, the wire chart open an
    /// inspect pane about I$ misses; marking fewer would leave the
    /// per-function charts -- the ones the pane is ABOUT -- inert.
    #[test]
    fn exactly_the_four_reports_page_charts_are_frame_selectable() {
        let mut samples = plain_samples();
        samples.frames[1].sc_upload = 3;
        samples.frames[1].bg_tiles_distinct = 12;
        samples.frames[1].bg_evictions = 1;
        samples.frames[1].apu_fetch_wire = 8;
        samples.profile = vec![ProfileRow {
            frame: 1,
            caller: String::new(),
            func: "update".to_string(),
            misses: 4,
            evicted: 1,
        }];
        let charts = build_charts(&samples);
        let selectable: Vec<&str> = charts
            .iter()
            .filter(|c| c.selectable)
            .map(|c| c.title.as_str())
            .collect();
        assert_eq!(
            selectable,
            vec![
                "Cache misses per frame",
                "Tile working set vs cache capacity",
                "I$ misses by function",
                "I$ eviction victims by function",
            ]
        );
    }

    /// A histogram has no frame axis at all, so it can never be
    /// selectable -- there is no frame under the cursor to name.
    #[test]
    fn no_histogram_is_frame_selectable() {
        let charts = build_charts(&plain_samples());
        for c in &charts {
            if matches!(c.kind, ChartKind::Histogram) {
                assert!(!c.selectable, "{} is a distribution", c.title);
            }
        }
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
            caller: String::new(),
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
