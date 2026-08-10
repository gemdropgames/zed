//! The device-run history rail: `~/.ggo/diag.db` -> `~/.ggo/ggo_ide.db`
//! row cloning, the cloned `runs` rows, and one run's `run_log`.
//!
//! # Why this CLONES rather than reading `diag.db` live
//!
//! Because `diag.db` is another tool's file. `ggo-diag` (spawned as a real
//! child process, not linked in) owns it and writes it *while a run is in
//! flight*; this app owns `ggo_ide.db`. The project's no-shared-dbs rule --
//! "every tool owns its own SQLite file; a reader pulls rows into its own
//! db rather than opening the owner's file as a peer" -- is spelled out on
//! `ggo_db::open_existing`, and the mechanical reason is right there too:
//! **`ggo_db::open` MIGRATES what it opens**, i.e. runs `BEGIN IMMEDIATE`-
//! guarded `CREATE TABLE`/`ALTER TABLE` statements. Opening a foreign db
//! the ordinary way therefore takes a *write* transaction on it, which is
//! precisely what a reader promised not to do -- and, against a live
//! `ggo-diag` run, precisely what would collide with the writer. So
//! `diag_db::clone_runs` opens `diag.db` through `open_existing` (which
//! issues no statement at all beyond the open, and errors instead of
//! creating a missing file) and `SELECT`s rows out of it into our own
//! database; every subsequent read -- [`load`]'s listing, [`log`]'s log
//! viewer -- touches only `ggo_ide.db`.
//!
//! ggo-ide's own `backend/diag.rs` records the history that led there: the
//! Tauri app ran the diag pipeline IN-PROCESS and so already held the one
//! connection the pipeline wrote through, which is why nothing there ever
//! needed to clone. The subprocess architecture is what created the second
//! file, and cloning is the "IDE pulls, the secondary never pushes"
//! direction the rule requires.
//!
//! # What the rail assumes about run kinds -- nothing, deliberately
//!
//! `diag_db::list_cloned_runs` lists **every** cloned run, undifferentiated
//! (R1's carried deferral, restated on that fn): the `runs` table has no
//! run-kind column, and a cart run vs a full-system run is distinguished
//! only by path convention at ingest time. This rail therefore does not
//! group, filter or label by kind -- it is one flat "device runs, newest
//! first" list, and the only per-row facts it shows (`started_at`, `state`,
//! `verdict`) are columns that genuinely exist. No column was invented.
//!
//! These rows are also a DIFFERENT id space from the charts picker's:
//! `runs.id` is a `TEXT` primary key, while the perf `run` table's `id` is
//! an `INTEGER` (R1/R2's trap (3)). Nothing here ever hands one to the
//! other -- [`RunSummary::id`] only ever reaches [`log`].

use std::path::Path;

use ggo_worldlib::charts::reports::diag_db;

pub use ggo_worldlib::charts::reports::diag_db::RunSummary;

/// How many history rows the rail pulls, newest first. ggo-ide's
/// `pages/device.rs::HISTORY_LIMIT`, itself `ggo-diag --list-runs`' own
/// default.
pub const HISTORY_LIMIT: i64 = 50;

/// No `~/.ggo/diag.db` at all -- the ordinary state of a fresh machine, and
/// the reason the rail is empty on one.
///
/// States the fact and names what writes the file; it does NOT claim the
/// user has never run diagnostics (a cleared `~/.ggo`, a different `HOME`,
/// or a `ggo-diag` that failed before its first write all land here too).
pub const NO_DIAG_DB: &str =
    "no ~/.ggo/diag.db — that file is written by ggo-diag, which has recorded no run here yet";

/// The clone found `diag.db` but our own database has nothing in it. Same
/// hedging rule: this says what is true (no cloned rows), not why.
pub const NO_RUNS: &str = "no device runs recorded yet";

/// What the rail shows: the runs it found, plus the reason it found none
/// (or found them incompletely).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// Newest first, capped at the caller's limit. Undifferentiated by run
    /// kind -- see this module's doc.
    pub runs: Vec<RunSummary>,
    /// A legible reason to put under an empty (or stale) rail: no
    /// `diag.db`, no rows, or the error a read failed with. `None` only
    /// when the rail has rows AND nothing went wrong.
    pub note: Option<String>,
}

/// Pull whatever `diag_db_path` has that `ide_db_path` does not, then list
/// the cloned runs newest first.
///
/// **BLOCKING** (`diag_db`'s calls each spin their own current-thread tokio
/// runtime -- R1's concern (5)); background threads only.
///
/// Never panics and never fails outright: a missing `diag.db`, a missing
/// `ggo_ide.db`, and a read that errors all come back as an empty rail with
/// a reason, because on a fresh machine all three are ordinary.
///
/// **Neither path is created.** The `!exists()` guards in front are
/// load-bearing, exactly as `loader`'s are: `list_cloned_runs` opens
/// through `ggo_db::open`, which CREATES and migrates a missing file, so an
/// unguarded rail refresh would litter `~/.ggo` with a database on a
/// machine that has never run anything. (`clone_runs` is already safe on
/// its own side -- `open_existing` errors rather than creating -- but it is
/// skipped anyway so the reason the rail shows is this module's wording
/// rather than a raw "does not exist".)
pub fn load(diag_db_path: &Path, ide_db_path: &Path, limit: i64) -> History {
    let mut note = None;
    if !diag_db_path.exists() {
        note = Some(NO_DIAG_DB.to_string());
    } else if let Err(e) = diag_db::clone_runs(diag_db_path, ide_db_path) {
        note = Some(format!("could not read {}: {e}", diag_db_path.display()));
    }

    if !ide_db_path.exists() {
        return History {
            runs: Vec::new(),
            note: note.or_else(|| Some(NO_RUNS.to_string())),
        };
    }
    match diag_db::list_cloned_runs(ide_db_path, limit) {
        // A clone note survives a successful listing: previously-cloned
        // rows are still worth showing, and "these may be stale, here is
        // why" is more useful than either half alone.
        Ok(runs) if runs.is_empty() => History {
            runs,
            note: note.or_else(|| Some(NO_RUNS.to_string())),
        },
        Ok(runs) => History { runs, note },
        Err(e) => History {
            runs: Vec::new(),
            note: Some(format!("could not list device runs: {e}")),
        },
    }
}

/// One cloned run's `run_log` lines, in `seq` order.
///
/// `run_log` (not `uart`) is the pipeline's own narration -- the same
/// content class ggo-ide's live diag stream carries, which is why its
/// history viewer reads as "the same log, after the fact" rather than as a
/// different kind of data. BLOCKING, same rule as [`load`]; and the same
/// `!exists()` guard, for the same reason.
pub fn log(ide_db_path: &Path, run_id: &str) -> Result<Vec<String>, String> {
    if !ide_db_path.exists() {
        return Ok(Vec::new());
    }
    diag_db::cloned_run_log(ide_db_path, run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn seed_diag_run(path: &Path, run_id: &str, started_at: &str) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO runs (id, started_at, branch, commit_hash, git_describe, \
                 hostname, state, verdict) \
                 VALUES (?1, ?2, 'main', 'abc123', 'v1.2.3', 'test-host', 'done', 'PASS')",
                (run_id, started_at),
            )
            .await
            .unwrap();
            for (seq, text) in ["==> compile", "<== compile — ok"].iter().enumerate() {
                conn.execute(
                    "INSERT INTO run_log (run_id, seq, text) VALUES (?1, ?2, ?3)",
                    (run_id, seq as i64, *text),
                )
                .await
                .unwrap();
            }
        });
    }

    fn paths() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let diag = dir.path().join("diag.db");
        let ide = dir.path().join("ggo_ide.db");
        (dir, diag, ide)
    }

    /// The rail's whole point: seeded diag runs come back newest first,
    /// having been CLONED out of `diag.db` into our own database.
    #[test]
    fn load_clones_and_lists_the_seeded_runs_newest_first() {
        let (_dir, diag, ide) = paths();
        seed_diag_run(&diag, "run-older", "2026-08-01T00:00:00Z");
        seed_diag_run(&diag, "run-newer", "2026-08-02T00:00:00Z");

        let history = load(&diag, &ide, HISTORY_LIMIT);
        assert_eq!(history.note, None, "a rail with rows needs no reason");
        let ids: Vec<&str> = history.runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["run-newer", "run-older"]);
        assert_eq!(history.runs[0].state, "done");
        assert_eq!(history.runs[0].verdict.as_deref(), Some("PASS"));
        assert!(ide.exists(), "the clone created our own db, as it should");
    }

    #[test]
    fn load_respects_the_limit() {
        let (_dir, diag, ide) = paths();
        seed_diag_run(&diag, "run-1", "2026-08-01T00:00:00Z");
        seed_diag_run(&diag, "run-2", "2026-08-02T00:00:00Z");
        assert_eq!(load(&diag, &ide, 1).runs.len(), 1);
    }

    /// The common case on a fresh machine: no `diag.db`. An empty rail with
    /// a legible reason -- and, critically, no file left behind on either
    /// side. `turso::Builder::new_local` creates an empty database on open,
    /// so an unguarded read both fails AND litters `~/.ggo`.
    #[test]
    fn a_missing_diag_db_is_a_reason_not_an_error_and_creates_no_file() {
        let (_dir, diag, ide) = paths();
        let history = load(&diag, &ide, HISTORY_LIMIT);
        assert!(history.runs.is_empty());
        assert_eq!(history.note.as_deref(), Some(NO_DIAG_DB));
        assert!(!diag.exists(), "a missing diag.db must not be created");
        assert!(!ide.exists(), "nor may our own db be created by a read");
    }

    /// An unreadable `diag.db` (here: a file that is not a database at all)
    /// reports what went wrong rather than panicking or blanking.
    #[test]
    fn an_unreadable_diag_db_reports_the_reason() {
        let (_dir, diag, ide) = paths();
        std::fs::write(&diag, b"this is not a sqlite database").unwrap();
        let history = load(&diag, &ide, HISTORY_LIMIT);
        assert!(history.runs.is_empty());
        let note = history.note.expect("an unreadable db must say so");
        assert!(note.contains("diag.db"), "{note}");
    }

    /// `diag.db` exists but holds no runs: a different fact from "there is
    /// no diag.db", and a different sentence.
    #[test]
    fn an_empty_diag_db_says_no_runs_rather_than_no_database() {
        let (_dir, diag, ide) = paths();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            ggo_db::open(&diag).await.unwrap();
        });
        let history = load(&diag, &ide, HISTORY_LIMIT);
        assert!(history.runs.is_empty());
        assert_eq!(history.note.as_deref(), Some(NO_RUNS));
        assert_ne!(NO_RUNS, NO_DIAG_DB);
    }

    #[test]
    fn log_returns_the_runs_lines_in_seq_order() {
        let (_dir, diag, ide) = paths();
        seed_diag_run(&diag, "run-1", "2026-08-01T00:00:00Z");
        load(&diag, &ide, HISTORY_LIMIT);
        assert_eq!(
            log(&ide, "run-1").unwrap(),
            vec!["==> compile", "<== compile — ok"]
        );
    }

    /// A run whose log has no rows, and a database that does not exist at
    /// all, are both empty results rather than errors -- and the second
    /// creates nothing.
    #[test]
    fn log_is_empty_for_an_unknown_run_or_a_missing_db() {
        let (_dir, diag, ide) = paths();
        seed_diag_run(&diag, "run-1", "2026-08-01T00:00:00Z");
        load(&diag, &ide, HISTORY_LIMIT);
        assert!(log(&ide, "no-such-run").unwrap().is_empty());

        let (_dir2, _diag2, missing) = paths();
        assert!(log(&missing, "run-1").unwrap().is_empty());
        assert!(!missing.exists());
    }
}
