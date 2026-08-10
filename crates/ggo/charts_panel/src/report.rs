//! The run-detail report's non-chart surface: the KPI tile row, the
//! header's run-config line, and the failed-asset-loads / panics tables --
//! a mirror of ggo-ide's Reports run-detail page
//! (`tools/ggo-ide/src/pages/reports.rs`'s `kpi_row`, `header_row`,
//! `asset_failures_section`, `panics_section`).
//!
//! **Nothing here derives anything.** Every number and every string comes
//! out of `ggo_worldlib::charts::reports::{kpi, uart_diag}` (F5.4 Task
//! R1's extraction of exactly this arithmetic out of `reports.rs`); this
//! module only decides which of them to ask for, in what order, and what
//! to say when there is nothing to show. Keeping it that way is what
//! makes "the panel and the page agree for the same run" checkable rather
//! than hopeful.
//!
//! Kept out of `ggo_charts_panel` proper for the same reason `chart_set`
//! is: it is pure, so the whole tile row and both tables are unit-testable
//! without a window.

use ggo_worldlib::charts::reports::fmt::with_thousands;
use ggo_worldlib::charts::reports::kpi::{self, PCT_SCALE};
use ggo_worldlib::charts::reports::uart_diag::{self, AssetFailure, PanicRow};

use crate::chart_set::ignore_set;
use crate::loader::{FrameRow, RunDetail, RunSamples, UartLine};

/// One KPI tile: a fixed label and an already-formatted value. The value
/// is a `String` because the tiles are heterogeneous (counts, percentages,
/// a ratio, "N tiles") and the formatting rule per tile is ggo-ide's, not
/// this panel's -- see [`kpi_tiles`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KpiTile {
    pub label: &'static str,
    pub value: String,
}

impl KpiTile {
    fn new(label: &'static str, value: String) -> Self {
        Self { label, value }
    }
}

/// `reports.rs::kpi_row`'s tiles, in its exact order, over frames the
/// caller has ALREADY ignore-filtered (R1's concern (1): no derivation in
/// `kpi` applies the filter itself, so passing raw frames here silently
/// folds frame 0's cold-cache burst into every tile and the panel's
/// numbers stop matching the page's for the same run). [`build`] is the
/// only caller in this crate and it filters first.
///
/// `budget` is `detail.frame_budget_cycles` -- the same column
/// `FrameRow::frame_budget_cycles` carries, but read off the `run` row the
/// way ggo-ide reads it, so a run whose frames somehow disagree with their
/// own run row still shows the run's budget.
///
/// The last five tiles are conditional and each is gated by its own
/// `kpi::*_tile` returning `Some` -- NOT by
/// `ggo_worldlib::charts::reports::gates`. That is ggo-ide's behaviour
/// (`kpi_row` never consults `chart_gates`; the gates are for the
/// conditional CHARTS, which is where `chart_set` uses them), and it is
/// what makes "no tile at all" mean "this run never measured that": each
/// threshold is the counter's own (`> 0` for the peak/underrun/upload
/// tiles, `> TILE_CACHE_TILES` for the two working sets, because a working
/// set that fits in the cache is not a finding). A measured zero is
/// therefore never rendered as a `0` tile.
///
/// Worth knowing while reading this next to the charts: `gates::has_ppu`
/// checks 4 columns, not the 7 `RunPage.tsx` checks -- `FrameRow` never
/// carried `bg_loads`/`fg_loads`/`spr_loads`. That is pre-existing and
/// documented on the fn (R1's concern (3)); this task deliberately did not
/// "fix" it, because the fork's job is to match ggo-ide, and a column the
/// row does not have cannot be checked here either.
pub fn kpi_tiles(frames: &[FrameRow], budget: Option<i64>) -> Vec<KpiTile> {
    let avg_wire = kpi::avg_wire_total(frames);
    let ipc = kpi::ipc(frames);
    let i_hit = kpi::hit_rate(kpi::sum_i_hits(frames), kpi::sum_i_misses(frames));
    let d_hit = kpi::hit_rate(kpi::sum_d_hits(frames), kpi::sum_d_misses(frames));

    let mut tiles = vec![
        KpiTile::new("Frames", with_thousands(frames.len() as i64)),
        KpiTile::new(
            "Over-budget frames",
            with_thousands(kpi::over_budget_count(frames)),
        ),
        KpiTile::new(
            "Avg wire vs budget",
            kpi::budget_pct(avg_wire, budget)
                .map_or_else(|| "—".to_string(), |p| format!("{p:.1}%")),
        ),
        KpiTile::new("IPC (instr / wire cyc)", format!("{ipc:.2}")),
        KpiTile::new("I$ hit rate", format!("{:.1}%", i_hit * PCT_SCALE)),
        KpiTile::new("D$ hit rate", format!("{:.1}%", d_hit * PCT_SCALE)),
        KpiTile::new(
            "Max i_miss / frame",
            with_thousands(kpi::max_i_misses(frames)),
        ),
        KpiTile::new(
            "Max d_miss / frame",
            with_thousands(kpi::max_d_misses(frames)),
        ),
    ];

    if let Some(peak) = kpi::peak_spr_line_tile(frames) {
        tiles.push(KpiTile::new(
            "Peak sprites / scanline",
            with_thousands(peak),
        ));
    }
    if let Some(spr) = kpi::spr_working_set_tile(frames) {
        tiles.push(KpiTile::new(
            "Sprite working set",
            format!("{} tiles", with_thousands(spr)),
        ));
    }
    if let Some(bg) = kpi::bg_working_set_tile(frames) {
        tiles.push(KpiTile::new(
            "BG working set",
            format!("{} tiles", with_thousands(bg)),
        ));
    }
    if let Some(vram) = kpi::vram_uploads_per_frame_tile(frames) {
        tiles.push(KpiTile::new("VRAM uploads / frame", format!("{vram:.1}")));
    }
    if let Some(total) = kpi::apu_underruns_tile(frames) {
        tiles.push(KpiTile::new("APU underruns", with_thousands(total)));
    }
    tiles
}

/// The failed-asset-loads and panics tables' contents for one run, and --
/// the part worldlib deliberately cannot answer -- whether an empty table
/// means "nothing failed" or "there is nothing to have failed in".
///
/// `uart_diag`'s module doc spells the distinction out: an empty
/// `parse_asset_failures` result means "no failing lines in the UART this
/// run recorded", which is NOT the same as "this run recorded no UART at
/// all" (a run from before UART persistence existed, or a diag run whose
/// serial log lives in `run_log` under a string id). The only signal that
/// separates them is whether `perf_db::run_uart` returned any lines, so
/// this enum is built from exactly that.
///
/// **Deviation from ggo-ide, deliberate.** `reports.rs`'s
/// `asset_failures_section` collapses both cases into one hedged sentence
/// ("none — every asset_load succeeded (or the run predates UART
/// capture)") and its `panics_section` just says "none", which reads as
/// "this run did not panic" even for a run that recorded nothing at all --
/// the more dangerous of the two readings. This task's brief requires them
/// separated, so they are; see this task's report.
#[derive(Debug, Clone, PartialEq)]
pub enum Diagnostics {
    /// The run persisted no UART lines, so neither table can say anything
    /// about it either way.
    NoUart,
    /// The run persisted UART; these are what parsing it found (either
    /// list may still be empty, which now honestly means "none recorded").
    Recorded {
        failures: Vec<AssetFailure>,
        panics: Vec<PanicRow>,
    },
}

/// What both tables print instead of rows when the run recorded UART and
/// nothing in it matched.
pub const NONE_RECORDED: &str = "none recorded";

/// What both tables print when there is no UART to have recorded anything
/// in -- a different claim from [`NONE_RECORDED`], and the one ggo-ide
/// conflates with it.
pub const NO_UART: &str = "no UART recorded for this run — it predates UART capture";

impl Diagnostics {
    pub fn from_uart(lines: &[UartLine]) -> Self {
        if lines.is_empty() {
            return Self::NoUart;
        }
        Self::Recorded {
            failures: uart_diag::parse_asset_failures(lines),
            panics: uart_diag::parse_panics(lines),
        }
    }

    pub fn failures(&self) -> &[AssetFailure] {
        match self {
            Self::NoUart => &[],
            Self::Recorded { failures, .. } => failures,
        }
    }

    pub fn panics(&self) -> &[PanicRow] {
        match self {
            Self::NoUart => &[],
            Self::Recorded { panics, .. } => panics,
        }
    }

    /// The sentence a table shows in place of rows, or `None` when it has
    /// rows to show.
    pub fn empty_state(&self, rows: usize) -> Option<&'static str> {
        match self {
            Self::NoUart => Some(NO_UART),
            Self::Recorded { .. } if rows == 0 => Some(NONE_RECORDED),
            Self::Recorded { .. } => None,
        }
    }
}

/// Everything the run-detail surface renders that is not a chart,
/// assembled once per run selection (alongside `chart_set::build_charts`)
/// rather than once per render -- the same call `reports.rs`'s
/// `detail_view` makes, in the same order.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    /// Empty when the run has no frames left after the ignore filter --
    /// every tile would read 0 (or, for the hit rates, a
    /// `DEFAULT_HIT_RATE` 100%) for a run that measured nothing, which is
    /// a worse lie than showing no tiles. ggo-ide skips `kpi_row` on the
    /// same condition. The diagnostic tables below are NOT skipped with
    /// them: a cart that panicked before its first vsync is exactly the
    /// frameless run whose panic row matters most.
    pub tiles: Vec<KpiTile>,
    /// `None` only when the run id has no `run` row at all; ggo-ide shows
    /// nothing in that case too (its `header_row` matches on
    /// `self.detail.value()`).
    pub config_line: Option<String>,
    pub diagnostics: Diagnostics,
}

/// Build the report for one loaded run.
///
/// The ignore filter is applied HERE, through the same [`ignore_set`]
/// `chart_set::build_charts` uses, so a tile and a chart on the same
/// screen can never be summarising different frame sets.
pub fn build(samples: &RunSamples) -> RunReport {
    let frames = ggo_worldlib::charts::reports::ignore::apply(&samples.frames, &ignore_set());
    RunReport {
        tiles: if frames.is_empty() {
            Vec::new()
        } else {
            kpi_tiles(&frames, budget_of(samples.detail.as_ref(), &frames))
        },
        config_line: samples.detail.as_ref().map(kpi::run_config_line),
        diagnostics: Diagnostics::from_uart(&samples.uart),
    }
}

/// The budget the "Avg wire vs budget" tile measures against: the `run`
/// row's, exactly as ggo-ide reads it -- falling back to the copy the
/// frame rows carry (the same column, denormalized by `run_frames`' join)
/// when there is no `run` row, so a run whose listing outlived its row
/// still shows a percentage rather than a dash.
fn budget_of(detail: Option<&RunDetail>, frames: &[FrameRow]) -> Option<i64> {
    detail
        .and_then(|d| d.frame_budget_cycles)
        .or_else(|| frames.first().and_then(|f| f.frame_budget_cycles))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R1's own `kpi::tests::sample_frames` fixture, field for field --
    /// so the assertions below are literally against the numbers that
    /// module's unit tests hand-computed, and a divergence between this
    /// panel and ggo-ide would have to break one of the two suites.
    fn sample_frames() -> Vec<FrameRow> {
        vec![
            FrameRow {
                n: 0,
                instrs: 40_000,
                i_hits: 200,
                i_misses: 5,
                d_hits: 80,
                d_misses: 2,
                wire_total: 100_000,
                over_budget: false,
                apu_underruns: 7,
                frame_budget_cycles: Some(555_549),
                ..FrameRow::default()
            },
            FrameRow {
                n: 1,
                instrs: 250_000,
                i_hits: 300,
                i_misses: 9,
                d_hits: 100,
                d_misses: 4,
                wire_total: 600_000,
                over_budget: true,
                apu_underruns: 2,
                frame_budget_cycles: Some(555_549),
                ..FrameRow::default()
            },
            FrameRow {
                n: 2,
                instrs: 90_000,
                i_hits: 220,
                i_misses: 3,
                d_hits: 90,
                d_misses: 1,
                wire_total: 200_000,
                over_budget: false,
                apu_underruns: 1,
                frame_budget_cycles: Some(555_549),
                ..FrameRow::default()
            },
        ]
    }

    fn sample_detail() -> RunDetail {
        RunDetail {
            id: 1,
            cart_id: 1,
            cart_name: "demo.cart".to_string(),
            started_at: "2026-08-01T00:00:00Z".to_string(),
            frames: 3,
            frame_budget_cycles: Some(555_549),
            scanout_wire_cycles: Some(164_400),
            refill_cycles: Some(100),
            writeback_cycles: Some(65),
            wire_wait_cycles: 10,
            label: Some("first".to_string()),
            over_budget_frames: 1,
            avg_wire_total: Some(300_000.0),
            max_wire_total: Some(600_000),
            avg_i_misses: Some(5.0),
            avg_d_misses: Some(2.0),
            max_i_misses: Some(9),
            max_d_misses: Some(4),
            apu_underruns: 3,
        }
    }

    fn value_of<'a>(tiles: &'a [KpiTile], label: &str) -> Option<&'a str> {
        tiles
            .iter()
            .find(|t| t.label == label)
            .map(|t| t.value.as_str())
    }

    /// The always-on eight, in ggo-ide's `kpi_row` order.
    #[test]
    fn the_unconditional_tiles_are_ggo_ides_eight_in_order() {
        let tiles = kpi_tiles(&sample_frames(), Some(555_549));
        let labels: Vec<&str> = tiles.iter().map(|t| t.label).collect();
        assert_eq!(
            labels,
            vec![
                "Frames",
                "Over-budget frames",
                "Avg wire vs budget",
                "IPC (instr / wire cyc)",
                "I$ hit rate",
                "D$ hit rate",
                "Max i_miss / frame",
                "Max d_miss / frame",
                // The fixture's frame 0 carries apu_underruns: 7, which
                // survives here because this call is NOT ignore-filtered.
                "APU underruns",
            ]
        );
    }

    /// The trap R1 flagged as concern (1), pinned as a test rather than a
    /// comment: `build` must ignore-filter before deriving, and the values
    /// it produces must be the ones `kpi`'s own
    /// `kpis_after_default_ignore_match_what_run_page_would_show` test
    /// hand-computed over frames 1 and 2 -- avg wire 400,000 (72.0% of
    /// budget), 1 over-budget frame, IPC 340,000/800,000 = 0.425, I$ hit
    /// rate 520/532. Deriving over the unfiltered set would instead give
    /// 3 frames, 54.0% and a 97.0% I$ hit rate, which is exactly the
    /// silent divergence from ggo-ide's numbers this test exists to catch.
    ///
    /// (IPC is asserted for completeness, not as a discriminator: it
    /// reads "0.42" either way -- 0.425 is not exactly representable and
    /// its nearest `f64` is below the midpoint, so `{:.2}` rounds it
    /// down, landing on the unfiltered 0.4222's string too.)
    #[test]
    fn build_ignore_filters_before_deriving_so_the_kpis_match_ggo_ides() {
        let samples = RunSamples {
            frames: sample_frames(),
            detail: Some(sample_detail()),
            ..RunSamples::default()
        };
        let tiles = build(&samples).tiles;

        assert_eq!(value_of(&tiles, "Frames"), Some("2"), "frame 0 is dropped");
        assert_eq!(value_of(&tiles, "Over-budget frames"), Some("1"));
        // 400,000 / 555,549 = 71.999...%
        assert_eq!(value_of(&tiles, "Avg wire vs budget"), Some("72.0%"));
        assert_eq!(value_of(&tiles, "IPC (instr / wire cyc)"), Some("0.42"));
        // 520/532 = 97.744%, 190/195 = 97.436%
        assert_eq!(value_of(&tiles, "I$ hit rate"), Some("97.7%"));
        assert_eq!(value_of(&tiles, "D$ hit rate"), Some("97.4%"));
        assert_eq!(value_of(&tiles, "Max i_miss / frame"), Some("9"));
        assert_eq!(value_of(&tiles, "Max d_miss / frame"), Some("4"));
        // frame 0's 7 underruns are excluded: 2 + 1.
        assert_eq!(value_of(&tiles, "APU underruns"), Some("3"));
    }

    /// The same assertion from the other side: an ignored frame must
    /// actually change the answer, so a `build` that forgot to filter
    /// could not pass both this and the test above.
    #[test]
    fn an_ignored_frame_changes_the_kpis() {
        let all = sample_frames();
        let unfiltered = kpi_tiles(&all, Some(555_549));
        let filtered = build(&RunSamples {
            frames: all,
            detail: Some(sample_detail()),
            ..RunSamples::default()
        })
        .tiles;
        assert_ne!(
            value_of(&unfiltered, "Frames"),
            value_of(&filtered, "Frames")
        );
        assert_ne!(
            value_of(&unfiltered, "Avg wire vs budget"),
            value_of(&filtered, "Avg wire vs budget")
        );
    }

    /// A run that never measured PPU/APU counters gets NO conditional
    /// tile -- not a tile reading "0". This is the whole point of
    /// `kpi::*_tile` returning `Option`.
    #[test]
    fn a_run_without_ppu_or_apu_counters_renders_no_conditional_tiles() {
        let frames: Vec<FrameRow> = (1..=3)
            .map(|n| FrameRow {
                n,
                instrs: 1_000,
                wire_total: 100,
                ..FrameRow::default()
            })
            .collect();
        let tiles = kpi_tiles(&frames, Some(555_549));
        assert_eq!(tiles.len(), 8, "only the unconditional eight");
        for absent in [
            "Peak sprites / scanline",
            "Sprite working set",
            "BG working set",
            "VRAM uploads / frame",
            "APU underruns",
        ] {
            assert_eq!(value_of(&tiles, absent), None, "{absent} must be absent");
        }
        // The unconditional eight legitimately DO read 0 here (this run
        // never went over budget and never missed): "no zeroes" is a rule
        // about counters the run never MEASURED, which is why it is
        // enforced as tile absence above and not as a value check.
        assert_eq!(value_of(&tiles, "Over-budget frames"), Some("0"));
    }

    /// Each conditional tile appears once its own counter crosses its own
    /// threshold -- and the two working-set tiles' threshold is the cache
    /// capacity (64), not zero, so a working set that FITS still shows no
    /// tile.
    #[test]
    fn each_conditional_tile_appears_at_its_own_threshold() {
        let fits = vec![FrameRow {
            n: 1,
            peak_spr_line: 4,
            bg_tiles_distinct: 64,
            spr_tiles_distinct: 64,
            sc_upload: 3,
            ..FrameRow::default()
        }];
        let tiles = kpi_tiles(&fits, None);
        assert_eq!(value_of(&tiles, "Peak sprites / scanline"), Some("4"));
        assert_eq!(value_of(&tiles, "VRAM uploads / frame"), Some("3.0"));
        assert_eq!(
            value_of(&tiles, "BG working set"),
            None,
            "a working set AT the cache capacity fits, so it is not a finding"
        );
        assert_eq!(value_of(&tiles, "Sprite working set"), None);

        let thrashes = vec![FrameRow {
            n: 1,
            bg_tiles_distinct: 65,
            spr_tiles_distinct: 70,
            ..FrameRow::default()
        }];
        let tiles = kpi_tiles(&thrashes, None);
        assert_eq!(value_of(&tiles, "BG working set"), Some("65 tiles"));
        assert_eq!(value_of(&tiles, "Sprite working set"), Some("70 tiles"));
    }

    /// A device run has no wire model, so there is no budget to be a
    /// percentage of -- a dash, never a 0.0%.
    #[test]
    fn a_run_without_a_budget_dashes_the_budget_tile() {
        let samples = RunSamples {
            frames: vec![FrameRow {
                n: 1,
                wire_total: 100,
                ..FrameRow::default()
            }],
            ..RunSamples::default()
        };
        assert_eq!(
            value_of(&build(&samples).tiles, "Avg wire vs budget"),
            Some("—")
        );
    }

    /// A run with nothing left after the filter gets no tiles at all --
    /// a "Frames: 0" tile next to a "100.0%" I$ hit rate would be a
    /// confident summary of nothing.
    #[test]
    fn a_run_with_no_surviving_frames_gets_no_tiles() {
        let only_frame_zero = RunSamples {
            frames: vec![FrameRow {
                n: 0,
                ..FrameRow::default()
            }],
            ..RunSamples::default()
        };
        assert!(build(&only_frame_zero).tiles.is_empty());
        assert!(build(&RunSamples::default()).tiles.is_empty());
    }

    // ------------------------------------------------------ config line

    #[test]
    fn the_config_line_is_worldlibs_run_config_line() {
        let samples = RunSamples {
            detail: Some(sample_detail()),
            ..RunSamples::default()
        };
        assert_eq!(
            build(&samples).config_line.as_deref(),
            Some(
                "frame budget 555,549 cyc (60 fps) · scanout 164,400 · refill 100 · \
                 writeback 65 · wire wait 10 (calibrated)"
            )
        );
    }

    #[test]
    fn a_run_with_no_run_row_has_no_config_line() {
        assert_eq!(build(&RunSamples::default()).config_line, None);
    }

    // ------------------------------------------------------ diagnostics

    fn uart(lines: &[&str]) -> Vec<UartLine> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn diagnostics_parses_both_tables_out_of_the_recorded_uart() {
        let diag = Diagnostics::from_uart(&uart(&[
            "== GGO OS booted ==",
            "asset: MISS \"grooble.til\"",
            "asset: MISS \"grooble.til\"",
            "gfx: inert tileset \"x.til\" (asset_load=0)",
            "f=7| panicked at 'boom', src/main.rs:1:1",
        ]));

        let failures = diag.failures();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].kind, "MISS");
        assert_eq!(failures[0].path, "grooble.til");
        assert_eq!(failures[0].count, 2, "repeats collapse and count");
        assert_eq!(failures[1].kind, "inert tileset");

        let panics = diag.panics();
        assert_eq!(panics.len(), 1);
        assert_eq!(panics[0].frame, Some(7));
        assert!(panics[0].message.starts_with("panicked at"));

        assert_eq!(diag.empty_state(failures.len()), None);
        assert_eq!(diag.empty_state(panics.len()), None);
    }

    /// The distinction ggo-ide does not draw: a run that DID record UART
    /// and had nothing fail says "none recorded"...
    #[test]
    fn a_clean_run_that_recorded_uart_says_none_recorded() {
        let diag = Diagnostics::from_uart(&uart(&["== GGO OS booted ==", "hello"]));
        assert!(diag.failures().is_empty());
        assert!(diag.panics().is_empty());
        assert_eq!(diag.empty_state(0), Some(NONE_RECORDED));
    }

    /// ...while a run with no UART at all says so instead, because
    /// "nothing failed" is a claim its data cannot support.
    #[test]
    fn a_run_with_no_uart_says_so_rather_than_claiming_nothing_failed() {
        let diag = Diagnostics::from_uart(&[]);
        assert_eq!(diag, Diagnostics::NoUart);
        assert!(diag.failures().is_empty());
        assert!(diag.panics().is_empty());
        assert_eq!(diag.empty_state(0), Some(NO_UART));
        assert_ne!(NO_UART, NONE_RECORDED);
    }

    #[test]
    fn build_carries_the_diagnostics_for_the_loaded_uart() {
        let samples = RunSamples {
            uart: uart(&["asset: MISS \"a.til\""]),
            ..RunSamples::default()
        };
        assert_eq!(build(&samples).diagnostics.failures().len(), 1);
        assert_eq!(
            build(&RunSamples::default()).diagnostics,
            Diagnostics::NoUart
        );
    }
}
