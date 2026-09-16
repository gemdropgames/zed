//! An in-process daemon for panel tests.
//!
//! Every panel that reads a report, a fault or a device run now goes
//! through the daemon. A test must not depend on one being installed and
//! running: it would read and write the developer's own database, and an
//! older `ggo` on `PATH` fails the call outright with "unknown tool".
//!
//! [`ingesting_daemon`] answers the transport in-process, calling the same
//! `ggo_worldlib::charts::reports` functions the real daemon calls, against
//! a database url the test owns. The rows a test reads back are therefore
//! really written and really queried -- this is a different daemon, not a
//! different database layer.
//!
//! Behind the `test-support` feature, because it pulls in worldlib's `db`
//! feature (and so sqlx): the production editor build must never link one.
//! The pattern is the fork's usual one -- see `crates/ggo/.rules`.
//!
//! For a daemon whose answers are scripted rather than real, use
//! [`crate::FakeDaemon`]; this one exists for the tests that assert on
//! what the database actually did.

use std::sync::Arc;

use serde_json::{json, Value};

use ggo_worldlib::charts::reports::{diag_db, faults, ingest, perf_db};

use crate::{Client, Connect, Transport, PROTOCOL_VERSION};

/// A [`Connect`] whose client talks to an in-process daemon backed by the
/// database at `db_url`.
///
/// `faults_dir` is where fault dumps are read from; tests that never touch
/// faults can pass any path.
pub fn ingesting_connect(db_url: impl Into<String>, faults_dir: impl Into<std::path::PathBuf>) -> Connect {
    let db_url = db_url.into();
    let faults_dir = faults_dir.into();
    Arc::new(move || {
        Client::with_transport(ingesting_daemon(db_url.clone(), faults_dir.clone())).map(Arc::new)
    })
}

/// The transport itself, for a caller that wants to build the client.
pub fn ingesting_daemon(db_url: String, faults_dir: std::path::PathBuf) -> Transport {
    Arc::new(move |line: &str| {
        let request: Value = serde_json::from_str(line)?;
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str);
        let params = request.get("params").cloned().unwrap_or_default();

        let outcome = match method {
            Some("initialize") => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {"name": "in-process", "version": "test"},
            })),
            Some("tools/call") => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                call(&db_url, &faults_dir, &name, &arguments)
            }
            other => Err(format!("the in-process daemon has no {other:?}")),
        };

        let response = match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            // A tool that failed is MCP content with `isError`, not a
            // transport failure -- exactly how the real daemon reports it,
            // so a test exercises the same error path the editor sees.
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": message}],
                    "isError": true,
                }
            }),
        };
        Ok(response.to_string())
    })
}

/// Pull every dump the database has not seen into it, returning why it
/// could not when it could not -- the real daemon's `import_faults`.
fn import_faults(faults_dir: &std::path::Path, db_url: &str) -> Option<String> {
    match faults::import(faults_dir, db_url) {
        Ok(_) => None,
        Err(error) => Some(format!(
            "importing faults from {} failed: {error}",
            faults_dir.display()
        )),
    }
}

/// Every tool this daemon serves, against a real database.
fn call(db_url: &str, faults_dir: &std::path::Path, name: &str, arguments: &Value) -> Result<Value, String> {
    let run_id = || -> Result<i64, String> {
        arguments
            .get("run_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("{name} needs a run_id"))
    };
    let text_id = || -> Result<String, String> {
        arguments
            .get("run_id")
            .or_else(|| arguments.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{name} needs an id"))
    };
    let limit = |fallback: i64| {
        arguments
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(fallback)
    };
    let as_string = |e: String| e;

    let payload = match name {
        "ggo_run_index" => json!(perf_db::run_index(db_url).map_err(|e| format!("{e:#}"))?),
        "ggo_carts" => json!(perf_db::carts(db_url).map_err(|e| format!("{e:#}"))?),
        "ggo_cart_runs" => {
            let cart_id = arguments
                .get("cart_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| "ggo_cart_runs needs a cart_id".to_string())?;
            json!(perf_db::cart_runs(db_url, cart_id).map_err(|e| format!("{e:#}"))?)
        }
        "ggo_run_detail" => json!(perf_db::run_detail(db_url, run_id()?).map_err(|e| format!("{e:#}"))?),
        "ggo_run_frames" => json!(perf_db::run_frames(db_url, run_id()?).map_err(|e| format!("{e:#}"))?),
        "ggo_run_uart" => json!(perf_db::run_uart(db_url, run_id()?).map_err(|e| format!("{e:#}"))?),
        "ggo_run_profile" => json!(perf_db::run_profile(db_url, run_id()?).map_err(|e| format!("{e:#}"))?),
        "ggo_diag_runs" => json!(diag_db::list_runs(db_url, limit(50)).map_err(as_string)?),
        "ggo_diag_run_log" => json!(diag_db::run_log(db_url, &text_id()?).map_err(as_string)?),
        "ggo_diag_perf_run" => json!({
            "perf_run_id": diag_db::device_perf_run_id(db_url, &text_id()?).map_err(as_string)?,
        }),
        "ggo_faults" => {
            // The real daemon imports on the way, so this does too --
            // otherwise a test seeds dumps and reads an empty rail. The
            // failure is CARRIED, not logged: an empty list and a failed
            // import are opposite facts.
            let import_error = import_faults(faults_dir, db_url);
            // A failing list and a failing import usually share one cause
            // (the server is down); the real daemon combines them rather
            // than dropping the import's reason, so this does too.
            let rows = faults::list(db_url, limit(50)).map_err(|error| match &import_error {
                Some(note) => format!("{error} (and {note})"),
                None => error,
            })?;
            json!({"rows": rows, "import_error": import_error})
        }
        "ggo_fault" => {
            // Imports first, as the real daemon does: a dump written
            // seconds ago must be fetchable by the id a caller just read
            // off the list, without a separate step in between.
            let import_error = import_faults(faults_dir, db_url);
            json!({
                "fault": faults::load(db_url, &text_id()?).map_err(as_string)?,
                "import_error": import_error,
            })
        }
        "ggo_fault_raw_path" => json!({
            "path": faults::raw_path(faults_dir, &text_id()?).display().to_string(),
        }),
        "ggo_ingest_run" => {
            let perf_json = arguments
                .get("perf_json")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let uart: Vec<String> = arguments
                .get("uart")
                .and_then(Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let label = arguments.get("label").and_then(Value::as_str);
            let run = ingest::ingest_run(db_url, perf_json, &uart, label).map_err(as_string)?;
            json!({
                "run_id": run.run_id,
                "cart_id": run.cart_id,
                "truncated_frames": run.truncated_frames,
            })
        }
        // The audio tools run the real codec against real files, so a
        // panel test bakes what the daemon would bake.
        "ggo_audio_probe" => {
            let path = std::path::PathBuf::from(
                arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ggo_audio_probe needs a path".to_string())?,
            );
            let is_adp = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("adp"));
            let (decoded, adp) = if is_adp {
                let bytes = std::fs::read(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                let decoded = ggo_audio::decode_adp(&bytes)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                (decoded, Some(bytes))
            } else {
                (
                    ggo_audio::decode(&path).map_err(|error| format!("{error:#}"))?,
                    None,
                )
            };
            json!({
                "waveform": ggo_audio::buckets(&decoded.samples, ggo_audio::WAVEFORM_BUCKETS),
                "rate_hz": decoded.rate_hz,
                "source_channels": decoded.source_channels,
                "duration_ms": decoded.duration_ms(),
                "sample_count": decoded.samples.len(),
                "default_rate_hz": ggo_audio::default_rate(&path),
                "adp": adp,
            })
        }
        "ggo_audio_bake" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "ggo_audio_bake needs a path".to_string())?;
            let rate_hz = arguments
                .get("rate_hz")
                .and_then(Value::as_u64)
                .ok_or_else(|| "ggo_audio_bake needs a rate_hz".to_string())? as u32;
            let decoded = ggo_audio::decode(std::path::Path::new(path))
                .map_err(|error| format!("{error:#}"))?;
            json!({"adp": crate::encode_base64(&ggo_audio::bake(&decoded, rate_hz))})
        }
        "ggo_audio_write" => {
            let root = arguments
                .get("root")
                .and_then(Value::as_str)
                .ok_or_else(|| "ggo_audio_write needs a root".to_string())?;
            let rel = arguments
                .get("rel")
                .and_then(Value::as_str)
                .ok_or_else(|| "ggo_audio_write needs a rel".to_string())?;
            let blob = crate::decode_base64_field(&arguments, "adp")
                .map_err(|error| format!("{error:#}"))?;
            ggo_audio::write_adp(std::path::Path::new(root), rel, &blob)
                .map_err(|error| format!("{error:#}"))?;
            json!({"written": rel})
        }
        "ggo_audio_budget" => {
            let blob = crate::decode_base64_field(&arguments, "adp")
                .map_err(|error| format!("{error:#}"))?;
            json!({
                "region_bytes": ggo_audio::adp_region_bytes(&blob).unwrap_or(0),
                "sample_region_bytes": ggo_audio::SAMPLE_REGION_BYTES,
                "rates": ggo_audio::RATES.to_vec(),
            })
        }
        other => return Err(format!("the in-process daemon has no {other}")),
    };
    Ok(json!({"content": [{"type": "text", "text": payload.to_string()}]}))
}
