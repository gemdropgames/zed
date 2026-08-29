//! Write a finished run's perf snapshot into `~/.ggo/ggo_ide.db`.
//!
//! Ported field-for-field from `ggo-ide`'s `src/backend/ingest.rs`
//! (`parse_output`/`ingest_run`/`write_run_rows` and its `iso8601_utc`
//! helper) -- same required fields, same optional fields, same validation,
//! same caps, same tables, same column order, same `BEGIN`/`COMMIT`
//! transaction. Nothing about the schema is invented here: the input is
//! whatever `ggo_emu_core::perfsim::perf_json` produced for the run
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
//! [`parse_output`] is pure (`&str -> Result<RunBody, String>`, no db, no
//! panics on malformed input); [`ingest_run`] is the blocking db half on
//! top of it and MUST be called from a background thread -- the panel runs
//! it inside `cx.background_spawn`, the same rule
//! `ggo_charts_panel::loader` follows for its reads.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value as Json;

/// Hard cap on frames per run ("sane size": ~28 min at 60 fps).
/// `ggo-ide::backend::ingest::MAX_FRAMES` verbatim.
pub const MAX_FRAMES: usize = 100_000;
/// Hard cap on function-attribution rows per `profile`/`dprofile` section.
const MAX_PROFILE_ROWS: usize = 500_000;
/// Cart names come from a 32-byte header slot; anything longer is junk.
const MAX_CART_NAME: usize = 64;

const INSERT_CART: &str = "INSERT OR IGNORE INTO cart(name) VALUES (?1)";
const SELECT_CART: &str = "SELECT id FROM cart WHERE name = ?1";
const INSERT_RUN: &str = "INSERT INTO run(cart_id, started_at, frames, frame_budget_cycles,
                                          scanout_wire_cycles, refill_cycles, writeback_cycles,
                                          wire_wait_cycles, label)
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";
const INSERT_UART: &str = "INSERT INTO uart(run_id, seq, text) VALUES (?1, ?2, ?3)";
const INSERT_PROFILE: &str = "INSERT INTO profile(run_id, frame, caller, func, misses, evicted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const INSERT_DPROFILE: &str = "INSERT INTO dprofile(run_id, frame, caller, func, misses, evicted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const INSERT_FRAME: &str = "INSERT INTO frame(run_id, n, instrs, i_hits, i_misses, d_hits,
                                              d_misses, d_writebacks, evictions, blit_wire,
                                              miss_wire, scanout_wire, wire_total, over_budget,
                                              bg_hits, bg_misses, bg_evictions, bg_loads,
                                              fg_hits, fg_misses, fg_evictions, fg_loads,
                                              spr_hits, spr_misses, spr_evictions, spr_loads,
                                              tile_load_wire, apu_fetch_wire, apu_underruns,
                                              sc_upload, sc_oam, sc_layer, sc_audio, sc_other,
                                              peak_spr_line, bg_tiles_distinct, spr_tiles_distinct)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                                    ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26,
                                    ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37)";

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

/// Parse `output` and write it into `db_path` as a `cart`/`run`/`frame`
/// (/`uart`/`profile`/`dprofile`) row set -- the same shape the native
/// `ggo-emu`/`ggo-server` writers, `ggo-ide` and `ggo_fixture` all use,
/// and the shape `ggo_charts_panel::loader` reads.
///
/// A missing db file is created and migrated by `ggo_db::open` (the same
/// call ggo-ide makes), so the very first run a fresh checkout ingests
/// brings the database into existence.
///
/// `started_at` is stamped here, from the host clock. `uart` is the run's
/// diagnostic lines (zero rows written when empty, not an error) and
/// `label` an optional free-text run identity for the `run.label` column.
///
/// BLOCKING: opens a short-lived connection and returns once the write
/// commits. Callers on the UI thread must go through
/// `cx.background_spawn`.
pub fn ingest_run(
    db_path: &Path,
    output: &str,
    uart: &[String],
    label: Option<&str>,
) -> Result<RunId, String> {
    let body = parse_output(output)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(write_run(db_path, &body, uart, label))
}

async fn write_run(
    db_path: &Path,
    run: &RunBody,
    uart: &[String],
    label: Option<&str>,
) -> Result<RunId, String> {
    let db = ggo_db::open(db_path).await?;
    let conn = db.conn()?;
    let n_frames = run.cols[0].len();

    conn.execute(INSERT_CART, [run.cart.as_str()])
        .await
        .map_err(|e| e.to_string())?;
    let mut rows = conn
        .query(SELECT_CART, [run.cart.as_str()])
        .await
        .map_err(|e| e.to_string())?;
    let row = rows
        .next()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("cart row vanished after insert")?;
    let cart_id = *row
        .get_value(0)
        .map_err(|e| e.to_string())?
        .as_integer()
        .ok_or("cart.id not an integer")?;

    let started_at = iso8601_utc_now();
    conn.execute("BEGIN", ()).await.map_err(|e| e.to_string())?;
    let result = write_run_rows(&conn, run, cart_id, &started_at, n_frames, uart, label).await;
    match result {
        Ok(run_id) => {
            conn.execute("COMMIT", ())
                .await
                .map_err(|e| e.to_string())?;
            Ok(RunId {
                run_id,
                cart_id,
                truncated_frames: run.truncated_frames,
            })
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_run_rows(
    conn: &turso::Connection,
    run: &RunBody,
    cart_id: i64,
    started_at: &str,
    n_frames: usize,
    uart: &[String],
    label: Option<&str>,
) -> Result<i64, String> {
    conn.execute(
        INSERT_RUN,
        (
            cart_id,
            started_at,
            n_frames as i64,
            run.frame_budget_cycles,
            run.scanout_wire_cycles,
            run.refill_cycles,
            run.writeback_cycles,
            run.wire_wait_cycles,
            label,
        ),
    )
    .await
    .map_err(|e| e.to_string())?;
    let run_id = conn.last_insert_rowid();

    for i in 0..n_frames {
        // run_id + 13 required + 23 PPU/APU columns = 37 params (past the
        // tuple `IntoParams` limit, so bind via `params_from_iter`).
        let mut vals: Vec<i64> = Vec::with_capacity(1 + FRAME_COLS.len() + EXTRA_COLS.len());
        vals.push(run_id);
        for c in &run.cols {
            vals.push(c[i]);
        }
        for c in &run.extra_cols {
            vals.push(c[i]);
        }
        conn.execute(INSERT_FRAME, turso::params_from_iter(vals))
            .await
            .map_err(|e| e.to_string())?;
    }
    for (seq, line) in uart.iter().enumerate() {
        conn.execute(INSERT_UART, (run_id, seq as i64, line.as_str()))
            .await
            .map_err(|e| e.to_string())?;
    }
    for r in &run.profile {
        conn.execute(
            INSERT_PROFILE,
            (
                run_id,
                r.frame,
                r.caller.as_str(),
                r.func.as_str(),
                r.misses,
                r.evicted,
            ),
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    for r in &run.dprofile {
        conn.execute(
            INSERT_DPROFILE,
            (
                run_id,
                r.frame,
                r.caller.as_str(),
                r.func.as_str(),
                r.misses,
                r.evicted,
            ),
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ingest_run_creates_the_db_and_its_cart_and_run_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("ggo_ide.db");
        let out = ingest_run(&db_path, &output_for("demo", 3), &[], None).unwrap();
        assert_eq!(out.run_id, 1);
        assert_eq!(out.cart_id, 1);
        assert!(
            db_path.exists(),
            "a missing db (and its parent dir) is created + migrated"
        );
    }

    #[test]
    fn ingest_run_reuses_the_cart_row_across_two_runs() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        ingest_run(&db_path, &output_for("demo", 3), &[], None).unwrap();
        let out = ingest_run(&db_path, &output_for("demo", 3), &[], None).unwrap();
        assert_eq!(out.run_id, 2, "a second run gets a new run row");
        assert_eq!(out.cart_id, 1, "the same cart name reuses the cart row");
    }

    #[test]
    fn ingest_run_rejects_malformed_output_before_touching_the_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        assert!(ingest_run(&db_path, "not json at all", &[], None).is_err());
        assert!(
            !db_path.exists(),
            "a parse failure must never create the db file"
        );
    }

    #[test]
    fn ingest_run_persists_uart_lines_in_seq_order() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let lines = vec!["[run] green.cart".to_string(), "[run ended] stopped".into()];
        let out = ingest_run(&db_path, &output_for("uart-check", 1), &lines, None).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&db_path).await.unwrap();
            let conn = db.conn().unwrap();
            let mut rows = conn
                .query(
                    "SELECT seq, text FROM uart WHERE run_id = ?1 ORDER BY seq",
                    [out.run_id],
                )
                .await
                .unwrap();
            let mut got = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                got.push((
                    *row.get_value(0).unwrap().as_integer().unwrap(),
                    row.get_value(1).unwrap().as_text().unwrap().to_string(),
                ));
            }
            assert_eq!(got, vec![(0, lines[0].clone()), (1, lines[1].clone())]);
        });
    }

    #[test]
    fn ingest_run_with_no_uart_lines_writes_zero_uart_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let out = ingest_run(&db_path, &output_for("no-uart", 1), &[], None).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&db_path).await.unwrap();
            let conn = db.conn().unwrap();
            let mut rows = conn
                .query("SELECT COUNT(*) FROM uart WHERE run_id = ?1", [out.run_id])
                .await
                .unwrap();
            let row = rows.next().await.unwrap().unwrap();
            assert_eq!(row.get_value(0).unwrap(), turso::Value::Integer(0));
        });
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

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");

        let mut v: Json = serde_json::from_str(&output_for("Green Fix", 3)).unwrap();
        // A couple of the optional columns the charts read, so the
        // assertion covers the EXTRA_COLS tail of INSERT_FRAME too, not
        // just the 13 required ones.
        v["frames"]["bg_evictions"] = serde_json::json!([1, 2, 3]);
        v["frames"]["apu_underruns"] = serde_json::json!([0, 0, 4]);
        v["frames"]["spr_tiles_distinct"] = serde_json::json!([7, 8, 9]);
        let out = ingest_run(
            &db_path,
            &v.to_string(),
            &["[run] green.cart".to_string()],
            Some("carts/green.cart"),
        )
        .unwrap();

        let runs = loader::list_runs(&db_path).unwrap();
        assert_eq!(runs.len(), 1, "the charts panel's picker sees the run");
        assert_eq!(runs[0].id, out.run_id);
        assert_eq!(runs[0].cart_name, "Green Fix");
        assert_eq!(runs[0].label.as_deref(), Some("carts/green.cart"));
        assert!(!runs[0].started_at.is_empty());

        let samples = loader::load_run_samples(&db_path, out.run_id).unwrap();
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

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ggo_ide.db");
        let out = ingest_run(
            &db_path,
            &perf.perf_json,
            &finished.uart,
            Some("green.cart"),
        )
        .unwrap();

        let runs = loader::list_runs(&db_path).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].cart_name, "Green Fix",
            "the perf-JSON cart identity is the cart header's own title, \
             exactly as ggo-ide's CartStepper reports it"
        );
        let samples = loader::load_run_samples(&db_path, out.run_id).unwrap();
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
