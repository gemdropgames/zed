//! One run selection's ENTIRE off-thread pass: the four queries, plus
//! every derivation the run-detail view renders.
//!
//! # Why this module exists
//!
//! Until F5.4 R3 the panel loaded `RunSamples` in `cx.background_spawn` and
//! then called `report::build` and `chart_set::build_charts` in the update
//! closure that received them -- i.e. on the UI thread. R2's review measured
//! that path at 35 ms per 10,000 UART lines and 327 ms per 100,000 in a
//! debug build, and it was survivable only because the fork's own producer
//! caps a run's persisted log at `ggo_emu_panel::uart::UART_LOG_CAP` (2000).
//! That is a **producer-side promise, not a reader-side one**: `uart` rows
//! also arrive from `ggo-emu`/`ggo-server`/`ggo_fixture`, and R3's stored
//! console reads every one of them back.
//!
//! So the whole pass -- query AND build -- now happens inside the ONE
//! background spawn `ChartsPanel::select_run` already had (no second load
//! path; the same `detail_generation` guard still covers it), and what
//! crosses back to the UI thread is [`Detail`], a finished view model whose
//! `Arc`s the render path only refcount-bumps.
//!
//! # The tripwire
//!
//! gpui's test scheduler runs background and foreground runnables on ONE
//! thread (`scheduler::test_scheduler`), so no test can prove this by
//! comparing thread ids. [`no_build_here`] is what proves it instead:
//! [`build`] bumps a counter, and `select_run`'s update closure holds a
//! guard that asserts the counter did not move while it ran. Anything
//! rebuilt on the UI thread fails every test that selects a run.

use std::path::Path;
use std::sync::Arc;

use crate::chart_geom::ChartSpec;
use crate::chart_set;
use crate::loader::{self, RunSamples};
use crate::report::{self, RunReport};

/// Everything the run-detail view renders, already derived.
///
/// The `Arc`s (not `Rc`s) are load-bearing twice over: this value is built
/// on a background thread and must be `Send` to come back, and
/// `render_chart`'s prepaint closure is `'static` and so has to OWN
/// whatever it reads -- a spec holds one `Vec<f32>` per series over the
/// whole run, and deep-cloning all thirteen charts on every hover
/// mouse-move would be ~8 MB of memcpy per frame at ingest's 100,000-frame
/// cap. Sharing the handle makes that a refcount bump.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    /// Assembled by `chart_set::build_charts`; empty when the run had no
    /// plottable frames.
    pub charts: Vec<Arc<ChartSpec>>,
    /// How many frames the default ignore filter dropped, so the header can
    /// say so -- see `chart_set::ignored_count`.
    pub ignored: usize,
    /// The non-chart half: KPI tiles, the header's run-config line, and the
    /// two diagnostic tables.
    pub report: RunReport,
    /// The run's persisted guest UART, verbatim and in `seq` order -- what
    /// the stored console shows. Every line, not a tail: the console is
    /// rendered through a `uniform_list`, which lays out only the rows on
    /// screen, so the reader imposes no cap of its own.
    pub console: Arc<Vec<String>>,
}

/// Query `run_id` out of `db_path` and derive everything from it.
///
/// **BLOCKING, and never on the UI thread.** [`loader::load_run_samples`]
/// spins four short-lived tokio runtimes (R1's concern (5)) and the build
/// half is the cost this module's doc opens with. `select_run`'s
/// `cx.background_spawn` is the only caller.
pub fn load(db_path: &Path, run_id: i64) -> Result<Detail, String> {
    let samples = loader::load_run_samples(db_path, run_id)?;
    Ok(build(&samples))
}

/// The pure half of [`load`] -- every derivation, no I/O.
pub fn build(samples: &RunSamples) -> Detail {
    #[cfg(test)]
    BUILDS.with(|n| n.set(n.get() + 1));
    Detail {
        ignored: chart_set::ignored_count(&samples.frames),
        report: report::build(samples),
        charts: chart_set::build_charts(samples)
            .into_iter()
            .map(Arc::new)
            .collect(),
        console: Arc::new(samples.uart.clone()),
    }
}

// How many times `build` has run ON THIS THREAD. Test-only, and the whole
// mechanism behind `no_build_here`.
//
// Thread-LOCAL, not a static counter: `cargo test` runs the suite's tests
// concurrently, so a shared counter would let one test's build fire
// another test's guard. Thread-local is also exactly the right scope for
// what is being asserted -- "nothing was built on THIS thread while the
// guard was held" -- and gpui's test scheduler runs a background runnable
// on the same thread as the update closure that receives it, so the guard
// still sees a UI-thread build if one happens.
#[cfg(test)]
thread_local! {
    static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A scope guard that asserts NOTHING was derived while it was held.
///
/// Held across `select_run`'s update closure -- the one piece of that load
/// that genuinely runs on the UI thread. If a future edit moves
/// `report::build`/`chart_set::build_charts` (or a `detail::build`/`load`
/// call) back into that closure, every test that selects a run fails with
/// the message below rather than silently reintroducing a 327 ms stall.
///
/// A zero-sized no-op outside `cfg(test)`: the counter it reads does not
/// exist in a release build, so this costs the shipped panel nothing.
#[cfg(test)]
pub struct NoBuildHere(usize);

/// Release build: nothing to count, nothing to check.
#[cfg(not(test))]
pub struct NoBuildHere;

#[cfg(test)]
pub fn no_build_here() -> NoBuildHere {
    NoBuildHere(BUILDS.with(std::cell::Cell::get))
}

#[cfg(not(test))]
pub fn no_build_here() -> NoBuildHere {
    NoBuildHere
}

#[cfg(test)]
impl Drop for NoBuildHere {
    fn drop(&mut self) {
        // Never assert while unwinding: a double panic aborts the process
        // and would bury whatever actually failed first.
        if std::thread::panicking() {
            return;
        }
        assert_eq!(
            BUILDS.with(std::cell::Cell::get),
            self.0,
            "a run's charts/report were built on the UI thread -- \
             detail::load belongs inside select_run's background spawn"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::FrameRow;

    fn samples() -> RunSamples {
        RunSamples {
            frames: (0..=3)
                .map(|n| FrameRow {
                    n,
                    instrs: 1_000,
                    wire_total: 100 + n,
                    ..FrameRow::default()
                })
                .collect(),
            uart: vec!["== GGO OS booted ==".to_string(), "wave 1".to_string()],
            ..RunSamples::default()
        }
    }

    /// The background half does the WHOLE job: what comes back is a
    /// finished view model, charts and tiles and console included, so the
    /// UI thread has nothing left but refcount bumps.
    #[test]
    fn build_returns_a_finished_view_model() {
        let detail = build(&samples());
        assert!(
            !detail.charts.is_empty(),
            "the charts are already assembled"
        );
        assert!(
            !detail.report.tiles.is_empty(),
            "the KPIs are already derived"
        );
        assert_eq!(
            detail.ignored, 1,
            "frame 0 is dropped by the default filter"
        );
        assert_eq!(detail.console.len(), 2, "the stored UART came along");
    }

    /// The stored console is the run's UART verbatim and in order -- no
    /// tail, no cap, no reformatting. The reader must not quietly impose
    /// the producer's `UART_LOG_CAP`.
    #[test]
    fn the_console_is_every_stored_line_in_order() {
        let lines: Vec<String> = (0..5_000).map(|i| format!("line {i}")).collect();
        let detail = build(&RunSamples {
            uart: lines.clone(),
            ..RunSamples::default()
        });
        assert_eq!(*detail.console, lines);
    }

    #[test]
    fn load_of_a_missing_db_file_is_an_empty_detail_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.db");
        let detail = load(&missing, 1).unwrap();
        assert!(detail.charts.is_empty());
        assert!(detail.console.is_empty());
        assert!(!missing.exists(), "a missing-file read creates no db file");
    }

    /// The tripwire catches the regression it names. Without this, the
    /// guard `select_run` holds would be untested scaffolding.
    #[test]
    #[should_panic(expected = "built on the UI thread")]
    fn the_tripwire_fires_when_something_is_built_inside_the_guarded_scope() {
        let _guard = no_build_here();
        let _ = build(&RunSamples::default());
    }

    /// ...and does not fire when nothing is.
    #[test]
    fn the_tripwire_is_quiet_when_the_scope_only_moves_an_already_built_detail() {
        let detail = build(&samples());
        let guard = no_build_here();
        let moved = detail.clone();
        drop(guard);
        assert_eq!(moved, detail);
    }
}
