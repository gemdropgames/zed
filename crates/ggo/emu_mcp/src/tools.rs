//! Tool definitions and execution: each MCP tool resolves a target Zed
//! session from the registry, sends one protocol line over its socket,
//! and shapes the answer into MCP content.
//!
//! Emulation is LOCK-STEP: `emu_start` boots a cart paused and returns
//! the cart's own world-state JSON; each `emu_next_frame` latches pad
//! input, runs exactly one frame, and returns the new world state (plus
//! an optional screenshot); `emu_stop` ends the run with the uart log.
//! A script, an AI, or any other caller can play the emulator.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use ggo_emu_remote::protocol::{Cmd, FlashConfig, Request, Response};
use ggo_emu_remote::registry::{self, SessionInfo};
use serde_json::{Value, json};

use crate::png::bgra_to_png;

/// One request/response over `socket`, bounded by `timeout`. The
/// production connector; tests substitute a fake.
pub type Connector = dyn Fn(&Path, &str, Duration) -> std::io::Result<String>;

pub fn socket_call(socket: &Path, line: &str, timeout: Duration) -> std::io::Result<String> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    // A wedged Zed must surface as a tool error, not hang the agent.
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}

/// Every lock-step call is bounded host-side (one frame, or a 5s boot
/// wait); a wedged Zed surfaces as an error within this.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// The MCP `tools/list` payload.
pub fn tool_list() -> Value {
    let session_props = json!({
        "session": { "type": "number", "description": "Target Zed process id from zed_sessions (optional when only one is live)" },
        "workspace": { "type": "string", "description": "Absolute project-root path (optional when the session has only one workspace)" },
    });
    let with = |extra: Value| -> Value {
        let mut props = session_props.as_object().expect("json! object literal").clone();
        if let Some(map) = extra.as_object() {
            props.extend(map.clone());
        }
        json!({ "type": "object", "properties": props })
    };
    json!({ "tools": [
        { "name": "zed_sessions",
          "description": "List running Zed sessions hosting a GGO emulator panel: pid, workspaces, and what each panel is doing.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "emu_status", "description": "What the target session's emulator panels are doing.", "inputSchema": with(json!({})) },
        { "name": "emu_start",
          "description": "Boot a cart in the Zed emulator panel and pause at the first frame boundary (lock-step). Pack first: emd pack-ggo [--world <stem>]. Returns the initial world-state JSON — worlds that declare `InspectWorld = {}` under [resources] serialize every entity's registered components; others return null. Drive with emu_next_frame; end with emu_stop.",
          "inputSchema": with(json!({ "cart": { "type": "string", "description": "Worktree-relative packed cart path, e.g. wilds.ggo" } })) },
        { "name": "emu_next_frame",
          "description": "Latch the held pad buttons (names: z x a s up down left right q w e r t y u i enter select; empty/omitted releases all), run exactly ONE frame, and return the new world-state JSON. Set screenshot=true to also get the presented frame as PNG. Call repeatedly to play.",
          "inputSchema": with(json!({
              "buttons": { "type": "array", "items": { "type": "string" } },
              "screenshot": { "type": "boolean" }
          })) },
        { "name": "emu_stop", "description": "End the lock-step run; returns the cart's uart log.", "inputSchema": with(json!({})) },
        { "name": "hw_flash",
          "description": "Flash a world to the GemdropGo board and run it (build, program, boot-verify over UART, then a timed gameplay telemetry capture). Flashing is intensive (occupies the board; ~20 min with rebuild_gateware). Confirm with the user before invoking. Always pass an explicit `world` stem (e.g. worlds/chase_cam): omitting it flashes whichever world the panel last remembered or has open, which is often not the one you mean. Every other knob has a default (the default set: rebuild_gateware=false, tty=first serial port found, baud=115200, collect_seconds=120, telemetry=false); pass only what you mean to change. Returns as soon as the flash STARTS, with `config` = the effective configuration, defaults filled in — poll hw_flash_status, or block on hw_flash_wait. Even a start that errors opens the hardware tab in the user's Zed.",
          "inputSchema": with(json!({
              "world": { "type": "string", "description": "World stem to bake in as the boot world, e.g. worlds/chase_cam" },
              "rebuild_gateware": { "type": "boolean", "description": "Re-run place-and-route (~20 min) instead of reusing the cached bitstream; only needed after a gateware change (default false)" },
              "tty": { "type": "string", "description": "Serial device, e.g. /dev/ttyUSB0 (default: the first port the panel's scan found)" },
              "baud": { "type": "number", "description": "UART baud (default 115200)" },
              "collect_seconds": { "type": "number", "description": "How long the post-boot gameplay telemetry capture runs before the flash ends on its own (default 120)" },
              "telemetry": { "type": "boolean", "description": "Force GemOS's telemetry feature on in the firmware build (default false; firmware already defaults it on)" }
          })) },
        { "name": "hw_flash_status",
          "description": "Non-blocking snapshot of the flash in flight (else the last one): {active, what, phase, detail, elapsed_s, phases[{title,state,elapsed_s,detail}], diag_steps[{index,status}], verdict, failure, diag_run_id, perf_run_id, transcript, console_tail[]}. Poll it while a flash runs: `phases` is the whole timeline including phases still pending, `detail` is what the running phase is doing right now (e.g. which boot stage is up and the next stage's time budget), `console_tail` the newest output lines and `transcript` the full log file. verdict is null while running, true=PASS, false=FAIL (then `failure` says why); perf_run_id is the run number to hand to fetch_ggo_report, present only once a passed run's report has landed.",
          "inputSchema": with(json!({})) },
        { "name": "hw_flash_wait",
          "description": "Block until the flash in flight reaches a verdict, then return the same payload hw_flash_status returns (polling every 2s under the hood). Call it right after hw_flash; on its own it reports the LAST flash's verdict immediately. NOTE: this bridge serves ONE call at a time, so a long wait blocks every other tool here (including hw_flash_status) until it returns, and your client may time out first — nothing is lost if it does: the flash runs inside Zed, its status is per-run and persists, so call hw_flash_wait again to resume waiting on the same flash. Prefer a timeout_s you are willing to sit through (say 300) and re-call, over one long 1800s wait.",
          "inputSchema": with(json!({
              "timeout_s": { "type": "number", "description": "Give up after this many seconds; must be > 0 (omit for the default 1800)" }
          })) },
        { "name": "list_ggo_reports",
          "description": "Perf runs (emulator and board) in ~/.ggo/ggo_ide.db, newest first: run id, started_at, cart, label, and the ggo-diag log path for board runs. Reads the database directly — no Zed session needed.",
          "inputSchema": json!({
              "type": "object",
              "properties": { "limit": { "type": "number", "description": "Newest N runs (default 20)" } },
          }) },
        { "name": "fetch_ggo_report",
          "description": "Paste-ready summary of one perf run (emulator or board) from ~/.ggo/ggo_ide.db: cart, frame budget, wire/cache aggregates, over-budget frames, and the ggo-diag log path when the run has one. Takes the run number hw_flash_wait/hw_flash_status report as perf_run_id, or one from list_ggo_reports. Reads the database directly — no Zed session needed.",
          "inputSchema": json!({
              "type": "object",
              "properties": { "run": { "type": "number", "description": "Perf run id, e.g. perf_run_id from hw_flash_wait" } },
              "required": ["run"],
          }) },
        { "name": "open_ggo_report",
          "description": "Open the Reports tab in the user's Zed on one perf run (the same page a passed flash lands on).",
          "inputSchema": with(json!({ "run": { "type": "number", "description": "Perf run id from list_ggo_reports" } })) },
        { "name": "close_ggo_report",
          "description": "Close the Reports tab in the user's Zed. With `run`, only if that is the run it shows. Returns {closed: false} when no tab is open.",
          "inputSchema": with(json!({ "run": { "type": "number", "description": "Only close if the tab shows this run (optional)" } })) },
    ] })
}

/// Pick the target session: explicit pid; else the one advertising
/// `workspace`; else the only live one.
pub fn resolve_session(
    sessions: &[SessionInfo],
    pid: Option<u32>,
    workspace: Option<&str>,
) -> Result<SessionInfo, String> {
    if let Some(pid) = pid {
        return sessions
            .iter()
            .find(|s| s.pid == pid)
            .cloned()
            .ok_or_else(|| format!("no live zed session with pid {pid}; run zed_sessions"));
    }
    if let Some(workspace) = workspace {
        let hosting: Vec<&SessionInfo> =
            sessions.iter().filter(|s| s.workspaces.iter().any(|w| w == workspace)).collect();
        if let [only] = hosting[..] {
            return Ok(only.clone());
        }
        // No unique host: fall through to the pid-free rules so a fresh
        // session whose advertisement predates the workspace still works
        // when it is the only one live.
    }
    match sessions {
        [only] => Ok(only.clone()),
        [] => Err("no live zed session found — is Zed running with a GGO project open?".to_string()),
        many => Err(format!(
            "multiple zed sessions live (pids {:?}) — pass `session`",
            many.iter().map(|s| s.pid).collect::<Vec<_>>()
        )),
    }
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key)?.as_u64().map(|n| n as u32)
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(str::to_string)
}

/// A whole-number argument, tolerating the `12.0` an LLM client sometimes
/// sends where the schema says a count or an id.
fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    let v = args.get(key)?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// Execute one MCP tool call. Returns (content, is_error).
pub fn call_tool(name: &str, args: &Value, dir: &Path, connect: &Connector) -> (Vec<Value>, bool) {
    match call_tool_inner(name, args, dir, connect) {
        Ok(content) => (content, false),
        Err(e) => (vec![json!({ "type": "text", "text": e })], true),
    }
}

fn call_tool_inner(
    name: &str,
    args: &Value,
    dir: &Path,
    connect: &Connector,
) -> Result<Vec<Value>, String> {
    let sessions = registry::list(dir);
    if name == "zed_sessions" {
        let mut rows = Vec::new();
        for s in &sessions {
            let status = send(&s.socket, Cmd::Status, CALL_TIMEOUT, connect)
                .map(|d| d.to_string())
                .unwrap_or_else(|e| format!("(unreachable: {e})"));
            rows.push(format!("pid {} workspaces {:?} status {status}", s.pid, s.workspaces));
        }
        let text = if rows.is_empty() { "no live zed sessions".to_string() } else { rows.join("\n") };
        return Ok(vec![json!({ "type": "text", "text": text })]);
    }

    if name == "list_ggo_reports" {
        let limit = arg_i64(args, "limit").filter(|n| *n > 0).unwrap_or(20) as usize;
        return list_reports(&ggo_dir()?, limit);
    }
    if name == "fetch_ggo_report" {
        let run = arg_i64(args, "run").ok_or("missing required argument: run (a perf run id)")?;
        return fetch_report(&ggo_dir()?, run);
    }

    let workspace = arg_str(args, "workspace");
    let session = resolve_session(&sessions, arg_u32(args, "session"), workspace.as_deref())?;
    match name {
        "emu_status" => {
            let data = send(&session.socket, Cmd::Status, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "emu_start" => {
            let cart = arg_str(args, "cart").ok_or("missing required argument: cart")?;
            let data = send(&session.socket, Cmd::Start { workspace, cart }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "emu_next_frame" => {
            let buttons = args
                .get("buttons")
                .and_then(|b| b.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let screenshot = args.get("screenshot").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut data = send(
                &session.socket,
                Cmd::NextFrame { workspace, buttons, screenshot },
                CALL_TIMEOUT,
                connect,
            )?;
            let shot = data.as_object_mut().and_then(|o| o.remove("screenshot"));
            let mut content = vec![json!({ "type": "text", "text": data.to_string() })];
            if let Some(shot) = shot {
                use base64::Engine as _;
                let bgra = base64::engine::general_purpose::STANDARD
                    .decode(shot["bgra_base64"].as_str().unwrap_or_default())
                    .map_err(|e| e.to_string())?;
                let (w, h) = (
                    shot["width"].as_u64().unwrap_or(0) as u32,
                    shot["height"].as_u64().unwrap_or(0) as u32,
                );
                let png = bgra_to_png(w, h, &bgra)?;
                content.push(json!({
                    "type": "image",
                    "mimeType": "image/png",
                    "data": base64::engine::general_purpose::STANDARD.encode(&png),
                }));
            }
            Ok(content)
        }
        "emu_stop" => {
            let data = send(&session.socket, Cmd::Stop { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_flash" => {
            let config = FlashConfig {
                world: arg_str(args, "world"),
                rebuild_gateware: args.get("rebuild_gateware").and_then(Value::as_bool).unwrap_or(false),
                tty: arg_str(args, "tty"),
                baud: arg_i64(args, "baud").map(|n| n as u32),
                collect_seconds: arg_i64(args, "collect_seconds").map(|n| n as u64),
                telemetry: args.get("telemetry").and_then(Value::as_bool).unwrap_or(false),
            };
            let data = send(&session.socket, Cmd::FlashWorld { workspace, config }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_flash_status" => {
            let data = send(&session.socket, Cmd::FlashStatus { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "open_ggo_report" => {
            let run = arg_i64(args, "run").ok_or("missing required argument: run (a perf run id)")?;
            // Checked here so a bad id is an error, not a Reports tab
            // that silently lands on the runs list.
            run_detail_or_missing(&ggo_dir()?, run)?;
            let data = send(&session.socket, Cmd::OpenReport { workspace, run }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "close_ggo_report" => {
            let run = arg_i64(args, "run");
            let data = send(&session.socket, Cmd::CloseReport { workspace, run }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_flash_wait" => {
            // A `timeout_s` of 0 (or negative) is a caller mistake, not a
            // request for the default: silently turning it into
            // `DEFAULT_FLASH_TIMEOUT_S` would block this single-flight
            // bridge for half an hour on an argument that asked for the
            // opposite. Omitting it is what asks for the default.
            let timeout = match arg_i64(args, "timeout_s") {
                Some(s) if s <= 0 => return Err("timeout_s must be > 0".to_string()),
                Some(s) => s as u64,
                None => DEFAULT_FLASH_TIMEOUT_S,
            };
            flash_wait(
                &session.socket,
                workspace,
                Duration::from_secs(timeout),
                FLASH_POLL_INTERVAL,
                PERF_ID_GRACE,
                connect,
            )
        }
        other => Err(format!("unknown tool {other:?}")),
    }
}

/// How long a flash may run before `hw_flash_wait` gives up: a gateware
/// rebuild is ~20 minutes, so half an hour is "something is wrong", not
/// "still going".
const DEFAULT_FLASH_TIMEOUT_S: u64 = 1800;
/// Gap between `FlashStatus` polls. Each poll is its own short socket
/// call -- a flash outlives any socket read timeout, so waiting is a loop
/// of cheap questions, never one long blocking read.
const FLASH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Once a run PASSES, the Zed panel still has a database hop to make
/// before it can name the cloned perf run. Waiting this long for it (in
/// `poll` steps) is what lets `hw_flash_wait`'s answer carry the id the
/// caller then hands to `fetch_ggo_report`, instead of a null the agent has to
/// go poll for itself.
const PERF_ID_GRACE: Duration = Duration::from_secs(10);

/// Poll `FlashStatus` until the run reaches a verdict (or `timeout`),
/// then return the final payload as text content. `poll` and `grace` are
/// parameters so tests don't sleep (production passes
/// [`FLASH_POLL_INTERVAL`] and [`PERF_ID_GRACE`]).
fn flash_wait(
    socket: &Path,
    workspace: Option<String>,
    timeout: Duration,
    poll: Duration,
    grace: Duration,
    connect: &Connector,
) -> Result<Vec<Value>, String> {
    let started = std::time::Instant::now();
    let mut last_phase: Option<String> = None;
    let mut verdict_at: Option<std::time::Instant> = None;
    loop {
        let data = send(
            socket,
            Cmd::FlashStatus { workspace: workspace.clone() },
            CALL_TIMEOUT,
            connect,
        )?;
        if let Some(phase) = data.get("phase").and_then(Value::as_str) {
            last_phase = Some(phase.to_string());
        }
        if let Some(verdict) = data.get("verdict").unwrap_or(&Value::Null).as_bool() {
            // A FAIL never gets a report id, and a PASS that already has
            // one is done; only a PASS still waiting on the clone lingers
            // -- and never past the caller's own timeout, since a finished
            // run's payload beats an error about a run that did finish.
            let waiting_for_id = verdict && data.get("perf_run_id").is_none_or(Value::is_null);
            let seen_at = verdict_at.get_or_insert_with(std::time::Instant::now);
            if !waiting_for_id || seen_at.elapsed() >= grace || started.elapsed() >= timeout {
                return Ok(vec![json!({ "type": "text", "text": data.to_string() })]);
            }
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "flash still running after {}s (last phase: {}) — check the hardware tab in Zed, \
                 or poll hw_flash_status",
                timeout.as_secs(),
                last_phase.as_deref().unwrap_or("unknown")
            ));
        }
        std::thread::sleep(poll);
    }
}

/// `~/.ggo` -- the same directory `ggo_common::default_db_path` resolves
/// (duplicated as two lines rather than pulling a gpui-shaped crate into
/// this bridge binary).
fn ggo_dir() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("cannot find your home directory (neither HOME nor USERPROFILE is set)")?;
    Ok(std::path::PathBuf::from(home).join(".ggo"))
}

/// Every perf run in `<db_dir>/ggo_ide.db`, newest first, one line each,
/// with the ggo-diag log path beside the board runs that have one.
fn list_reports(db_dir: &Path, limit: usize) -> Result<Vec<Value>, String> {
    use ggo_worldlib::charts::reports::{diag_db, perf_db};

    let ide_db = db_dir.join("ggo_ide.db");
    // Same best-effort clone as `fetch_report`: board runs reach this
    // database only by being copied across, and a clone failure must not
    // hide the emulator runs already here.
    if let Err(e) = diag_db::clone_runs(&db_dir.join("diag.db"), &ide_db) {
        eprintln!("list_ggo_reports: could not clone every device run: {e}");
    }
    if !ide_db.exists() {
        return Ok(vec![json!({ "type": "text", "text": format!("no runs yet ({} does not exist)", ide_db.display()) })]);
    }
    let mut rows = Vec::new();
    for cart in perf_db::carts(&ide_db).map_err(|e| format!("reading carts: {e:#}"))? {
        for run in perf_db::cart_runs(&ide_db, cart.id)
            .map_err(|e| format!("reading runs of cart {}: {e:#}", cart.name))?
        {
            rows.push((run.started_at, run.id, cart.name.clone(), run.label, run.frames));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let logs_dir = db_dir.join("diag").join("logs");
    let lines: Vec<String> = rows
        .iter()
        .take(limit)
        .map(|(started_at, id, cart, label, frames)| {
            let log = ggo_emu_remote::diag_log_path(&logs_dir, started_at)
                .map(|p| format!("  log={}", p.display()))
                .unwrap_or_default();
            format!(
                "run {id}  {started_at}  {cart}  label={}  frames={frames}{log}",
                label.as_deref().unwrap_or("-")
            )
        })
        .collect();
    let text = if lines.is_empty() { "no runs yet".to_string() } else { lines.join("\n") };
    Ok(vec![json!({ "type": "text", "text": text })])
}

/// One perf run's paste-ready summary out of `<db_dir>/ggo_ide.db`.
/// `db_dir` is a parameter so tests can point at a temp directory;
/// production passes `~/.ggo`.
fn fetch_report(db_dir: &Path, run: i64) -> Result<Vec<Value>, String> {
    use ggo_worldlib::charts::reports::perf_db;

    let ide_db = db_dir.join("ggo_ide.db");
    let detail = run_detail_or_missing(db_dir, run)?;
    let frames =
        perf_db::run_frames(&ide_db, run).map_err(|e| format!("reading run {run} frames: {e:#}"))?;
    let mut text = perf_db::run_handoff_text(&detail, &frames);
    let log = ggo_emu_remote::diag_log_path(&db_dir.join("diag").join("logs"), &detail.started_at);
    text.push_str(&format!(
        "\nggo_diag_log: {}\n",
        log.map(|p| p.display().to_string()).unwrap_or_else(|| "- (emulator run, or log pruned)".to_string())
    ));
    Ok(vec![json!({ "type": "text", "text": text })])
}

/// The run's row, or the "no run" error naming both databases.
fn run_detail_or_missing(
    db_dir: &Path,
    run: i64,
) -> Result<ggo_worldlib::charts::reports::perf_db::RunDetail, String> {
    use ggo_worldlib::charts::reports::{diag_db, perf_db};

    let ide_db = db_dir.join("ggo_ide.db");
    let diag_db_path = db_dir.join("diag.db");
    // Best effort, unconditionally: a board run only reaches the reports
    // database once ggo-diag's rows are cloned across, and the panel's own
    // clone may not have run since -- but NO clone failure may fail this
    // tool. Every reason it can fail (no diag.db at all, on a machine that
    // has never run the device tooling; a diag.db a migration behind this
    // build, which is ggo-diag's own file to repair) leaves the run the
    // caller asked for exactly as readable as it already was -- and most
    // runs asked about are emulator runs the clone has nothing to say
    // about. The error is kept only as context for the "no run" case
    // below, where it may well be WHY the run is missing.
    let clone_error = diag_db::clone_runs(&diag_db_path, &ide_db).err();
    // Checked rather than left to the query: opening a missing database
    // CREATES it, and a stray empty ~/.ggo/ggo_ide.db is worse than the
    // error the caller gets either way.
    let missing = || match &clone_error {
        Some(e) => format!(
            "no run {run} in {} (and cloning device runs from {} failed: {e})",
            ide_db.display(),
            diag_db_path.display()
        ),
        None => format!("no run {run} in {}", ide_db.display()),
    };
    if !ide_db.exists() {
        return Err(missing());
    }
    perf_db::run_detail(&ide_db, run)
        .map_err(|e| format!("reading run {run}: {e:#}"))?
        .ok_or_else(missing)
}

/// Shape a script report into MCP content: a text summary (frames + uart),
/// then each captured frame as a labeled PNG image.
fn send(socket: &Path, cmd: Cmd, timeout: Duration, connect: &Connector) -> Result<Value, String> {
    let req = Request { id: 1, cmd };
    let line = serde_json::to_string(&req).expect("Request serializes");
    let reply = connect(socket, &line, timeout).map_err(|e| format!("zed session unreachable: {e}"))?;
    let resp: Response =
        serde_json::from_str(reply.trim()).map_err(|e| format!("bad host reply: {e}"))?;
    if resp.ok {
        Ok(resp.data.unwrap_or(Value::Null))
    } else {
        Err(resp.error.unwrap_or_else(|| "unknown host error".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_session(dir: &Path, pid: u32) -> SessionInfo {
        let info = SessionInfo {
            pid,
            socket: PathBuf::from(format!("/fake/{pid}.sock")),
            workspaces: vec!["/proj".to_string()],
        };
        registry::publish(dir, &info).unwrap();
        info
    }

    #[test]
    fn resolve_session_by_pid_single_and_ambiguous() {
        let a = SessionInfo { pid: 1, socket: "/a".into(), workspaces: vec![] };
        let b = SessionInfo { pid: 2, socket: "/b".into(), workspaces: vec![] };
        assert_eq!(resolve_session(&[a.clone(), b.clone()], Some(2), None).unwrap().pid, 2);
        assert_eq!(resolve_session(std::slice::from_ref(&a), None, None).unwrap().pid, 1);
        assert!(resolve_session(&[a, b], None, None).unwrap_err().contains("multiple"));
        assert!(resolve_session(&[], None, None).unwrap_err().contains("no live"));
    }

    #[test]
    fn resolve_session_picks_the_session_hosting_the_workspace() {
        let a = SessionInfo { pid: 1, socket: "/a".into(), workspaces: vec!["/proj/a".to_string()] };
        let b = SessionInfo { pid: 2, socket: "/b".into(), workspaces: vec!["/proj/b".to_string()] };
        let sessions = [a, b];
        assert_eq!(resolve_session(&sessions, None, Some("/proj/b")).unwrap().pid, 2);
        assert!(resolve_session(&sessions, None, Some("/proj/c")).unwrap_err().contains("multiple"));
    }

    #[test]
    fn emu_start_and_next_frame_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            if line.contains(r#""cmd":"start""#) {
                assert!(line.contains(r#""cart":"wilds.ggo""#), "{line}");
                Ok(r#"{"id":1,"ok":true,"data":{"started":true,"frame":2,"world":{"entities":[]}}}"#.to_string())
            } else {
                assert!(line.contains(r#""cmd":"next_frame""#), "{line}");
                assert!(line.contains(r#""buttons":["right"]"#), "{line}");
                Ok(r#"{"id":1,"ok":true,"data":{"frame":3,"world":{"entities":[{"id":5,"components":{}}]}}}"#.to_string())
            }
        };
        let (content, is_err) =
            call_tool("emu_start", &json!({"cart": "wilds.ggo"}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""frame":2"#));

        let (content, is_err) =
            call_tool("emu_next_frame", &json!({"buttons": ["right"]}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""id":5"#));
    }

    #[test]
    fn next_frame_screenshot_becomes_png_image_content() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]); // red px
        let reply = format!(
            r#"{{"id":1,"ok":true,"data":{{"frame":9,"world":null,"screenshot":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}}}"#
        );
        let connect = move |_: &Path, _: &str, _: Duration| -> std::io::Result<String> { Ok(reply.clone()) };
        let (content, is_err) =
            call_tool("emu_next_frame", &json!({"screenshot": true}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert_eq!(content[1]["type"], "image");
        let png = base64::engine::general_purpose::STANDARD
            .decode(content[1]["data"].as_str().unwrap())
            .unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn host_error_becomes_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            Ok(r#"{"id":1,"ok":false,"error":"no cart at /proj/nope.ggo"}"#.to_string())
        };
        let (content, is_err) =
            call_tool("emu_start", &json!({"cart": "nope.ggo"}), dir.path(), &connect);
        assert!(is_err);
        assert_eq!(content[0]["text"], "no cart at /proj/nope.ggo");
    }

    #[test]
    fn tool_list_names_exactly_the_lockstep_surface() {
        let list = tool_list();
        let names: Vec<&str> =
            list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "zed_sessions",
                "emu_status",
                "emu_start",
                "emu_next_frame",
                "emu_stop",
                "hw_flash",
                "hw_flash_status",
                "hw_flash_wait",
                "list_ggo_reports",
                "fetch_ggo_report",
                "open_ggo_report",
                "close_ggo_report",
            ]
        );
    }

    /// The user directive behind the tool: an agent must not occupy the
    /// board without asking, so the warning has to be in the text the
    /// agent actually reads.
    #[test]
    fn hw_flash_description_warns_to_confirm_with_the_user() {
        let list = tool_list();
        let flash = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "hw_flash")
            .expect("hw_flash is listed");
        let text = flash["description"].as_str().unwrap();
        assert!(
            text.contains(
                "Flashing is intensive (occupies the board; ~20 min with rebuild_gateware). \
                 Confirm with the user before invoking."
            ),
            "{text}"
        );
        assert!(flash["inputSchema"]["properties"]["world"].is_object(), "{flash}");
        assert!(flash["inputSchema"]["properties"]["rebuild_gateware"].is_object(), "{flash}");
    }

    /// The bridge serves one call at a time, so a long wait starves every
    /// other tool -- an agent can only budget for that if the tool says so.
    #[test]
    fn hw_flash_wait_description_discloses_that_it_blocks_the_bridge() {
        let list = tool_list();
        let text = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "hw_flash_wait")
            .expect("hw_flash_wait is listed")["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains("ONE call at a time"), "{text}");
        assert!(text.contains("blocks every other tool"), "{text}");
        assert!(text.contains("call hw_flash_wait again"), "{text}");
    }

    #[test]
    fn hw_flash_sends_flash_world_and_returns_the_started_reply() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"flash_world""#), "{line}");
            assert!(line.contains(r#""world":"worlds/chase_cam""#), "{line}");
            assert!(line.contains(r#""rebuild_gateware":true"#), "{line}");
            assert!(line.contains(r#""collect_seconds":30"#), "{line}");
            assert!(line.contains(r#""tty":null"#), "an unset knob travels as null: {line}");
            Ok(r#"{"id":1,"ok":true,"data":{"started":true,"config":{"world":"worlds/chase_cam","rebuild_gateware":true,"tty":"/dev/ttyUSB0","baud":115200,"collect_seconds":30,"telemetry":false}}}"#.to_string())
        };
        let (content, is_err) = call_tool(
            "hw_flash",
            &json!({"world": "worlds/chase_cam", "rebuild_gateware": true, "collect_seconds": 30}),
            dir.path(),
            &connect,
        );
        assert!(!is_err, "{content:?}");
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains(r#""started":true"#), "{text}");
        assert!(text.contains(r#""tty":"/dev/ttyUSB0""#), "the effective config comes back: {text}");
    }

    #[test]
    fn hw_flash_status_returns_the_payload_json() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"flash_status""#), "{line}");
            Ok(r#"{"id":1,"ok":true,"data":{"active":true,"phase":"Boot verify (UART)","verdict":null,"diag_run_id":"r1","perf_run_id":null}}"#.to_string())
        };
        let (content, is_err) = call_tool("hw_flash_status", &json!({}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains(r#""phase":"Boot verify (UART)""#), "{text}");
    }

    /// Each poll is its own short socket call (never one long blocking
    /// read), and the loop ends on the first verdict.
    #[test]
    fn hw_flash_wait_polls_until_a_verdict_lands() {
        let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let seen = polls.clone();
        let connect = move |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"flash_status""#), "{line}");
            seen.set(seen.get() + 1);
            Ok(match seen.get() {
                1 => r#"{"id":1,"ok":true,"data":{"active":true,"phase":"Build","verdict":null,"diag_run_id":null,"perf_run_id":null}}"#,
                2 => r#"{"id":1,"ok":true,"data":{"active":true,"phase":"Boot verify (UART)","verdict":null,"diag_run_id":"r7","perf_run_id":null}}"#,
                _ => r#"{"id":1,"ok":true,"data":{"active":false,"phase":"Done","verdict":true,"diag_run_id":"r7","perf_run_id":12}}"#,
            }
            .to_string())
        };
        let content = flash_wait(
            Path::new("/fake.sock"),
            None,
            Duration::from_secs(1800),
            Duration::ZERO,
            Duration::from_secs(60),
            &connect,
        )
        .unwrap();
        assert_eq!(polls.get(), 3);
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains(r#""verdict":true"#), "{text}");
        assert!(text.contains(r#""perf_run_id":12"#), "{text}");
    }

    #[test]
    fn hw_flash_wait_timeout_is_an_error_naming_the_last_phase() {
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            Ok(r#"{"id":1,"ok":true,"data":{"active":true,"phase":"Place and route","verdict":null,"diag_run_id":null,"perf_run_id":null}}"#.to_string())
        };
        let err = flash_wait(
            Path::new("/fake.sock"),
            None,
            Duration::ZERO,
            Duration::ZERO,
            Duration::from_secs(60),
            &connect,
        )
        .unwrap_err();
        assert!(err.contains("Place and route"), "{err}");
    }

    #[test]
    fn hw_flash_wait_dispatches_with_the_callers_timeout() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"flash_status""#), "{line}");
            Ok(r#"{"id":1,"ok":true,"data":{"active":false,"phase":"Boot verify (UART)","verdict":false,"diag_run_id":"r9","perf_run_id":null}}"#.to_string())
        };
        // `60.0`, not `60`: MCP clients routinely send whole numbers as floats.
        let (content, is_err) =
            call_tool("hw_flash_wait", &json!({"timeout_s": 60.0}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""verdict":false"#));
    }

    /// `timeout_s: 0` asked for no wait at all; answering it with a
    /// half-hour block on a bridge that serves one call at a time is the
    /// worst possible reading of it. Omitting the argument is what asks
    /// for [`DEFAULT_FLASH_TIMEOUT_S`].
    #[test]
    fn hw_flash_wait_rejects_a_non_positive_timeout() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            Ok(r#"{"id":1,"ok":true,"data":{"active":false,"phase":"Done","verdict":true,"diag_run_id":"r1","perf_run_id":3}}"#.to_string())
        };
        for bad in [json!({"timeout_s": 0}), json!({"timeout_s": -5}), json!({"timeout_s": 0.0})] {
            let (content, is_err) = call_tool("hw_flash_wait", &bad, dir.path(), &connect);
            assert!(is_err, "{bad} must be rejected: {content:?}");
            assert_eq!(content[0]["text"], "timeout_s must be > 0", "{bad}");
        }

        // Omitted is what asks for the default, and still runs normally.
        let (content, is_err) = call_tool("hw_flash_wait", &json!({}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""verdict":true"#));
        assert_eq!(DEFAULT_FLASH_TIMEOUT_S, 1800);
    }

    /// A run passes before the panel's post-PASS database hop resolves the
    /// report id; waiting a beat for it is the whole point of the tool.
    #[test]
    fn hw_flash_wait_gives_a_passed_run_a_beat_to_publish_its_report_id() {
        let seen = std::cell::Cell::new(0u32);
        let connect = move |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            seen.set(seen.get() + 1);
            Ok(if seen.get() < 3 {
                r#"{"id":1,"ok":true,"data":{"active":false,"phase":"Done","verdict":true,"diag_run_id":"r7","perf_run_id":null}}"#
            } else {
                r#"{"id":1,"ok":true,"data":{"active":false,"phase":"Done","verdict":true,"diag_run_id":"r7","perf_run_id":30}}"#
            }
            .to_string())
        };
        let content = flash_wait(
            Path::new("/fake.sock"),
            None,
            Duration::from_secs(1800),
            Duration::ZERO,
            Duration::from_secs(60),
            &connect,
        )
        .unwrap();
        assert!(content[0]["text"].as_str().unwrap().contains(r#""perf_run_id":30"#));

        // A FAIL never gets a report id: return it immediately, however
        // much grace the caller allowed.
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            Ok(r#"{"id":1,"ok":true,"data":{"active":false,"phase":"Boot verify (UART)","verdict":false,"diag_run_id":"r8","perf_run_id":null}}"#.to_string())
        };
        let content = flash_wait(
            Path::new("/fake.sock"),
            None,
            Duration::from_secs(1800),
            Duration::ZERO,
            Duration::from_secs(60),
            &connect,
        )
        .unwrap();
        assert!(content[0]["text"].as_str().unwrap().contains(r#""verdict":false"#));
    }

    /// The grace is bounded, not a second wait: a PASS whose report id
    /// never lands comes back as the payload it has (null id), not an
    /// error and not a hang.
    #[test]
    fn hw_flash_wait_returns_the_null_id_payload_once_the_grace_runs_out() {
        let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let seen = polls.clone();
        let connect = move |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            seen.set(seen.get() + 1);
            Ok(r#"{"id":1,"ok":true,"data":{"active":false,"phase":"Done","verdict":true,"diag_run_id":"r7","perf_run_id":null}}"#.to_string())
        };
        let content = flash_wait(
            Path::new("/fake.sock"),
            None,
            Duration::from_secs(1800),
            Duration::ZERO,
            Duration::ZERO,
            &connect,
        )
        .unwrap();
        assert_eq!(polls.get(), 1, "an exhausted grace must not keep polling");
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains(r#""verdict":true"#), "{text}");
        assert!(text.contains(r#""perf_run_id":null"#), "{text}");
    }

    /// A temp `ggo_ide.db` through the real schema (`ggo_db::open`
    /// migrates), seeded with the same INSERT shape ggo-worldlib's
    /// `perf_db` tests use: one cart, one run, two frames.
    fn seeded_db_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'wilds')", ()).await.unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, frame_budget_cycles,
                                  scanout_wire_cycles, refill_cycles, writeback_cycles,
                                  wire_wait_cycles, label)
                 VALUES (7, 1, '2026-08-31T00:00:00Z', 2, 555549, 164400, 100, 65, 0, 'board')",
                (),
            )
            .await
            .unwrap();
            for (n, wire_total, cyc) in [(0i64, 100_000i64, 700_000i64), (1, 600_000, 812_345)] {
                conn.execute(
                    "INSERT INTO frame (run_id, n, wire_total, i_misses, d_misses, over_budget,
                                        apu_underruns, cyc)
                     VALUES (7, ?1, ?2, 5, 2, 0, 0, ?3)",
                    (n, wire_total, cyc),
                )
                .await
                .unwrap();
            }
        });
        dir
    }

    #[test]
    fn fetch_report_renders_the_run_handoff_text_and_its_log_path() {
        let dir = seeded_db_dir();
        let content = fetch_report(dir.path(), 7).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("GemdropGo perf run #7"), "{text}");
        assert!(text.contains("wilds"), "{text}");
        assert!(text.contains("frame_rows_loaded: 2"), "{text}");
        assert!(text.contains("ggo_diag_log: -"), "{text}");

        let logs = dir.path().join("diag").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log = logs.join("main_abc1234_2026-08-31T00:00:00Z.log");
        std::fs::write(&log, "").unwrap();
        let content = fetch_report(dir.path(), 7).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("ggo_diag_log: {}", log.display())), "{text}");
    }

    #[test]
    fn list_reports_lists_runs_newest_first_with_their_logs() {
        let dir = seeded_db_dir();
        let logs = dir.path().join("diag").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log = logs.join("main_abc1234_2026-08-31T00:00:00Z.log");
        std::fs::write(&log, "").unwrap();
        let content = list_reports(dir.path(), 20).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.starts_with("run 7  2026-08-31T00:00:00Z  wilds  label=board  frames=2  log="), "{text}");
        assert!(text.ends_with(&log.display().to_string()), "{text}");
        assert_eq!(list_reports(dir.path(), 0).unwrap()[0]["text"], "no runs yet");
    }

    #[test]
    fn list_reports_with_no_database_says_so_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let text = list_reports(dir.path(), 20).unwrap()[0]["text"].as_str().unwrap().to_string();
        assert!(text.starts_with("no runs yet"), "{text}");
        assert!(!dir.path().join("ggo_ide.db").exists(), "must not create an empty db");
    }

    #[test]
    fn open_report_rejects_an_unknown_run_before_touching_zed() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            panic!("no socket call for a run that does not exist")
        };
        let (content, is_err) = call_tool("open_ggo_report", &json!({"run": 999}), dir.path(), &connect);
        assert!(is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains("no run 999"), "{content:?}");
    }

    #[test]
    fn close_report_forwards_the_run_filter() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"close_report""#) && line.contains(r#""run":55"#), "{line}");
            Ok(r#"{"id":1,"ok":true,"data":{"closed":true}}"#.to_string())
        };
        let (content, is_err) = call_tool("close_ggo_report", &json!({"run": 55}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""closed":true"#));
    }

    #[test]
    fn fetch_report_unknown_run_is_a_tool_error() {
        let dir = seeded_db_dir();
        let err = fetch_report(dir.path(), 999).unwrap_err();
        assert!(err.contains("no run 999"), "{err}");
        assert!(err.contains("ggo_ide.db"), "{err}");
    }

    /// No `~/.ggo` at all: the missing diag.db is swallowed (nothing to
    /// clone), and the missing ide db is the same "no such run" error --
    /// never a stray empty database left behind.
    #[test]
    fn fetch_report_with_no_databases_is_a_tool_error_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let err = fetch_report(dir.path(), 1).unwrap_err();
        assert!(err.contains("no run 1"), "{err}");
        assert!(!dir.path().join("ggo_ide.db").exists(), "must not create an empty db");
    }

    /// A `diag.db` this build cannot read -- here one whose `runs` table
    /// predates `007_perf_run_id.sql`, which is how a real one goes stale:
    /// ggo-diag owns that file, and only ggo-diag migrates it.
    fn seed_stale_diag_db(dir: &Path) {
        let path = dir.join("diag.db");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("DROP TABLE runs", ()).await.unwrap();
            conn.execute(
                "CREATE TABLE runs (id TEXT PRIMARY KEY, started_at TEXT, branch TEXT,
                                    commit_hash TEXT, git_describe TEXT, hostname TEXT,
                                    verdict TEXT, boot_outcome TEXT, telem_overflows INTEGER,
                                    state TEXT NOT NULL DEFAULT 'done', updated_at TEXT)",
                (),
            )
            .await
            .unwrap();
        });
    }

    /// The clone is best effort for EVERY failure, not just a missing
    /// file: a diag.db that cannot be cloned must not fail a report the
    /// ide db can already answer -- which is every emulator run.
    #[test]
    fn fetch_report_still_reports_when_the_diag_clone_fails() {
        let dir = seeded_db_dir();
        seed_stale_diag_db(dir.path());
        // Pre-condition: this diag.db really does break the clone, with an
        // error nothing here is allowed to special-case.
        let clone_err = ggo_worldlib::charts::reports::diag_db::clone_runs(
            &dir.path().join("diag.db"),
            &dir.path().join("ggo_ide.db"),
        )
        .unwrap_err();
        assert!(clone_err.contains("no such column"), "{clone_err}");

        let content = fetch_report(dir.path(), 7).unwrap();
        assert!(content[0]["text"].as_str().unwrap().contains("GemdropGo perf run #7"));
    }

    /// ...and when the run really is missing, the swallowed clone error
    /// comes back as context, since it may be exactly why.
    #[test]
    fn fetch_report_unknown_run_carries_the_clone_failure_as_context() {
        let dir = seeded_db_dir();
        seed_stale_diag_db(dir.path());
        let err = fetch_report(dir.path(), 999).unwrap_err();
        assert!(err.contains("no run 999"), "{err}");
        assert!(err.contains("no such column"), "{err}");
    }
}
