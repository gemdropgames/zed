//! Off-thread perf-run listing: reads `~/.ggo/ggo_ide.db`'s `run`/`cart`
//! tables directly -- the charts-panel analog of `ggo_world_panel::loader`
//! / `ggo_sprite_panel::loader` (same one-shot background-pass framing),
//! reading the SAME database ggo-ide's `backend/db.rs` opens.
//!
//! **Per-run reads go through worldlib, not through this module.** F5.4
//! Task R1 moved ggo-ide's whole `backend/perf.rs` into
//! [`ggo_worldlib::charts::reports::perf_db`], so [`load_run_samples`] is
//! now a thin fan-out over that module's `run_frames`/`run_profile`/
//! `run_detail`/`run_uart` rather than the hand-copied SQL and
//! `FrameRow`/`ProfileRow` shadows this file carried through C2. Those
//! shadows are gone: [`FrameRow`]/[`ProfileRow`]/[`RunDetail`]/
//! [`UartLine`] are re-exports of worldlib's own types, so the panel and
//! ggo-ide are now decoding the same columns through the same code.
//!
//! [`list_runs`] is the one query that stays local, because worldlib has
//! no equivalent: `perf_db` exposes `carts` + `cart_runs` (ggo-ide's
//! two-level cart -> run drill-down), while this panel's picker is one
//! flat "every run, newest first" list across all carts.
//!
//! Unlike the other panels' loaders (a filesystem walk), everything here
//! opens a `turso` connection, which needs an async runtime underneath.
//! `perf_db` (and [`list_runs`] below, matching it) spins up a private
//! current-thread tokio runtime per call and BLOCKS on it. That block is
//! only safe off the UI thread, so every call site in this crate must be
//! inside `cx.background_spawn` -- the same rule
//! `ggo_world_panel::loader::load_world` leans on for its own synchronous
//! fs walk, and R1's concern (5) restated.

use std::path::Path;

use ggo_worldlib::charts::reports::perf_db;
use turso::Value;

// Worldlib owns these now (F5.4 Task R1). Re-exported rather than
// re-declared so `chart_set`'s specs, the KPI derivations in
// `ggo_worldlib::charts::reports::kpi`, and `ggo_emu_panel`'s ingest
// round-trip test all name one type -- a private copy is exactly how a
// panel's numbers drift from the page it mirrors.
pub use ggo_worldlib::charts::reports::perf_db::{FrameRow, ProfileRow, RunDetail, UartLine};

/// One row of the runs list: enough to identify a run in the picker.
/// Selecting one loads its samples through [`load_run_samples`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunListing {
    pub id: i64,
    pub started_at: String,
    pub cart_name: String,
    pub label: Option<String>,
}

impl RunListing {
    /// The picker's display text: `"<cart> — <label>"` when the run has
    /// a non-empty label, else just the cart name -- mirrors how
    /// `RunPage.tsx`'s row title falls back on the TS side.
    pub fn display_title(&self) -> String {
        match self.label.as_deref() {
            Some(label) if !label.is_empty() => format!("{} — {label}", self.cart_name),
            _ => self.cart_name.clone(),
        }
    }
}

/// One raw db row (`id, started_at, cart_name, label`, in that column
/// order) -> a [`RunListing`], or `None` if the row is short or the
/// leading columns aren't the types the schema promises. Pure -- no I/O
/// -- so it is unit-tested directly against hand-built `turso::Value`
/// rows below, no db required.
fn row_to_listing(row: &[Value]) -> Option<RunListing> {
    let [id, started_at, cart_name, label] = row else {
        return None;
    };
    let Value::Integer(id) = id else {
        return None;
    };
    let Value::Text(started_at) = started_at else {
        return None;
    };
    let Value::Text(cart_name) = cart_name else {
        return None;
    };
    let label = match label {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    };
    Some(RunListing {
        id: *id,
        started_at: started_at.clone(),
        cart_name: cart_name.clone(),
        label,
    })
}

/// Every run across every cart, newest first (`started_at DESC, id DESC`
/// -- same tie-break `perf.rs::cart_runs_async` uses) -- the panel's
/// picker feed.
///
/// A missing db file reads as a clean empty list, not an error: the file
/// doesn't exist until `ggo-ide`, `ggo-emu`, or `ggo-server` first
/// ingests a run, and that's an ordinary "nothing to show yet" state for
/// a freshly opened project, not a failure. This MUST be checked BEFORE
/// opening -- `turso::Builder::new_local` creates an empty, tableless
/// file on open otherwise, which would then fail the `SELECT` with "no
/// such table: run" instead of reading as "no runs yet".
pub fn list_runs(db_path: &Path) -> Result<Vec<RunListing>, String> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(list_runs_async(db_path))
}

async fn list_runs_async(db_path: &Path) -> Result<Vec<RunListing>, String> {
    let db = turso::Builder::new_local(&db_path.to_string_lossy())
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let conn = db.connect().map_err(|e| e.to_string())?;
    conn.busy_timeout(ggo_db::BUSY_TIMEOUT)
        .map_err(|e| e.to_string())?;

    let mut rows = conn
        .query(
            "SELECT r.id, r.started_at, c.name, r.label
             FROM run r JOIN cart c ON c.id = r.cart_id
             ORDER BY r.started_at DESC, r.id DESC",
            (),
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(|e| e.to_string())? {
        let n = row.column_count();
        let mut vals = Vec::with_capacity(n);
        for i in 0..n {
            vals.push(row.get_value(i).map_err(|e| e.to_string())?);
        }
        if let Some(listing) = row_to_listing(&vals) {
            out.push(listing);
        }
    }
    Ok(out)
}

// ------------------------------------------------------- per-run samples

/// Everything one run's detail view needs, fetched in a single
/// background pass: the frames every chart plots, the I$ profile rows the
/// per-function charts need, the `run` row the header's config line and
/// KPI budget come off, and the persisted guest UART the failure/panic
/// tables parse.
///
/// All four are UNFILTERED, exactly as worldlib hands them over: the
/// ignore filter is a view-level recalibration applied by whoever derives
/// from them (`ggo_worldlib::charts::reports::ignore::apply` /
/// `apply_profile`), never inside a query and never here. See
/// `charts::reports`'s module doc -- skipping it folds frame 0's
/// cold-cache burst into every KPI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunSamples {
    pub frames: Vec<FrameRow>,
    pub profile: Vec<ProfileRow>,
    /// `None` when the run id has no `run` row (deleted mid-session, or
    /// an id that never existed) -- the header falls back to the picker's
    /// own listing rather than erroring.
    pub detail: Option<RunDetail>,
    /// Empty for a run recorded before UART persistence, and for a diag
    /// run whose serial log lives in `run_log` under a string id. That is
    /// NOT the same as "the run recorded UART with no failures in it" --
    /// `report::Diagnostics` is where the panel separates the two, and
    /// this emptiness is the only signal that can.
    pub uart: Vec<UartLine>,
}

/// One run's frames, profile rows, `run` row and UART.
///
/// Blocking, and it spins one short-lived tokio runtime per underlying
/// `perf_db` call (four of them), so it must only ever be called from
/// `cx.background_spawn` -- see this module's doc.
///
/// A missing db file reads as an empty result rather than an error, for
/// the same reason [`list_runs`] does, and the check MUST stay in front
/// of the `perf_db` calls: `turso::Builder::new_local` creates an empty,
/// tableless file on open, which would then fail every `SELECT` with
/// "no such table" instead of reading as "nothing ingested yet".
/// `perf_db` has no such guard of its own (ggo-ide only ever points it at
/// a db its own `backend::db::Db` has already created and migrated).
///
/// Every query returns EVERY row -- no `LIMIT`, no SQL-side bucketing --
/// which is ggo-ide's behavior too. Ingest caps a run at 100,000 frames
/// and there is no index on `frame(run_id)`, so this is a full scan;
/// acceptable because it runs off-thread, once per selection.
///
/// Errors are stringified with `{e:#}`, not `{e}`: `perf_db` builds its
/// `anyhow::Error`s out of `.context(..)`, whose plain `Display` shows
/// only the outermost context -- the alternate form appends the chain,
/// which is where the actual turso failure is.
pub fn load_run_samples(db_path: &Path, run_id: i64) -> Result<RunSamples, String> {
    if !db_path.exists() {
        return Ok(RunSamples::default());
    }
    // `{e:#}` (not `{e}`): `perf_db` returns `anyhow::Error`s built from
    // `.context(..)`, and the plain `Display` shows only the outermost
    // context -- the alternate form appends the chain, which is where the
    // actual turso failure is.
    Ok(RunSamples {
        frames: perf_db::run_frames(db_path, run_id).map_err(|e| format!("{e:#}"))?,
        // A run with no profile rows is the norm (only a native
        // `--profile` capture writes them), so an empty result here is
        // not an error -- `perf_db` already answers list queries that way.
        profile: perf_db::run_profile(db_path, run_id).map_err(|e| format!("{e:#}"))?,
        detail: perf_db::run_detail(db_path, run_id).map_err(|e| format!("{e:#}"))?,
        uart: perf_db::run_uart(db_path, run_id).map_err(|e| format!("{e:#}"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_to_listing_parses_a_labeled_run() {
        let row = vec![
            Value::Integer(7),
            Value::Text("2026-08-01T00:00:00Z".to_string()),
            Value::Text("demo".to_string()),
            Value::Text("arena".to_string()),
        ];
        let listing = row_to_listing(&row).expect("row should parse");
        assert_eq!(listing.id, 7);
        assert_eq!(listing.started_at, "2026-08-01T00:00:00Z");
        assert_eq!(listing.cart_name, "demo");
        assert_eq!(listing.label.as_deref(), Some("arena"));
    }

    #[test]
    fn row_to_listing_treats_a_null_label_as_none() {
        let row = vec![
            Value::Integer(1),
            Value::Text("2026-08-01T00:00:00Z".to_string()),
            Value::Text("demo".to_string()),
            Value::Null,
        ];
        assert_eq!(row_to_listing(&row).unwrap().label, None);
    }

    #[test]
    fn row_to_listing_rejects_a_short_or_mistyped_row() {
        assert_eq!(row_to_listing(&[]), None);
        assert_eq!(row_to_listing(&[Value::Integer(1)]), None);
        assert_eq!(
            row_to_listing(&[
                Value::Text("not-an-id".to_string()),
                Value::Text("2026-08-01T00:00:00Z".to_string()),
                Value::Text("demo".to_string()),
                Value::Null,
            ]),
            None
        );
    }

    #[test]
    fn display_title_falls_back_to_cart_name_when_unlabeled_or_empty() {
        let unlabeled = RunListing {
            id: 1,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            cart_name: "demo".to_string(),
            label: None,
        };
        assert_eq!(unlabeled.display_title(), "demo");

        let empty_label = RunListing {
            label: Some(String::new()),
            ..unlabeled
        };
        assert_eq!(empty_label.display_title(), "demo");
    }

    #[test]
    fn display_title_shows_the_label_when_present() {
        let listing = RunListing {
            id: 1,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            cart_name: "demo".to_string(),
            label: Some("arena".to_string()),
        };
        assert_eq!(listing.display_title(), "demo — arena");
    }

    #[test]
    fn list_runs_returns_empty_for_a_missing_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.db");
        assert_eq!(list_runs(&missing).unwrap(), Vec::new());
        assert!(
            !missing.exists(),
            "a missing-file read must not create the db file"
        );
    }

    /// A fixture db authored through `ggo_db::migrate` (the real schema,
    /// same pattern ggo-ide's `backend/perf.rs` tests use), seeded with
    /// one cart and two runs -- pins both the newest-first ordering and
    /// the label/no-label column mapping in one pass.
    #[test]
    fn list_runs_reads_seeded_rows_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 3, 'first')",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (2, 1, '2026-08-02T00:00:00Z', 2, NULL)",
                (),
            )
            .await
            .unwrap();
        });

        let runs = list_runs(&path).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, 2, "newest started_at sorts first");
        assert_eq!(runs[0].label, None);
        assert_eq!(runs[1].id, 1);
        assert_eq!(runs[1].label.as_deref(), Some("first"));
    }

    #[test]
    fn list_runs_is_empty_for_a_freshly_migrated_db_with_no_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            ggo_db::open(&path).await.unwrap();
        });
        assert_eq!(list_runs(&path).unwrap(), Vec::new());
    }

    // -------------------------------------------------- per-run samples

    // Column-order/decoding tests for `FrameRow`/`ProfileRow` are NOT
    // duplicated here any more: those rows are decoded by
    // `ggo_worldlib::charts::reports::perf_db`, whose own tests pin the
    // select-list order, the NULL-budget case and the short-row case.
    // What is still this crate's business is the fan-out below --
    // that `load_run_samples` asks for all four pieces, against the real
    // migrated schema, and that a missing db is empty rather than an error.

    #[test]
    fn load_run_samples_returns_nothing_for_a_missing_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.db");
        assert_eq!(
            load_run_samples(&missing, 1).unwrap(),
            RunSamples::default()
        );
        assert!(!missing.exists());
    }

    /// A fixture db through the real `ggo_db::migrate` schema, seeded with
    /// two frames (inserted out of order, to pin `ORDER BY f.n`), one
    /// profile row and two UART lines -- proves all four queries decode
    /// against the ACTUAL column names/types.
    #[test]
    fn load_run_samples_reads_frames_in_frame_number_order_with_profile_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, frame_budget_cycles, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 2, 555549, 'arena')",
                (),
            )
            .await
            .unwrap();
            for (n, wire_total, i_misses) in [(1i64, 200i64, 20i64), (0, 100, 10)] {
                conn.execute(
                    "INSERT INTO frame
                       (run_id, n, instrs, i_hits, i_misses, d_hits, d_misses,
                        scanout_wire, blit_wire, miss_wire, wire_total, over_budget,
                        sc_upload, bg_evictions, apu_fetch_wire)
                     VALUES (1, ?1, 1000, 0, ?2, 0, 5, 164400, 30, 40, ?3, 0, 7, 2, 9)",
                    (n, i_misses, wire_total),
                )
                .await
                .unwrap();
            }
            conn.execute(
                "INSERT INTO profile (run_id, frame, caller, func, misses, evicted)
                 VALUES (1, 1, '', 'update_entities', 12, 4)",
                (),
            )
            .await
            .unwrap();
            for (seq, text) in [(0i64, "== GGO OS booted =="), (1, "asset: MISS \"x.til\"")] {
                conn.execute(
                    "INSERT INTO uart (run_id, seq, text) VALUES (1, ?1, ?2)",
                    (seq, text),
                )
                .await
                .unwrap();
            }
        });

        let samples = load_run_samples(&path, 1).unwrap();
        assert_eq!(samples.frames.len(), 2);
        assert_eq!(samples.frames[0].n, 0, "ORDER BY f.n, not insertion order");
        assert_eq!(samples.frames[0].wire_total, 100);
        assert_eq!(samples.frames[1].n, 1);
        assert_eq!(samples.frames[1].i_misses, 20);
        assert_eq!(samples.frames[0].scanout_wire, 164_400);
        assert_eq!(samples.frames[0].sc_upload, 7);
        assert_eq!(samples.frames[0].bg_evictions, 2);
        assert_eq!(samples.frames[0].apu_fetch_wire, 9);
        assert_eq!(samples.frames[0].frame_budget_cycles, Some(555_549));
        assert_eq!(samples.profile.len(), 1);
        assert_eq!(samples.profile[0].func, "update_entities");
        assert_eq!(samples.profile[0].evicted, 4);

        // The three columns the panel's own `FrameRow` shadow used to
        // drop, and which every KPI tile needs -- the reason that shadow
        // is gone.
        assert_eq!(samples.frames[0].i_hits, 0);
        assert_eq!(samples.frames[0].d_hits, 0);
        assert!(!samples.frames[0].over_budget);

        // The `run` row (config line + KPI budget) and the UART the
        // failure tables parse, both fetched in this same pass.
        let detail = samples.detail.as_ref().expect("the run row exists");
        assert_eq!(detail.id, 1);
        assert_eq!(detail.cart_name, "demo");
        assert_eq!(detail.frame_budget_cycles, Some(555_549));
        assert_eq!(samples.uart.len(), 2, "UART comes back in `seq` order");
        assert_eq!(samples.uart[1], "asset: MISS \"x.til\"");
    }

    /// The overwhelmingly common case: a run with frames but no profile
    /// rows (anything but a native `--profile` capture). The profile
    /// query must come back empty, not error.
    #[test]
    fn load_run_samples_of_a_run_without_profile_rows_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames) VALUES (1, 1, 'x', 1)",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO frame (run_id, n, wire_total) VALUES (1, 0, 42)",
                (),
            )
            .await
            .unwrap();
        });

        let samples = load_run_samples(&path, 1).unwrap();
        assert_eq!(samples.frames.len(), 1);
        assert_eq!(samples.frames[0].wire_total, 42);
        assert!(samples.profile.is_empty());
        assert_eq!(
            samples.frames[0].frame_budget_cycles, None,
            "an un-set budget column reads as None, so no budget line is drawn"
        );
    }

    /// A run id with no rows at all (deleted/never-ingested) is an empty
    /// sample set, not an error -- the panel shows its no-samples message.
    #[test]
    fn load_run_samples_of_an_unknown_run_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            ggo_db::open(&path).await.unwrap();
        });
        assert_eq!(load_run_samples(&path, 999).unwrap(), RunSamples::default());
    }
}
