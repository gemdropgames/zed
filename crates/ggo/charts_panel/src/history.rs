//! The device-run history rail: the `runs` rows `ggo-diag` writes, and one
//! run's `run_log`.
//!
//! # Why there is nothing to clone any more
//!
//! Until the PostgreSQL migration this module CLONED rows out of
//! `ggo-diag`'s own database into the fork's, because the two tools owned
//! separate SQLite files and the no-shared-dbs rule forbade a reader from
//! opening (and therefore migrating) another tool's file. One database
//! ends that: `runs`/`run_log` ARE the tables `ggo-diag` writes, so the
//! rail reads them directly and the clone -- along with every "the clone
//! found a stale copy" state it could produce -- is deleted.
//!
//! What survives from that design is the property that made it safe: these
//! reads are `SELECT`s and nothing else, so the rail may be refreshed at
//! any instant, including while a `ggo-diag` run is writing. A run caught
//! mid-pipeline simply reads as whatever it is at that moment (`state`
//! `'running'`, no verdict) and reads as finished on the next activation.
//! There is still no signal telling this panel that a device run ended --
//! it owns no child process -- and there still does not need to be.
//!
//! # What the rail assumes about run kinds -- nothing, deliberately
//!
//! `diag_db::list_runs` lists **every** run, undifferentiated (R1's
//! carried deferral, restated on that fn): the `runs` table has no
//! run-kind column, and a cart run vs a full-system run is distinguished
//! only by path convention at ingest time. This rail therefore does not
//! group, filter or label by kind -- it is one flat "device runs, newest
//! first" list, and the only per-row facts it shows (`started_at`,
//! `state`, `verdict`) are columns that genuinely exist. No column was
//! invented.
//!
//! These rows are also a DIFFERENT id space from the charts picker's:
//! `runs.id` is a `TEXT` primary key, while the perf `run` table's `id` is
//! a `BIGINT` (R1/R2's trap (3)). Nothing here ever hands one to the
//! other -- [`RunSummary::id`] only ever reaches [`log`].

use ggo_worldlib::charts::reports::diag_db;

pub use ggo_worldlib::charts::reports::diag_db::RunSummary;

/// How many history rows the rail pulls, newest first. ggo-ide's
/// `pages/device.rs::HISTORY_LIMIT`, itself `ggo-diag --list-runs`' own
/// default.
pub const HISTORY_LIMIT: i64 = 50;

/// The database is reachable and holds no device runs -- the ordinary
/// state of a machine `ggo-diag` has never run on. States the fact, not a
/// cause: a cleared database, a different `$GGO_DATABASE_URL`, or a
/// `ggo-diag` that failed before its first write all land here too.
pub const NO_RUNS: &str = "no device runs recorded yet";

/// No database url could be resolved at all (no `HOME`), so there is
/// nothing to read. Same hedging rule as [`NO_RUNS`]: it says what is
/// true, not why the environment is that way.
pub const NO_DATABASE_URL: &str = "could not resolve the ggo database url";

/// What the rail shows: the runs it found, plus the reason it found none
/// (or found them incompletely).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// Newest first, capped at the caller's limit. Undifferentiated by run
    /// kind -- see this module's doc.
    pub runs: Vec<RunSummary>,
    /// A legible reason to put under an empty rail: no rows, or the error
    /// a read failed with. `None` only when the rail has rows AND nothing
    /// went wrong.
    pub note: Option<String>,
}

/// The device runs, newest first, capped at `limit`.
///
/// **BLOCKING** (`diag_db`'s calls each wrap their body in
/// `ggo_db::block_on`, which panics inside a tokio runtime); background
/// threads only.
///
/// Never panics and never fails outright: an empty database and a read
/// that errors both come back as an empty rail with a reason, because on
/// a fresh machine the first is ordinary and the second is not worth
/// blanking the panel over.
pub fn load(db_url: &str, limit: i64) -> History {
    match diag_db::list_runs(db_url, limit) {
        Ok(runs) if runs.is_empty() => History {
            runs,
            note: Some(NO_RUNS.to_string()),
        },
        Ok(runs) => History { runs, note: None },
        Err(e) => History {
            runs: Vec::new(),
            note: Some(format!("could not list device runs: {e}")),
        },
    }
}

/// One device run's `run_log` lines, in `seq` order.
///
/// `run_log` (not `uart`) is the pipeline's own narration -- the same
/// content class ggo-ide's live diag stream carries, which is why its
/// history viewer reads as "the same log, after the fact" rather than as a
/// different kind of data. BLOCKING, same rule as [`load`].
pub fn log(db_url: &str, run_id: &str) -> Result<Vec<String>, String> {
    diag_db::run_log(db_url, run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::UNREACHABLE_DB_URL;
    use ggo_db::TestDb;
    use ggo_db::sqlx;

    /// One `ggo-diag` run and the two `run_log` lines it narrated.
    fn seed_diag_run(db: &TestDb, run_id: &str, started_at: &str) {
        let pool = db.pool();
        ggo_db::block_on(async {
            sqlx::query(
                "INSERT INTO runs (id, started_at, branch, commit_hash, git_describe, \
                 hostname, state, verdict) \
                 VALUES ($1, $2, 'main', 'abc123', 'v1.2.3', 'test-host', 'done', 'PASS')",
            )
            .bind(run_id)
            .bind(started_at)
            .execute(&pool)
            .await
            .unwrap();
            for (seq, text) in ["==> compile", "<== compile — ok"].iter().enumerate() {
                sqlx::query("INSERT INTO run_log (run_id, seq, text) VALUES ($1, $2, $3)")
                    .bind(run_id)
                    .bind(seq as i64)
                    .bind(*text)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        });
    }

    /// The rail's whole point: seeded diag runs come back newest first.
    #[test]
    fn load_lists_the_seeded_runs_newest_first() {
        let db = TestDb::new();
        seed_diag_run(&db, "run-older", "2026-08-01T00:00:00Z");
        seed_diag_run(&db, "run-newer", "2026-08-02T00:00:00Z");

        let history = load(db.url(), HISTORY_LIMIT);
        assert_eq!(history.note, None, "a rail with rows needs no reason");
        let ids: Vec<&str> = history.runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["run-newer", "run-older"]);
        assert_eq!(history.runs[0].state, "done");
        assert_eq!(history.runs[0].verdict.as_deref(), Some("PASS"));
    }

    #[test]
    fn load_respects_the_limit() {
        let db = TestDb::new();
        seed_diag_run(&db, "run-1", "2026-08-01T00:00:00Z");
        seed_diag_run(&db, "run-2", "2026-08-02T00:00:00Z");
        assert_eq!(load(db.url(), 1).runs.len(), 1);
    }

    /// A database with no `runs` rows: an empty rail with a legible
    /// reason, never a panic and never a silent blank.
    #[test]
    fn an_empty_database_says_no_runs_rather_than_erroring() {
        let db = TestDb::new();
        let history = load(db.url(), HISTORY_LIMIT);
        assert!(history.runs.is_empty());
        assert_eq!(history.note.as_deref(), Some(NO_RUNS));
    }

    /// An unreachable database reports what went wrong rather than
    /// panicking or blanking -- and the reason is a listing failure, which
    /// is a different sentence from "there are no runs".
    #[test]
    fn an_unreachable_database_reports_the_reason() {
        let history = load(UNREACHABLE_DB_URL, HISTORY_LIMIT);
        assert!(history.runs.is_empty());
        let note = history.note.expect("an unreachable db must say so");
        assert!(note.contains("could not list device runs"), "{note}");
        assert!(
            note.contains(ggo_db::INSTALL_HINT),
            "and the underlying hint survives for whoever is fixing it: {note}"
        );
        assert_ne!(note, NO_RUNS);
    }

    /// A run written mid-pipeline reads as what it is, and the next load
    /// sees the finished run -- the convergence the activation trigger
    /// leans on, now that nothing is copied and there is nothing to go
    /// stale.
    #[test]
    fn a_run_still_in_flight_is_read_again_by_the_next_load() {
        let db = TestDb::new();
        seed_diag_run(&db, "run-live", "2026-08-01T00:00:00Z");
        let pool = db.pool();
        ggo_db::block_on(async {
            sqlx::query("UPDATE runs SET state = 'running', verdict = NULL WHERE id = 'run-live'")
                .execute(&pool)
                .await
                .unwrap();
        });

        let mid = load(db.url(), HISTORY_LIMIT);
        assert_eq!(mid.runs[0].state, "running");
        assert_eq!(mid.runs[0].verdict, None);

        ggo_db::block_on(async {
            sqlx::query(
                "UPDATE runs SET state = 'done', updated_at = '2026-08-01T00:05:00Z', \
                 verdict = 'FAIL' WHERE id = 'run-live'",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO run_log (run_id, seq, text) VALUES ('run-live', 2, 'RESULT: FAIL')",
            )
            .execute(&pool)
            .await
            .unwrap();
        });

        let after = load(db.url(), HISTORY_LIMIT);
        assert_eq!(after.runs[0].state, "done");
        assert_eq!(after.runs[0].verdict.as_deref(), Some("FAIL"));
        assert_eq!(
            log(db.url(), "run-live").unwrap(),
            vec!["==> compile", "<== compile — ok", "RESULT: FAIL"],
            "the log is the writer's own rows, so it grows with the run"
        );
    }

    #[test]
    fn log_returns_the_runs_lines_in_seq_order() {
        let db = TestDb::new();
        seed_diag_run(&db, "run-1", "2026-08-01T00:00:00Z");
        assert_eq!(
            log(db.url(), "run-1").unwrap(),
            vec!["==> compile", "<== compile — ok"]
        );
    }

    /// A run whose log has no rows is an empty result rather than an
    /// error; an unreachable database is the error.
    #[test]
    fn log_is_empty_for_an_unknown_run_and_errors_for_an_unreachable_db() {
        let db = TestDb::new();
        seed_diag_run(&db, "run-1", "2026-08-01T00:00:00Z");
        assert!(log(db.url(), "no-such-run").unwrap().is_empty());
        assert!(log(UNREACHABLE_DB_URL, "run-1").is_err());
    }
}
