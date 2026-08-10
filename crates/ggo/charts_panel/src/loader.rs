//! Off-thread perf-run listing: reads `~/.ggo/ggo_ide.db`'s `run`/`cart`
//! tables directly -- the charts-panel analog of `ggo_world_panel::loader`
//! / `ggo_sprite_panel::loader` (same one-shot background-pass
//! framing), reading the SAME database ggo-ide's `backend/db.rs` opens
//! and `backend/perf.rs::carts`/`cart_runs` query (read those two files
//! before touching this one -- this is a deliberately narrower port: one
//! flat "every run, newest first" query across all carts, id/date/cart
//! label, for the panel's run picker), plus (C2) the per-run sample
//! queries [`load_run_samples`] wraps -- faithful ports of that same
//! file's `run_frames_async`/`run_profile_async`.
//!
//! Unlike those loaders (a filesystem walk), this one opens a `turso`
//! connection, which needs an async runtime underneath. `ggo-ide`'s own
//! `backend/db.rs`/`backend/perf.rs` solve this by spinning up a private
//! single-threaded tokio runtime per call and blocking on it; [`list_runs`]
//! does the exact same thing. That block is safe here because it only
//! ever runs inside `cx.background_spawn` -- a background-executor
//! thread, not the UI thread (same rule `ggo_world_panel::loader::load_world`
//! leans on for its own synchronous fs walk).

use std::path::Path;

use turso::Value;

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

/// One frame's sampled counters -- the ONLY feed for every chart, exactly
/// as in ggo-ide (`backend/perf.rs::FrameRow`). Field set is that struct's,
/// minus the columns no chart reads: `i_hits`/`d_hits`/`over_budget` (KPI
/// tiles only, and this panel has no KPI row) are dropped, everything the
/// 13 charts plot is kept.
///
/// Units, per `ggo-emu-core/src/perfsim.rs`: every `*_wire`/budget value
/// is a raw 33.3 MHz **wire cycle** count (a 60 fps frame budget is
/// 555,549 of them); everything else is a raw per-frame **count** (or, for
/// `peak_spr_line`, a per-frame max). Nothing is milliseconds or bytes,
/// which is why the charts label no units -- same as ggo-ide's, which
/// renders raw cycles and a percent-of-budget rather than converting.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameRow {
    pub n: i64,
    pub instrs: i64,
    pub i_misses: i64,
    pub d_misses: i64,
    pub scanout_wire: i64,
    pub blit_wire: i64,
    pub miss_wire: i64,
    pub wire_total: i64,
    pub apu_underruns: i64,
    pub bg_evictions: i64,
    pub fg_evictions: i64,
    pub spr_evictions: i64,
    pub tile_load_wire: i64,
    pub apu_fetch_wire: i64,
    pub sc_upload: i64,
    pub sc_oam: i64,
    pub sc_layer: i64,
    pub sc_audio: i64,
    pub sc_other: i64,
    pub peak_spr_line: i64,
    pub bg_tiles_distinct: i64,
    pub spr_tiles_distinct: i64,
    /// From the joined `run` row, so it repeats on every frame. `None` for
    /// a device run (no wire model) -- the budget reference line is simply
    /// not drawn then.
    pub frame_budget_cycles: Option<i64>,
}

/// One per-frame I$ attribution sample (`backend/perf.rs::ProfileRow`).
/// Only native `ggo-emu --profile <elf>` runs write these; every other
/// run has none, which is exactly the gate ggo-ide uses to hide the two
/// per-function charts.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileRow {
    pub frame: i64,
    pub func: String,
    pub misses: i64,
    pub evicted: i64,
}

/// Everything one run's charts need, fetched in a single background pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunSamples {
    pub frames: Vec<FrameRow>,
    pub profile: Vec<ProfileRow>,
}

/// `perf.rs`'s `as_i64`: a `Real` column reads as its truncated integer,
/// anything else (including `Null`) as 0. Ported rather than using
/// `unwrap_or(0)` on a typed getter so a column written as a float by an
/// older ingest still decodes the same way ggo-ide decodes it.
fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        Value::Real(f) => *f as i64,
        _ => 0,
    }
}

/// `perf.rs`'s `as_opt_i64`: `Null` -> `None` (the device-run case for
/// `frame_budget_cycles`), numeric -> `Some`.
fn as_opt_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::Integer(i) => Some(*i),
        Value::Real(f) => Some(*f as i64),
        _ => None,
    }
}

/// `perf.rs`'s `as_string`.
fn as_string(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        _ => String::new(),
    }
}

/// Column count of [`FRAME_SQL`]'s select list -- a row shorter than this
/// is dropped rather than panicking on an index (`perf.rs` indexes
/// `row.get_value(0..=22)` unguarded, which is safe there only because it
/// owns the same literal).
const FRAME_COLUMNS: usize = 23;

/// A `frame`-table row in [`FRAME_SQL`]'s column order -> a [`FrameRow`].
/// Pure -- unit-tested below against hand-built `turso::Value` rows.
fn row_to_frame(row: &[Value]) -> Option<FrameRow> {
    if row.len() < FRAME_COLUMNS {
        return None;
    }
    Some(FrameRow {
        n: as_i64(&row[0]),
        instrs: as_i64(&row[1]),
        i_misses: as_i64(&row[2]),
        d_misses: as_i64(&row[3]),
        scanout_wire: as_i64(&row[4]),
        blit_wire: as_i64(&row[5]),
        miss_wire: as_i64(&row[6]),
        wire_total: as_i64(&row[7]),
        apu_underruns: as_i64(&row[8]),
        bg_evictions: as_i64(&row[9]),
        fg_evictions: as_i64(&row[10]),
        spr_evictions: as_i64(&row[11]),
        tile_load_wire: as_i64(&row[12]),
        apu_fetch_wire: as_i64(&row[13]),
        sc_upload: as_i64(&row[14]),
        sc_oam: as_i64(&row[15]),
        sc_layer: as_i64(&row[16]),
        sc_audio: as_i64(&row[17]),
        sc_other: as_i64(&row[18]),
        peak_spr_line: as_i64(&row[19]),
        bg_tiles_distinct: as_i64(&row[20]),
        spr_tiles_distinct: as_i64(&row[21]),
        frame_budget_cycles: as_opt_i64(&row[22]),
    })
}

fn row_to_profile(row: &[Value]) -> Option<ProfileRow> {
    let [frame, func, misses, evicted] = row else {
        return None;
    };
    Some(ProfileRow {
        frame: as_i64(frame),
        func: as_string(func),
        misses: as_i64(misses),
        evicted: as_i64(evicted),
    })
}

/// `perf.rs::run_frames_async`'s query, minus the three columns this
/// panel's charts never plot (`i_hits`/`d_hits`/`over_budget`). Same
/// join, same `WHERE`, same `ORDER BY f.n` -- the frame axis every chart
/// shares is that ordering, so it is NOT optional.
const FRAME_SQL: &str = "SELECT f.n, f.instrs, f.i_misses, f.d_misses,
                                f.scanout_wire, f.blit_wire, f.miss_wire, f.wire_total,
                                f.apu_underruns,
                                f.bg_evictions, f.fg_evictions, f.spr_evictions,
                                f.tile_load_wire, f.apu_fetch_wire,
                                f.sc_upload, f.sc_oam, f.sc_layer, f.sc_audio, f.sc_other,
                                f.peak_spr_line, f.bg_tiles_distinct, f.spr_tiles_distinct,
                                r.frame_budget_cycles
                         FROM frame f JOIN run r ON r.id = f.run_id
                         WHERE f.run_id = ?1
                         ORDER BY f.n";

/// `perf.rs::run_profile_async`'s query, minus `caller` (this panel has no
/// click-to-inspect caller breakdown -- the two per-function CHARTS only
/// ever group by `func`).
const PROFILE_SQL: &str =
    "SELECT frame, func, misses, evicted FROM profile WHERE run_id = ?1 ORDER BY frame";

/// One run's frames + I$ profile rows. Same blocking-runtime shape (and
/// same "must only be called from `cx.background_spawn`" rule) as
/// [`list_runs`]; a missing db reads as no samples rather than an error,
/// for the same reason.
///
/// Both queries return EVERY row -- no `LIMIT`, no SQL-side bucketing --
/// which is ggo-ide's behavior too (`perf.rs` has neither, and the
/// histogram binning happens in Rust). Ingest caps a run at 100,000
/// frames, and there is no index on `frame(run_id)`, so this is a full
/// scan; acceptable here because it runs off-thread and once per
/// selection, but see this task's report for the caveat.
pub fn load_run_samples(db_path: &Path, run_id: i64) -> Result<RunSamples, String> {
    if !db_path.exists() {
        return Ok(RunSamples::default());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(load_run_samples_async(db_path, run_id))
}

async fn load_run_samples_async(db_path: &Path, run_id: i64) -> Result<RunSamples, String> {
    let db = turso::Builder::new_local(&db_path.to_string_lossy())
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let conn = db.connect().map_err(|e| e.to_string())?;
    conn.busy_timeout(ggo_db::BUSY_TIMEOUT)
        .map_err(|e| e.to_string())?;

    let frames = query_rows(&conn, FRAME_SQL, run_id, row_to_frame).await?;
    // A run with no profile rows is the norm (only native `--profile`
    // runs write them), so this must not fail the whole load.
    let profile = query_rows(&conn, PROFILE_SQL, run_id, row_to_profile).await?;
    Ok(RunSamples { frames, profile })
}

/// `perf.rs::all_rows` + a row decoder in one pass: every row's column
/// values collected positionally, undecodable rows skipped.
async fn query_rows<T>(
    conn: &turso::Connection,
    sql: &str,
    run_id: i64,
    decode: fn(&[Value]) -> Option<T>,
) -> Result<Vec<T>, String> {
    let mut rows = conn.query(sql, [run_id]).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(|e| e.to_string())? {
        let n = row.column_count();
        let mut vals = Vec::with_capacity(n);
        for i in 0..n {
            vals.push(row.get_value(i).map_err(|e| e.to_string())?);
        }
        if let Some(decoded) = decode(&vals) {
            out.push(decoded);
        }
    }
    Ok(out)
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

    #[test]
    fn row_to_frame_maps_every_column_in_select_order() {
        // 23 columns in FRAME_SQL's order, each a distinct value so a
        // transposed mapping would fail loudly.
        let mut row: Vec<Value> = (0..22).map(|i| Value::Integer(i as i64 + 1)).collect();
        row.push(Value::Integer(555_549));
        let f = row_to_frame(&row).expect("a full row must decode");
        assert_eq!(f.n, 1);
        assert_eq!(f.instrs, 2);
        assert_eq!(f.i_misses, 3);
        assert_eq!(f.d_misses, 4);
        assert_eq!(f.scanout_wire, 5);
        assert_eq!(f.blit_wire, 6);
        assert_eq!(f.miss_wire, 7);
        assert_eq!(f.wire_total, 8);
        assert_eq!(f.apu_underruns, 9);
        assert_eq!(f.bg_evictions, 10);
        assert_eq!(f.fg_evictions, 11);
        assert_eq!(f.spr_evictions, 12);
        assert_eq!(f.tile_load_wire, 13);
        assert_eq!(f.apu_fetch_wire, 14);
        assert_eq!(f.sc_upload, 15);
        assert_eq!(f.sc_oam, 16);
        assert_eq!(f.sc_layer, 17);
        assert_eq!(f.sc_audio, 18);
        assert_eq!(f.sc_other, 19);
        assert_eq!(f.peak_spr_line, 20);
        assert_eq!(f.bg_tiles_distinct, 21);
        assert_eq!(f.spr_tiles_distinct, 22);
        assert_eq!(f.frame_budget_cycles, Some(555_549));
    }

    /// A device run has no wire model, so `run.frame_budget_cycles` is
    /// NULL -- that must read as `None` (no budget line drawn), not 0
    /// (a budget line at zero).
    #[test]
    fn row_to_frame_reads_a_null_budget_as_none() {
        let mut row: Vec<Value> = (0..22).map(|_| Value::Integer(0)).collect();
        row.push(Value::Null);
        assert_eq!(row_to_frame(&row).unwrap().frame_budget_cycles, None);
    }

    #[test]
    fn row_to_frame_rejects_a_short_row() {
        assert_eq!(row_to_frame(&[]), None);
        let short: Vec<Value> = (0..FRAME_COLUMNS - 1).map(|_| Value::Integer(0)).collect();
        assert_eq!(row_to_frame(&short), None);
    }

    #[test]
    fn row_to_profile_parses_and_rejects() {
        let row = vec![
            Value::Integer(3),
            Value::Text("update_entities".to_string()),
            Value::Integer(12),
            Value::Integer(4),
        ];
        assert_eq!(
            row_to_profile(&row),
            Some(ProfileRow {
                frame: 3,
                func: "update_entities".to_string(),
                misses: 12,
                evicted: 4,
            })
        );
        assert_eq!(row_to_profile(&[]), None);
    }

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
    /// two frames (inserted out of order, to pin `ORDER BY f.n`) and one
    /// profile row -- proves both queries decode against the ACTUAL
    /// column names/types, not just hand-built `Value` rows.
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
