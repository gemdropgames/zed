//! Write a finished run's perf snapshot into the GemdropGo database.
//!
//! Ported field-for-field from `ggo-ide`'s `src/backend/ingest.rs`
//! (`parse_output`/`ingest_run`/`write_run_rows` and its `iso8601_utc`
//! helper) -- same required fields, same optional fields, same validation,
//! same caps, same tables, same column order, same single transaction.
//! Nothing about the schema is invented here: the input is whatever
//! `ggo_emu_core::perfsim::perf_json` produced for the run
//! ([`crate::drive`] calls it exactly the way ggo-ide's `CartStepper::
//! perf_json` does), and the output is the `cart`/`run`/`frame`/`uart`
//! (/`profile`/`dprofile`) row set that `ggo_charts_panel::loader`'s
//! `list_runs`/`load_run_samples` read back. `ggo_emu_panel`'s own tests
//! prove that round trip against the charts panel's real query functions.
//!
//! Only the transport differs from ggo-ide: there the perf JSON crosses an
//! `EmuCmd::Snapshot` request/reply channel to a persistent emu thread;
//! here the per-run emulator thread stores its own snapshot on the way out
//! (see [`crate::drive::Session::wait`]), so there is no round trip and no
//! "snapshot answered by the wrong stepper" race to guard against.
//!
//! The writer's SHAPE is `ggo-emu`'s `report.rs`: one `pool.begin()`
//! transaction, `RETURNING id` for the run row, and batched multi-row
//! `INSERT`s for the per-frame tables. The two are deliberately separate
//! copies rather than shared code -- they write the same tables from
//! different in-memory representations (a `FrameRecord` slice there,
//! column-oriented `Vec`s here).
//!
//! [`parse_output`] is pure (`&str -> Result<RunBody, String>`, no db, no
//! panics on malformed input); [`ingest_run`] is the blocking db half on
//! top of it and MUST be called from a background thread -- the panel runs
//! it inside `cx.background_spawn`, the same rule
//! `ggo_charts_panel::loader` follows for its reads.

use std::time::{SystemTime, UNIX_EPOCH};

use ggo_db::sqlx::{self, PgConnection, QueryBuilder, Row};
use serde_json::Value as Json;

/// Hard cap on frames per run ("sane size": ~28 min at 60 fps).
/// `ggo-ide::backend::ingest::MAX_FRAMES` verbatim.
pub const MAX_FRAMES: usize = 100_000;
/// Hard cap on function-attribution rows per `profile`/`dprofile` section.
const MAX_PROFILE_ROWS: usize = 500_000;
/// Cart names come from a 32-byte header slot; anything longer is junk.
const MAX_CART_NAME: usize = 64;

/// Rows per multi-row `INSERT`. Mirrors `ggo-emu`'s `report::INSERT_BATCH`
/// and exists for the same reason: a long run writes 100k frame rows, and
/// PostgreSQL caps one statement at 65535 bind parameters -- the widest
/// table here (`frame`, [`FRAME_COL_COUNT`] columns) stays well under that
/// at this batch size.
const INSERT_BATCH: usize = 1_000;

const INSERT_CART: &str = "INSERT INTO cart(name) VALUES ($1) ON CONFLICT DO NOTHING";
const SELECT_CART: &str = "SELECT id FROM cart WHERE name = $1";
const INSERT_RUN: &str = "INSERT INTO run(cart_id, started_at, frames, frame_budget_cycles,
                                          scanout_wire_cycles, refill_cycles, writeback_cycles,
                                          wire_wait_cycles, label)
                          VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                          RETURNING id";
/// `uart` carries TWO nullable keys -- `run_id` (ggo-diag's TEXT run id)
/// and `perf_run_id` (a perf `run.id`). This writer owns a perf run, so it
/// fills the latter; `perf_db::run_uart` reads both.
const INSERT_UART: &str = "INSERT INTO uart(perf_run_id, seq, text) VALUES ($1, $2, $3)";
/// Column lists for the batched writers; the `VALUES` tuples are appended
/// by [`QueryBuilder::push_values`].
const PROFILE_INSERT_HEAD: &str =
    "INSERT INTO profile(run_id, frame, caller, func, misses, evicted) ";
const DPROFILE_INSERT_HEAD: &str =
    "INSERT INTO dprofile(run_id, frame, caller, func, misses, evicted) ";
/// `run_id` + [`FRAME_COLS`] + [`EXTRA_COLS`], in that order. The `frame`
/// table has three more columns (`aok`/`afail`/`cyc`, written only by
/// ggo-diag's device ingest); they keep their defaults here.
const FRAME_INSERT_HEAD: &str = "INSERT INTO frame(run_id, n, instrs, i_hits, i_misses, d_hits,
                                                   d_misses, d_writebacks, evictions, blit_wire,
                                                   miss_wire, scanout_wire, wire_total, over_budget,
                                                   bg_hits, bg_misses, bg_evictions, bg_loads,
                                                   fg_hits, fg_misses, fg_evictions, fg_loads,
                                                   spr_hits, spr_misses, spr_evictions, spr_loads,
                                                   tile_load_wire, apu_fetch_wire, apu_underruns,
                                                   sc_upload, sc_oam, sc_layer, sc_audio, sc_other,
                                                   peak_spr_line, bg_tiles_distinct,
                                                   spr_tiles_distinct) ";
/// `run_id` + the 13 + 23 frame counters -- pinned by
/// `frame_columns_match_the_insert_head`.
const FRAME_COL_COUNT: usize = 37;
/// PostgreSQL's hard cap on bind parameters in one statement.
const MAX_BIND_PARAMS: usize = 65_535;
/// The batch size is only safe against that cap for the WIDEST table here,
/// so the widest table is what checks it -- at compile time, because a
/// batch that overflowed would fail at runtime on a long run only.
const _: () = assert!(INSERT_BATCH * FRAME_COL_COUNT <= MAX_BIND_PARAMS);

/// The 13 REQUIRED frame arrays, in `INSERT_FRAME` column order after
/// `run_id`. A body missing any of these is rejected.
const FRAME_COLS: [&str; 13] = [
    "n",
    "instrs",
    "i_hits",
    "i_misses",
    "d_hits",
    "d_misses",
    "d_writebacks",
    "evictions",
    "blit_wire",
    "miss_wire",
    "scanout_wire",
    "wire_total",
    "over_budget",
];

/// The PPU/APU/Tier-1/Tier-2 frame arrays, appended after [`FRAME_COLS`].
/// OPTIONAL: output that predates them omits the keys and each defaults to
/// a 0-filled column, so the ingest still succeeds.
const EXTRA_COLS: [&str; 23] = [
    "bg_hits",
    "bg_misses",
    "bg_evictions",
    "bg_loads",
    "fg_hits",
    "fg_misses",
    "fg_evictions",
    "fg_loads",
    "spr_hits",
    "spr_misses",
    "spr_evictions",
    "spr_loads",
    "tile_load_wire",
    "apu_fetch_wire",
    "apu_underruns",
    "sc_upload",
    "sc_oam",
    "sc_layer",
    "sc_audio",
    "sc_other",
    "peak_spr_line",
    "bg_tiles_distinct",
    "spr_tiles_distinct",
];

/// The 5 columns a `"profile"`/`"dprofile"` section carries.
const PROFILE_COLS: [&str; 5] = ["frame", "caller", "func", "misses", "evicted"];

// ---------------------------------------------------------------- time

/// ISO-8601 UTC "now", e.g. `2026-08-08T14:03:07Z`. Ported from ggo-ide's
/// `backend/ingest.rs` (itself Howard Hinnant's civil-from-days, all
/// integer) rather than pulling a date-time dependency in for one string.
fn iso8601_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso8601_utc(secs)
}

fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3_600,
        (tod % 3_600) / 60,
        tod % 60
    )
}

// ---------------------------------------------------------------- parsing

/// Validated perf-JSON, ready to insert.
#[derive(Debug, PartialEq)]
struct RunBody {
    cart: String,
    frame_budget_cycles: i64,
    scanout_wire_cycles: i64,
    refill_cycles: i64,
    writeback_cycles: i64,
    wire_wait_cycles: i64,
    /// Column-oriented, [`FRAME_COLS`] order; all `Vec`s equal length.
    cols: Vec<Vec<i64>>,
    /// Column-oriented, [`EXTRA_COLS`] order; each equal length to `cols`
    /// (0-filled when the body omitted that array).
    extra_cols: Vec<Vec<i64>>,
    profile: Vec<ProfileRow>,
    dprofile: Vec<ProfileRow>,
    /// `Some(original length)` when the frame arrays were cut to
    /// [`MAX_FRAMES`]: a run past ~28 minutes keeps its first 100k frames
    /// rather than losing its whole ingest.
    truncated_frames: Option<usize>,
}

/// One function-level miss/eviction row.
#[derive(Debug, PartialEq)]
struct ProfileRow {
    frame: i64,
    caller: String,
    func: String,
    misses: i64,
    evicted: i64,
}

/// Parse one optional `"profile"`/`"dprofile"` section. A missing key is
/// an empty `Vec`, not an error -- a cart run has no ELF/DWARF symbols to
/// attribute against, so it never emits one.
fn parse_profile_section(
    obj: &serde_json::Map<String, Json>,
    key: &str,
) -> Result<Vec<ProfileRow>, String> {
    let Some(section) = obj.get(key) else {
        return Ok(Vec::new());
    };
    let section = section
        .as_object()
        .ok_or_else(|| format!("\"{key}\" must be an object of arrays"))?;

    let arr = |name: &str| -> Result<&Vec<Json>, String> {
        section
            .get(name)
            .and_then(Json::as_array)
            .ok_or_else(|| format!("\"{key}.{name}\" must be an array"))
    };
    let frame = arr("frame")?;
    let n = frame.len();
    if n > MAX_PROFILE_ROWS {
        return Err(format!("\"{key}\" exceeds {MAX_PROFILE_ROWS} rows"));
    }
    for name in PROFILE_COLS {
        let a = arr(name)?;
        if a.len() != n {
            return Err(format!(
                "\"{key}\" arrays disagree on length: \"{key}.{name}\" has {} entries, expected {n}",
                a.len()
            ));
        }
    }
    let caller = arr("caller")?;
    let func = arr("func")?;
    let misses = arr("misses")?;
    let evicted = arr("evicted")?;

    let int_at = |a: &[Json], i: usize, name: &str| -> Result<i64, String> {
        a[i].as_i64()
            .filter(|v| *v >= 0)
            .ok_or_else(|| format!("\"{key}.{name}\" must contain non-negative integers"))
    };
    let str_at = |a: &[Json], i: usize, name: &str| -> Result<String, String> {
        a[i].as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("\"{key}.{name}\" must contain strings"))
    };

    (0..n)
        .map(|i| {
            Ok(ProfileRow {
                frame: int_at(frame, i, "frame")?,
                caller: str_at(caller, i, "caller")?,
                func: str_at(func, i, "func")?,
                misses: int_at(misses, i, "misses")?,
                evicted: int_at(evicted, i, "evicted")?,
            })
        })
        .collect()
}

/// A non-negative integer field (rejects floats, negatives, strings).
fn int_field(obj: &serde_json::Map<String, Json>, key: &str) -> Result<i64, String> {
    obj.get(key)
        .ok_or_else(|| format!("missing field \"{key}\""))?
        .as_i64()
        .filter(|v| *v >= 0)
        .ok_or_else(|| format!("\"{key}\" must be a non-negative integer"))
}

/// Parse the exact perf-JSON text a run produced into a validated
/// [`RunBody`]. Pure: no I/O, no db, malformed input always returns `Err`,
/// never panics.
fn parse_output(output: &str) -> Result<RunBody, String> {
    let doc: Json = serde_json::from_str(output).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = doc.as_object().ok_or("output must be a JSON object")?;

    let cart = obj
        .get("cart")
        .and_then(Json::as_str)
        .ok_or("\"cart\" must be a string")?;
    if cart.trim().is_empty() {
        return Err("\"cart\" must be non-empty".into());
    }
    if cart.len() > MAX_CART_NAME {
        return Err(format!("\"cart\" longer than {MAX_CART_NAME} bytes"));
    }

    let frame_budget_cycles = int_field(obj, "frame_budget_cycles")?;
    let scanout_wire_cycles = int_field(obj, "scanout_wire_cycles")?;
    let refill_cycles = int_field(obj, "refill_cycles")?;
    let writeback_cycles = int_field(obj, "writeback_cycles")?;
    // Optional: pre-wait-model output omits it -> 0.
    let wire_wait_cycles = obj
        .get("wire_wait_cycles")
        .and_then(Json::as_i64)
        .unwrap_or(0);

    let frames = obj
        .get("frames")
        .and_then(Json::as_object)
        .ok_or("\"frames\" must be an object of arrays")?;

    let mut cols: Vec<Vec<i64>> = Vec::with_capacity(FRAME_COLS.len());
    let mut len: Option<usize> = None;
    let mut truncated_frames: Option<usize> = None;
    for name in FRAME_COLS {
        let arr = frames
            .get(name)
            .and_then(Json::as_array)
            .ok_or_else(|| format!("\"frames.{name}\" must be an array"))?;
        let arr = if arr.len() > MAX_FRAMES {
            truncated_frames = Some(arr.len());
            &arr[..MAX_FRAMES]
        } else {
            arr.as_slice()
        };
        match len {
            None => len = Some(arr.len()),
            Some(l) if l != arr.len() => {
                return Err(format!(
                    "frame arrays disagree on length: \"frames.{name}\" has {} entries, expected {l}",
                    arr.len()
                ));
            }
            Some(_) => {}
        }
        let mut col = Vec::with_capacity(arr.len());
        for v in arr {
            let v = v
                .as_i64()
                .filter(|v| *v >= 0)
                .ok_or_else(|| format!("\"frames.{name}\" must contain non-negative integers"))?;
            if name == "over_budget" && v > 1 {
                return Err("\"frames.over_budget\" entries must be 0 or 1".into());
            }
            col.push(v);
        }
        cols.push(col);
    }

    let n = len.unwrap_or(0);
    let mut extra_cols: Vec<Vec<i64>> = Vec::with_capacity(EXTRA_COLS.len());
    for name in EXTRA_COLS {
        match frames.get(name).and_then(Json::as_array) {
            None => extra_cols.push(vec![0; n]),
            Some(arr) => {
                let arr = if truncated_frames.is_some() && arr.len() > n {
                    &arr[..n]
                } else {
                    arr.as_slice()
                };
                if arr.len() != n {
                    return Err(format!(
                        "frame arrays disagree on length: \"frames.{name}\" has {} entries, expected {n}",
                        arr.len()
                    ));
                }
                let mut col = Vec::with_capacity(arr.len());
                for v in arr {
                    let v = v.as_i64().filter(|v| *v >= 0).ok_or_else(|| {
                        format!("\"frames.{name}\" must contain non-negative integers")
                    })?;
                    col.push(v);
                }
                extra_cols.push(col);
            }
        }
    }

    let profile = parse_profile_section(obj, "profile")?;
    let dprofile = parse_profile_section(obj, "dprofile")?;

    Ok(RunBody {
        truncated_frames,
        cart: cart.to_owned(),
        frame_budget_cycles,
        scanout_wire_cycles,
        refill_cycles,
        writeback_cycles,
        wire_wait_cycles,
        cols,
        extra_cols,
        profile,
        dprofile,
    })
}

// ---------------------------------------------------------------- write

/// The ids [`ingest_run`] created (or reused, for `cart_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunId {
    pub run_id: i64,
    pub cart_id: i64,
    /// See `RunBody::truncated_frames`.
    pub truncated_frames: Option<usize>,
}

/// Parse `output` and write it into the database at `db_url` as a
/// `cart`/`run`/`frame` (/`uart`/`profile`/`dprofile`) row set -- the same
/// shape the native `ggo-emu`/`ggo-server` writers and `ggo-diag`'s device
/// ingest use, and the shape `ggo_charts_panel::loader` reads.
///
/// The database is migrated on first connection by `ggo_db::pool_for_async`
/// (the same call every other tool makes), so a fresh checkout's very first
/// ingest brings the schema into existence.
///
/// `started_at` is stamped here, from the host clock. `uart` is the run's
/// diagnostic lines (zero rows written when empty, not an error) and
/// `label` an optional free-text run identity for the `run.label` column.
///
/// BLOCKING: drives ggo-db's runtime to completion and returns once the
/// write commits. Callers on the UI thread must go through
/// `cx.background_spawn`; callers already inside a tokio runtime must not
/// call this at all (`ggo_db::block_on` panics there, by design).
pub fn ingest_run(
    db_url: &str,
    output: &str,
    uart: &[String],
    label: Option<&str>,
) -> Result<RunId, String> {
    let body = parse_output(output)?;
    ggo_db::block_on(write_run(db_url, &body, uart, label))
}

/// The whole run in ONE transaction: a partially written run (frames
/// without their `run` row, or half a profile) is never observable, and a
/// failure part-way through leaves the database exactly as it was.
async fn write_run(
    db_url: &str,
    run: &RunBody,
    uart: &[String],
    label: Option<&str>,
) -> Result<RunId, String> {
    let pool = ggo_db::pool_for_async(db_url).await?;
    let err = |e: sqlx::Error| e.to_string();
    let n_frames = run.cols[0].len();
    let started_at = iso8601_utc_now();

    let mut tx = pool.begin().await.map_err(err)?;
    sqlx::query(INSERT_CART)
        .bind(run.cart.as_str())
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    let cart_id: i64 = sqlx::query(SELECT_CART)
        .bind(run.cart.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(err)?
        .try_get(0)
        .map_err(err)?;
    let run_id = write_run_rows(&mut tx, run, cart_id, &started_at, n_frames, uart, label).await?;
    tx.commit().await.map_err(err)?;
    Ok(RunId {
        run_id,
        cart_id,
        truncated_frames: run.truncated_frames,
    })
}

/// Every row of one run, on an open transaction: the `run` row (whose id
/// the rest hang off), its frames, its UART lines and its two profile
/// sections. Split out from [`write_run`] so the transaction's commit and
/// rollback live in one place.
#[allow(clippy::too_many_arguments)]
async fn write_run_rows(
    conn: &mut PgConnection,
    run: &RunBody,
    cart_id: i64,
    started_at: &str,
    n_frames: usize,
    uart: &[String],
    label: Option<&str>,
) -> Result<i64, String> {
    let err = |e: sqlx::Error| e.to_string();
    let run_id: i64 = sqlx::query(INSERT_RUN)
        .bind(cart_id)
        .bind(started_at)
        .bind(n_frames as i64)
        .bind(run.frame_budget_cycles)
        .bind(run.scanout_wire_cycles)
        .bind(run.refill_cycles)
        .bind(run.writeback_cycles)
        .bind(run.wire_wait_cycles)
        .bind(label)
        .fetch_one(&mut *conn)
        .await
        .map_err(err)?
        .try_get(0)
        .map_err(err)?;

    // Column-oriented input, row-oriented output: each batch walks a
    // window of frame indices and reads the i-th entry out of every
    // column, in `FRAME_INSERT_HEAD` order.
    let mut from = 0;
    while from < n_frames {
        let to = (from + INSERT_BATCH).min(n_frames);
        let mut qb = QueryBuilder::new(FRAME_INSERT_HEAD);
        qb.push_values(from..to, |mut b, i| {
            b.push_bind(run_id);
            for c in &run.cols {
                b.push_bind(c[i]);
            }
            for c in &run.extra_cols {
                b.push_bind(c[i]);
            }
        });
        qb.build().execute(&mut *conn).await.map_err(err)?;
        from = to;
    }

    for (seq, line) in uart.iter().enumerate() {
        sqlx::query(INSERT_UART)
            .bind(run_id)
            .bind(seq as i64)
            .bind(line.as_str())
            .execute(&mut *conn)
            .await
            .map_err(err)?;
    }
    write_profile(conn, PROFILE_INSERT_HEAD, run_id, &run.profile).await?;
    write_profile(conn, DPROFILE_INSERT_HEAD, run_id, &run.dprofile).await?;
    Ok(run_id)
}

/// Batched writer shared by the `profile` and `dprofile` tables (same
/// shape, different table): `head` carries the table + column list.
async fn write_profile(
    conn: &mut PgConnection,
    head: &str,
    run_id: i64,
    rows: &[ProfileRow],
) -> Result<(), String> {
    for chunk in rows.chunks(INSERT_BATCH) {
        let mut qb = QueryBuilder::new(head);
        qb.push_values(chunk, |mut b, r| {
            b.push_bind(run_id)
                .push_bind(r.frame)
                .push_bind(r.caller.as_str())
                .push_bind(r.func.as_str())
                .push_bind(r.misses)
                .push_bind(r.evicted);
        });
        qb.build()
            .execute(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One scalar off the test database, for the row-count assertions
    /// below. `ggo_db::block_on` for the same reason the ingest uses it:
    /// these are plain `#[test]`s, not `#[tokio::test]`s.
    fn scalar_i64(db_url: &str, sql: &str, bind: Option<i64>) -> i64 {
        ggo_db::block_on(async {
            let pool = ggo_db::pool_for_async(db_url).await.unwrap();
            let mut q = sqlx::query_scalar::<_, i64>(sql);
            if let Some(b) = bind {
                q = q.bind(b);
            }
            q.fetch_one(&pool).await.unwrap()
        })
    }

    /// A minimal, valid perf-JSON body with `n` frames. Hand-built rather
    /// than cribbed from `ggo_fixture` (ggo-ide's own test fixture crate,
    /// which this fork does not depend on) -- but the SHAPE is not
    /// guessed: `real_perf_json_from_a_cart_run_ingests_cleanly` below
    /// drives an actual cart and ingests `perfsim::perf_json`'s real
    /// output, so a drift in the emitter would fail there.
    fn output_for(cart: &str, n: usize) -> String {
        let mut frames = serde_json::Map::new();
        for name in FRAME_COLS {
            let col: Vec<i64> = match name {
                "n" => (0..n as i64).collect(),
                "over_budget" => vec![0; n],
                _ => (0..n as i64).map(|i| i + 1).collect(),
            };
            frames.insert(name.to_string(), serde_json::json!(col));
        }
        serde_json::json!({
            "cart": cart,
            "frame_budget_cycles": 555_549,
            "scanout_wire_cycles": 164_400,
            "refill_cycles": 8,
            "writeback_cycles": 8,
            "wire_wait_cycles": 2,
            "frames": Json::Object(frames),
        })
        .to_string()
    }

    // ------------------------------------------------------------- time

    #[test]
    fn iso8601_known_epochs() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(iso8601_utc(1_784_476_800), "2026-07-19T16:00:00Z");
        let now = iso8601_utc_now();
        assert_eq!(now.len(), 20, "{now} not ISO-8601 shaped");
        assert!(now.ends_with('Z'));
    }

    // ----------------------------------------------------- parse_output

    #[test]
    fn parse_output_accepts_a_minimal_body_and_zero_fills_the_optional_columns() {
        let body = parse_output(&output_for("t", 3)).unwrap();
        assert_eq!(body.cart, "t");
        assert_eq!(body.cols.len(), FRAME_COLS.len());
        assert!(body.cols.iter().all(|c| c.len() == 3));
        assert_eq!(body.extra_cols.len(), EXTRA_COLS.len());
        assert!(
            body.extra_cols.iter().all(|c| c == &vec![0, 0, 0]),
            "omitted PPU/APU columns default to 0, they are not an error"
        );
        assert!(body.profile.is_empty());
        assert!(body.dprofile.is_empty());
    }

    #[test]
    fn parse_output_carries_present_optional_columns_through() {
        let mut v: Json = serde_json::from_str(&output_for("t", 3)).unwrap();
        v["frames"]["bg_hits"] = serde_json::json!([100, 101, 102]);
        let body = parse_output(&v.to_string()).unwrap();
        assert_eq!(body.extra_cols[0], vec![100, 101, 102]);
    }

    #[test]
    fn parse_output_rejects_junk_without_panicking() {
        let mut valid: Json = serde_json::from_str(&output_for("t", 3)).unwrap();
        for (label, text) in [
            ("not json", "nope".to_string()),
            ("not an object", "[1,2]".to_string()),
            (
                "the empty snapshot ggo-ide uses for a dead stepper",
                "{}".to_string(),
            ),
            ("empty cart", {
                let mut v = valid.clone();
                v["cart"] = serde_json::json!("   ");
                v.to_string()
            }),
            ("float cycles", {
                let mut v = valid.clone();
                v["refill_cycles"] = serde_json::json!(1.5);
                v.to_string()
            }),
            ("negative counter", {
                let mut v = valid.clone();
                v["frames"]["instrs"] = serde_json::json!([-1, 0, 0]);
                v.to_string()
            }),
            ("length mismatch", {
                let mut v = valid.clone();
                v["frames"]["d_misses"] = serde_json::json!([0]);
                v.to_string()
            }),
            ("missing array", {
                valid["frames"]
                    .as_object_mut()
                    .unwrap()
                    .remove("wire_total");
                valid.to_string()
            }),
        ] {
            assert!(
                parse_output(&text).is_err(),
                "{label} should be rejected, not panic"
            );
        }
    }

    #[test]
    fn parse_output_rejects_over_budget_outside_zero_or_one() {
        let mut v: Json = serde_json::from_str(&output_for("t", 3)).unwrap();
        v["frames"]["over_budget"] = serde_json::json!([0, 2, 0]);
        let err = parse_output(&v.to_string()).unwrap_err();
        assert!(err.contains("over_budget"), "{err}");
    }

    #[test]
    fn parse_output_truncates_oversized_runs_instead_of_rejecting_them() {
        let mut v: Json = serde_json::from_str(&output_for("t", 3)).unwrap();
        let big = serde_json::json!(vec![0; MAX_FRAMES + 1]);
        for name in FRAME_COLS {
            v["frames"][name] = big.clone();
        }
        v["frames"][EXTRA_COLS[0]] = big;
        let body = parse_output(&v.to_string()).expect("a long run still ingests");
        assert!(body.cols.iter().all(|c| c.len() == MAX_FRAMES));
        assert!(body.extra_cols.iter().all(|c| c.len() == MAX_FRAMES));
        assert_eq!(body.truncated_frames, Some(MAX_FRAMES + 1));
        assert_eq!(
            parse_output(&output_for("t", 3)).unwrap().truncated_frames,
            None
        );
    }

    #[test]
    fn parse_profile_section_round_trips_rows_in_order() {
        let mut v: Json = serde_json::from_str(&output_for("t", 3)).unwrap();
        v["profile"] = serde_json::json!({
            "frame": [0, 0],
            "caller": ["f", "g"],
            "func": ["f", "g::inline"],
            "misses": [3, 1],
            "evicted": [0, 2],
        });
        let body = parse_output(&v.to_string()).unwrap();
        assert_eq!(body.profile.len(), 2);
        assert_eq!(body.profile[0].caller, "f");
        assert_eq!(body.profile[1].func, "g::inline");
        assert!(
            body.dprofile.is_empty(),
            "an omitted dprofile is empty, not an error"
        );
    }

    #[test]
    fn parse_profile_section_rejects_length_mismatch() {
        let mut v: Json = serde_json::from_str(&output_for("t", 3)).unwrap();
        v["profile"] = serde_json::json!({
            "frame": [0, 0],
            "caller": ["f"],
            "func": ["f", "g"],
            "misses": [1, 1],
            "evicted": [0, 0],
        });
        let err = parse_output(&v.to_string()).unwrap_err();
        assert!(err.contains("profile.caller"), "{err}");
    }

    // ------------------------------------------------------- ingest_run

    #[test]
    fn frame_columns_match_the_insert_head() {
        assert_eq!(
            1 + FRAME_COLS.len() + EXTRA_COLS.len(),
            FRAME_COL_COUNT,
            "run_id plus every frame counter, one bind each"
        );
        // Not just the arity: the binds are pushed positionally, so a
        // REORDER of either array silently writes every counter into the
        // wrong column. Compare the names the statement lists, in order.
        const HEAD_PREFIX: &str = "INSERT INTO frame(";
        const HEAD_SUFFIX: &str = ")";
        let named: Vec<&str> = FRAME_INSERT_HEAD
            .trim()
            .strip_prefix(HEAD_PREFIX)
            .and_then(|list| list.strip_suffix(HEAD_SUFFIX))
            .expect("the insert head is `INSERT INTO frame(<columns>)`")
            .split(',')
            .map(str::trim)
            .collect();
        let bound: Vec<&str> = std::iter::once("run_id")
            .chain(FRAME_COLS)
            .chain(EXTRA_COLS)
            .collect();
        assert_eq!(
            named, bound,
            "the column list names exactly the bound columns, in bind order"
        );
    }

    #[test]
    fn ingest_run_writes_its_cart_and_run_rows() {
        let db = ggo_db::TestDb::new();
        let out = ingest_run(db.url(), &output_for("demo", 3), &[], None).unwrap();
        assert_eq!(out.run_id, 1);
        assert_eq!(out.cart_id, 1);
        assert_eq!(
            scalar_i64(
                db.url(),
                "SELECT COUNT(*) FROM frame WHERE run_id = $1",
                Some(out.run_id)
            ),
            3,
            "one frame row per frame in the body"
        );
    }

    #[test]
    fn ingest_run_reuses_the_cart_row_across_two_runs() {
        let db = ggo_db::TestDb::new();
        ingest_run(db.url(), &output_for("demo", 3), &[], None).unwrap();
        let out = ingest_run(db.url(), &output_for("demo", 3), &[], None).unwrap();
        assert_eq!(out.run_id, 2, "a second run gets a new run row");
        assert_eq!(out.cart_id, 1, "the same cart name reuses the cart row");
    }

    #[test]
    fn ingest_run_rejects_malformed_output_before_touching_the_db() {
        let db = ggo_db::TestDb::new();
        assert!(ingest_run(db.url(), "not json at all", &[], None).is_err());
        assert_eq!(
            scalar_i64(db.url(), "SELECT COUNT(*) FROM run", None),
            0,
            "a parse failure must never write a run row"
        );
    }

    #[test]
    fn ingest_run_persists_uart_lines_in_seq_order() {
        let db = ggo_db::TestDb::new();
        let lines = vec!["[run] green.cart".to_string(), "[run ended] stopped".into()];
        let out = ingest_run(db.url(), &output_for("uart-check", 1), &lines, None).unwrap();

        let got: Vec<(i64, String)> = ggo_db::block_on(async {
            let pool = ggo_db::pool_for_async(db.url()).await.unwrap();
            sqlx::query_as("SELECT seq, text FROM uart WHERE perf_run_id = $1 ORDER BY seq")
                .bind(out.run_id)
                .fetch_all(&pool)
                .await
                .unwrap()
        });
        assert_eq!(got, vec![(0, lines[0].clone()), (1, lines[1].clone())]);
    }

    #[test]
    fn ingest_run_with_no_uart_lines_writes_zero_uart_rows() {
        let db = ggo_db::TestDb::new();
        let out = ingest_run(db.url(), &output_for("no-uart", 1), &[], None).unwrap();
        assert_eq!(
            scalar_i64(
                db.url(),
                "SELECT COUNT(*) FROM uart WHERE perf_run_id = $1",
                Some(out.run_id)
            ),
            0
        );
    }

    // ---------------------------------------------- cross-panel round trip

    /// THE schema-fidelity test: a run written through this pane's ingest
    /// path is read back through `ggo_charts_panel`'s OWN query functions
    /// (`list_runs`/`load_run_samples`, the real read side of the charts
    /// panel, not a re-implementation here). If the two panels ever
    /// disagreed about a table, a column name or a column order, this
    /// fails.
    #[test]
    fn a_run_ingested_here_reads_back_through_the_charts_panels_queries() {
        use ggo_charts_panel::loader;

        let db = ggo_db::TestDb::new();

        let mut v: Json = serde_json::from_str(&output_for("Green Fix", 3)).unwrap();
        // A couple of the optional columns the charts read, so the
        // assertion covers the EXTRA_COLS tail of INSERT_FRAME too, not
        // just the 13 required ones.
        v["frames"]["bg_evictions"] = serde_json::json!([1, 2, 3]);
        v["frames"]["apu_underruns"] = serde_json::json!([0, 0, 4]);
        v["frames"]["spr_tiles_distinct"] = serde_json::json!([7, 8, 9]);
        let out = ingest_run(
            db.url(),
            &v.to_string(),
            &["[run] green.cart".to_string()],
            Some("carts/green.cart"),
        )
        .unwrap();

        let runs = loader::list_runs(db.url()).unwrap();
        assert_eq!(runs.len(), 1, "the charts panel's picker sees the run");
        assert_eq!(runs[0].id, out.run_id);
        assert_eq!(runs[0].cart_name, "Green Fix");
        assert_eq!(runs[0].label.as_deref(), Some("carts/green.cart"));
        assert!(!runs[0].started_at.is_empty());

        let samples = loader::load_run_samples(db.url(), out.run_id).unwrap();
        assert_eq!(samples.frames.len(), 3);
        // FRAME_COLS order: n, instrs, i_hits, i_misses, ... -- so frame 0
        // has n = 0, instrs = 1, i_hits = 1, i_misses = 1.
        assert_eq!(samples.frames[0].n, 0);
        assert_eq!(samples.frames[1].n, 1, "ORDER BY n on the read side");
        assert_eq!(samples.frames[0].instrs, 1);
        assert_eq!(samples.frames[2].instrs, 3);
        assert_eq!(
            samples.frames[0].frame_budget_cycles,
            Some(555_549),
            "the budget reference line comes off the joined run row"
        );
        assert_eq!(samples.frames[0].bg_evictions, 1);
        assert_eq!(samples.frames[2].apu_underruns, 4);
        assert_eq!(samples.frames[2].spr_tiles_distinct, 9);
        assert!(
            samples.profile.is_empty(),
            "a cart run has no function attribution -- and that is not an error"
        );
    }

    /// The other half of the round trip: real perf JSON, produced by
    /// `ggo_emu_core::perfsim::perf_json` from an actual cart run driven
    /// through [`crate::drive`], ingests cleanly and lands the right frame
    /// count. This is what pins the ingest against the EMITTER rather than
    /// against a hand-written fixture of it.
    #[test]
    fn real_perf_json_from_a_cart_run_ingests_cleanly() {
        use ggo_charts_panel::loader;

        let finished = crate::drive::tests_support::run_green_cart_briefly(5);
        let perf = finished.perf.expect("a cart that ran has a perf snapshot");
        assert!(perf.frames >= 5, "{} frames recorded", perf.frames);

        let db = ggo_db::TestDb::new();
        let out = ingest_run(db.url(), &perf.perf_json, &finished.uart, Some("green.cart")).unwrap();

        let runs = loader::list_runs(db.url()).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].cart_name, "Green Fix",
            "the perf-JSON cart identity is the cart header's own title, \
             exactly as ggo-ide's CartStepper reports it"
        );
        let samples = loader::load_run_samples(db.url(), out.run_id).unwrap();
        assert_eq!(samples.frames.len() as u64, perf.frames);
        assert!(
            samples
                .frames
                .iter()
                .all(|f| f.frame_budget_cycles.is_some()),
            "the wire model was enabled, so every frame has a budget"
        );
        assert!(
            samples.frames.iter().any(|f| f.instrs > 0),
            "the perf sim actually counted the cart's instructions"
        );
        assert!(
            !finished.uart.is_empty(),
            "the run's own diagnostics are ingested alongside the frames"
        );
    }
}
