//! Tool definitions and execution: each MCP tool resolves a target Zed
//! session from the registry, sends one protocol line over its socket,
//! and shapes the answer into MCP content.
//!
//! The surface covers emulator control in both modes — LOCK-STEP, where
//! `emu_start` boots a cart paused and each `emu_next_frame` latches pad
//! input and runs exactly one frame, and free-running, where `emu_run`
//! plays the cart under `emu_pause`/`emu_resume` — alongside the PPU
//! inspector, cart packing, hardware flash with its readiness probe,
//! perf reports, and reads of the authored world.

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

/// A pack is a cargo build; the lock-step bound would cut it off.
const PACK_TIMEOUT: Duration = Duration::from_secs(600);

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
          "description": "Boot a cart in the Zed emulator panel and pause at the first frame boundary (lock-step). Pack first with cart_pack (or emd pack-ggo --world <stem>). Returns the initial world-state JSON — worlds that declare `InspectWorld = {}` under [resources] serialize every entity's registered components; others return null. Drive with emu_next_frame; end with emu_stop.",
          "inputSchema": with(json!({ "cart": { "type": "string", "description": "Worktree-relative packed cart path, e.g. wilds.ggo" } })) },
        { "name": "emu_next_frame",
          "description": "Latch the held pad buttons (names: z x a s up down left right q w e r t y u i enter select; empty/omitted releases all), run exactly ONE frame, and return the new world-state JSON. Set screenshot=true to also get the presented frame as PNG. Call repeatedly to play.",
          "inputSchema": with(json!({
              "buttons": { "type": "array", "items": { "type": "string" } },
              "screenshot": { "type": "boolean" }
          })) },
        { "name": "emu_stop", "description": "End the lock-step run; returns the cart's uart log.", "inputSchema": with(json!({})) },
        { "name": "emu_screenshot",
          "description": "The emulator's last presented frame as PNG (320×240), for a lock-step or free-running run. Errors when no frame has been presented yet.",
          "inputSchema": with(json!({})) },
        { "name": "emu_uart",
          "description": "The run's UART/console log, newest `tail` lines (default all), readable while the run is live — read a panic without stopping the run.",
          "inputSchema": with(json!({ "tail": { "type": "number", "description": "Newest N lines (omit for the whole log)" } })) },
        { "name": "emu_run",
          "description": "Boot a cart free-running in the Zed emulator panel — the panel's own Run button, no lock-step. The game plays itself; watch it with emu_screenshot and emu_uart, pause with emu_pause. Use emu_start instead when you need to drive input frame by frame. Pack first: cart_pack (or emd pack-ggo).",
          "inputSchema": with(json!({ "cart": { "type": "string", "description": "Worktree-relative packed cart path, e.g. target/ggo-emulate/worlds-arena.ggo" } })) },
        { "name": "emu_pause", "description": "Pause the live run at the next frame boundary. Returns {paused, frame, running} — running = a run is live (true even while paused).", "inputSchema": with(json!({})) },
        { "name": "emu_resume", "description": "Resume a paused run. Returns {paused, frame, running} — running = a run is live (true even while paused).", "inputSchema": with(json!({})) },
        { "name": "emu_debug",
          "description": "The PPU inspector for the run in flight — what the frame-step debugger shows a human. view=tiles: every VRAM tile at 1× as PNG, colored by bank (0 bg/fg, 1 sprites) and palette; view=map: one background layer (0–3) composed at 1× as PNG plus scroll/enabled/priority; view=oam: the sprites composited on a blank 320×240 screen as PNG plus one text row per OAM entry; view=palettes: both palette banks as RGB565 hex (no image). Use it for 'why is this sprite garbage' — the answer is usually a wrong palette or tile index visible here.",
          "inputSchema": with(json!({
              "view": { "type": "string", "enum": ["tiles", "map", "oam", "palettes"] },
              "bank": { "type": "number", "description": "tiles: 0 bg/fg, 1 sprites (default 0)" },
              "palette": { "type": "number", "description": "tiles: palette index (default 0)" },
              "layer": { "type": "number", "description": "map: layer 0–3 (default 0)" }
          })) },
        { "name": "cart_pack",
          "description": "Build one world into a runnable cart: `emd pack-ggo --world <stem>` into the project's target/ggo-emulate/. Takes a world stem (worlds/arena) or file (assets/worlds/arena.toml). Returns {cart, world, lines}: hand `cart` to emu_start or emu_run. Blocks until the pack finishes (a cold build can take minutes; the 15s socket timeout is raised for this call) — and while it runs, every other tool against this Zed waits, because the host serves one request at a time. Errors carry emd's own failure line.",
          "inputSchema": with(json!({ "world": { "type": "string", "description": "World stem, e.g. worlds/arena" } })) },
        { "name": "hw_flash",
          "description": "Flash a world to the GemdropGo board and run it (build, program, boot-verify over UART, then a timed gameplay telemetry capture). Flashing is intensive (occupies the board; ~20 min with rebuild_gateware). Confirm with the user before invoking. Always pass an explicit `world` stem (e.g. worlds/chase_cam): omitting it flashes whichever world the panel last remembered or has open, which is often not the one you mean. Every other knob has a default (the default set: rebuild_gateware=false, tty=first serial port found, baud=460800, collect_seconds=120, telemetry=false); pass only what you mean to change. Returns as soon as the flash STARTS, with `config` = the effective configuration, defaults filled in — poll hw_flash_status, or block on hw_flash_wait. Even a start that errors opens the hardware tab in the user's Zed.",
          "inputSchema": with(json!({
              "world": { "type": "string", "description": "World stem to bake in as the boot world, e.g. worlds/chase_cam" },
              "rebuild_gateware": { "type": "boolean", "description": "Re-run place-and-route (~20 min) instead of reusing the cached bitstream; only needed after a gateware change (default false)" },
              "tty": { "type": "string", "description": "Serial device, e.g. /dev/ttyUSB0 (default: the first port the panel's scan found)" },
              "baud": { "type": "number", "description": "UART baud (default 460800)" },
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
        { "name": "hw_env",
          "description": "Is this machine ready to flash? {ready, missing[{code,label}], ports, stuck_board, project, repo, diag_bin, emd_bin, version_skew:[repo_commit, emu_commit]|null, emu_commit_in_repo}. Call before hw_flash: a missing prerequisite here is what hw_flash would fail on, and version_skew means the board would render a different PPU than the in-IDE emulator. Probes fresh each call.",
          "inputSchema": with(json!({})) },
        { "name": "hw_flash_cancel",
          "description": "Cancel the flash in flight, the same as the user's Cancel button. Returns {cancelled: bool}; false when nothing was running (including when no emulator panel has ever been opened in the workspace). The timeline keeps the phase it reached; a cancelled run is not a failed one.",
          "inputSchema": with(json!({})) },
        { "name": "list_ggo_reports",
          "description": "Every report in the ggo database, as two newest-first sections, each under a header naming its time zone. Under `--- runs (perf stamps UTC, device stamps local time) ---`, the runs: run id, started_at (ISO-UTC for emulator perf runs, the local underscore stamp for ggo-diag device runs), cart, label, and the ggo-diag log path for board runs. Under `--- faults (local time) ---`, the ggo-uartd fault dumps: fault id, timestamp (the daemon's local wall clock), kind: detail, and the probable perf run. The two zones are not comparable as text — line a fault up against a run only after converting. Fresh dumps are imported on the way. Reads the database directly — no Zed session needed.",
          "inputSchema": json!({
              "type": "object",
              "properties": { "limit": { "type": "number", "description": "Newest N of each section (default 20)" } },
          }) },
        { "name": "fetch_ggo_report",
          "description": "Paste-ready summary of one report from the ggo database. With `run` (a perf run, emulator or board): cart, frame budget, wire/cache aggregates, over-budget frames, and the ggo-diag log path when the run has one — takes the run number hw_flash_wait/hw_flash_status report as perf_run_id, or one from list_ggo_reports. With `fault` (a ggo-uartd dump id from list_ggo_reports): the dump digest — kind and detail, boot stage, frame telemetry, parsed panics and asset-load failures, the fault line marked » with 20 lines of context before and 5 after, and the raw dump path. One of the two is required; `run` wins when both are given. Reads the database directly — no Zed session needed.",
          "inputSchema": json!({
              "type": "object",
              "properties": {
                  "run": { "type": "number", "description": "Perf run id, e.g. perf_run_id from hw_flash_wait" },
                  "fault": { "type": "string", "description": "Fault dump id from list_ggo_reports, e.g. 2026-09-02_08-49-33_marker" },
              },
          }) },
        { "name": "open_ggo_report",
          "description": "Open the Reports tab in the user's Zed on one perf run (the same page a passed flash lands on) or on one ggo-uartd fault dump. One of `run`/`fault` is required; `run` wins when both are given. The id is checked against the ggo database first, so an unknown one is an error here rather than a tab that lands on the list.",
          "inputSchema": with(json!({
              "run": { "type": "number", "description": "Perf run id from list_ggo_reports" },
              "fault": { "type": "string", "description": "Fault dump id from list_ggo_reports, e.g. 2026-09-02_08-49-33_marker" },
          })) },
        { "name": "close_ggo_report",
          "description": "Close the Reports tab in the user's Zed. With `run`, only if that is the run it shows. Returns {closed: false} when no tab is open.",
          "inputSchema": with(json!({ "run": { "type": "number", "description": "Only close if the tab shows this run (optional)" } })) },
        { "name": "world_list",
          "description": "Every world file in the open project: {worlds: [{stem, rel_path}]}. Stems are what emd, cart_pack and hw_flash take (worlds/arena).",
          "inputSchema": with(json!({})) },
        { "name": "world_open",
          "description": "Open a world in Zed's World panel, as clicking its file would. Returns {opened: rel_path}. The already-open world is left alone (no reload, no prompt); a world with unsaved edits is never swapped out from under the user — save it first.",
          "inputSchema": with(json!({ "world": { "type": "string", "description": "World stem (worlds/arena) or rel path" } })) },
        { "name": "world_read",
          "description": "The world as the designer authored it, from the World panel: {stem, rel_path, dirty, entities[{index, pos, components}], instances[{index, world, pos, background_priority, error}], backgrounds[{layer, map}], selected[]}. Pass `world` to open one first. This is the level layout — what world_screenshot draws and what the cart boots — not the running game (that is emu_next_frame's world JSON).",
          "inputSchema": with(json!({ "world": { "type": "string", "description": "Open this world first (stem or rel path); omit to read the one already open" } })) },
        { "name": "world_screenshot",
          "description": "The authored world drawn to a PNG from the World panel — the level layout as designed, not a running frame. Default: the 320×240 device screen at the world's active camera (what the board shows on boot). full=true: the whole scene's bounding box (capped 4096²). Sprites, tilemaps and backgrounds render with real pixels; text and placeholder entities draw as flat boxes. Pass `world` to open one first.",
          "inputSchema": with(json!({
              "world": { "type": "string", "description": "Open this world first (stem or rel path); omit for the open one" },
              "full": { "type": "boolean", "description": "Frame the whole scene instead of the device screen" }
          })) },
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

/// A `{width, height, bgra_base64}` reply as MCP PNG image content.
fn image_content(shot: &Value) -> Result<Value, String> {
    use base64::Engine as _;
    let bgra = base64::engine::general_purpose::STANDARD
        .decode(shot["bgra_base64"].as_str().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let (width, height) = (
        shot["width"].as_u64().unwrap_or(0) as u32,
        shot["height"].as_u64().unwrap_or(0) as u32,
    );
    let png = bgra_to_png(width, height, &bgra)?;
    Ok(json!({
        "type": "image",
        "mimeType": "image/png",
        "data": base64::engine::general_purpose::STANDARD.encode(&png),
    }))
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
        return list_reports(&db_url()?, &ggo_dir()?, limit);
    }
    if name == "fetch_ggo_report" {
        let (db_url, db_dir) = (db_url()?, ggo_dir()?);
        return match (arg_i64(args, "run"), arg_str(args, "fault")) {
            (Some(run), _) => fetch_report(&db_url, &db_dir, run),
            (None, Some(fault)) => fetch_fault(&db_url, &db_dir, &fault),
            (None, None) => Err(NEEDS_RUN_OR_FAULT.to_string()),
        };
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
                content.push(image_content(&shot)?);
            }
            Ok(content)
        }
        "emu_stop" => {
            let data = send(&session.socket, Cmd::Stop { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "emu_screenshot" => {
            let data = send(&session.socket, Cmd::Screenshot { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![image_content(&data)?])
        }
        "emu_uart" => {
            let tail = arg_i64(args, "tail").filter(|n| *n > 0).map(|n| n as usize);
            let data = send(&session.socket, Cmd::Uart { workspace, tail }, CALL_TIMEOUT, connect)?;
            let lines: Vec<String> = data["lines"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            Ok(vec![json!({ "type": "text", "text": lines.join("\n") })])
        }
        "emu_run" => {
            let cart = arg_str(args, "cart").ok_or("missing required argument: cart")?;
            let data = send(&session.socket, Cmd::Run { workspace, cart }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "emu_pause" => {
            let data = send(&session.socket, Cmd::Pause { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "emu_resume" => {
            let data = send(&session.socket, Cmd::Resume { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "emu_debug" => {
            let view = match arg_str(args, "view").as_deref() {
                Some("tiles") => ggo_emu_remote::protocol::DebugView::Tiles,
                Some("map") => ggo_emu_remote::protocol::DebugView::Map,
                Some("oam") => ggo_emu_remote::protocol::DebugView::Oam,
                Some("palettes") => ggo_emu_remote::protocol::DebugView::Palettes,
                other => return Err(format!("view must be tiles|map|oam|palettes, got {other:?}")),
            };
            // A negative index is a caller mistake, not a request for
            // the default -- the same rule hw_flash applies to its knobs.
            let index = |key: &str| match arg_i64(args, key) {
                Some(n) if n < 0 => Err(format!("{key} must be >= 0")),
                Some(n) => Ok(n as usize),
                None => Ok(0),
            };
            let mut data = send(
                &session.socket,
                Cmd::Debug {
                    workspace,
                    view,
                    bank: index("bank")?,
                    palette: index("palette")?,
                    layer: index("layer")?,
                },
                CALL_TIMEOUT,
                connect,
            )?;
            // The image rides out as MCP image content, not as base64 in
            // the text block: the rows/labels stay readable that way.
            let image = data.as_object_mut().and_then(|o| o.remove("image"));
            let mut content = vec![json!({ "type": "text", "text": data.to_string() })];
            if let Some(image) = image {
                content.push(image_content(&image)?);
            }
            Ok(content)
        }
        "cart_pack" => {
            let world = arg_str(args, "world")
                .ok_or("missing required argument: world (a stem like worlds/arena)")?;
            let data = send(&session.socket, Cmd::PackWorld { workspace, world }, PACK_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_flash" => {
            // A zero or negative knob is a caller mistake, not a request
            // for the default (`as u32` would wrap it into nonsense).
            let baud = match arg_i64(args, "baud") {
                Some(n) if n <= 0 || n > i64::from(u32::MAX) => return Err("baud must be > 0".to_string()),
                Some(n) => Some(n as u32),
                None => None,
            };
            let collect_seconds = match arg_i64(args, "collect_seconds") {
                Some(n) if n <= 0 => return Err("collect_seconds must be > 0".to_string()),
                Some(n) => Some(n as u64),
                None => None,
            };
            let config = FlashConfig {
                world: arg_str(args, "world"),
                rebuild_gateware: args.get("rebuild_gateware").and_then(Value::as_bool).unwrap_or(false),
                tty: arg_str(args, "tty"),
                baud,
                collect_seconds,
                telemetry: args.get("telemetry").and_then(Value::as_bool).unwrap_or(false),
            };
            let data = send(&session.socket, Cmd::FlashWorld { workspace, config }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_flash_status" => {
            let data = send(&session.socket, Cmd::FlashStatus { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_env" => {
            let data = send(&session.socket, Cmd::HwEnv { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_flash_cancel" => {
            let data = send(&session.socket, Cmd::FlashCancel { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "open_ggo_report" => {
            let cmd = open_report_cmd(&db_url()?, &ggo_dir()?, workspace, args)?;
            let data = send(&session.socket, cmd, CALL_TIMEOUT, connect)?;
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
        "world_list" => {
            let data = send(&session.socket, Cmd::WorldList { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "world_open" => {
            let world = arg_str(args, "world").ok_or("missing required argument: world")?;
            let data =
                send(&session.socket, Cmd::WorldOpen { workspace, world }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "world_read" => {
            let world = arg_str(args, "world");
            let data =
                send(&session.socket, Cmd::WorldRead { workspace, world }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "world_screenshot" => {
            let world = arg_str(args, "world");
            let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
            let data = send(
                &session.socket,
                Cmd::WorldScreenshot { workspace, world, full },
                CALL_TIMEOUT,
                connect,
            )?;
            Ok(vec![image_content(&data)?])
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
/// before it can name the run's perf row. Waiting this long for it (in
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

/// `~/.ggo` -- the FILE tree beside the database: `ggo-diag`'s run logs
/// and `ggo-uartd`'s dump directory. Resolved here rather than through
/// `ggo_common` so this bridge binary need not pull in a gpui-shaped
/// crate for two lines.
fn ggo_dir() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("cannot find your home directory (neither HOME nor USERPROFILE is set)")?;
    Ok(std::path::PathBuf::from(home).join(".ggo"))
}

/// The one PostgreSQL database every ggo tool shares -- `$GGO_DATABASE_URL`
/// when set, else the local `ggo-pg` service. Every report row this module
/// prints comes from here; `ggo_dir` only supplies the files beside them.
fn db_url() -> Result<String, String> {
    ggo_db::url()
}

/// How the report tools name the database in their errors. There is no
/// path to print any more -- one server, one url, and the url may carry a
/// password -- so the messages say what it IS.
const DB_NAME: &str = "the ggo database";

/// The two report sections' headers. They carry the ZONE because the
/// producers disagree about it and the stamps do not say so themselves: a
/// perf run's `started_at` is ISO-UTC (ggo-server's ingest writes it that
/// way), a device run's is the LOCAL underscore stamp ggo-diag names its
/// run by (history's reconcile copies it as-is), and a dump's id and `at`
/// are `ggo-uartd`'s LOCAL wall clock off the file name. Listed together and unlabelled, one afternoon reads
/// as two, and a reader lining a fault up against the run it happened
/// during is out by the machine's UTC offset.
///
/// Each header is printed whenever its section has rows, a lone section
/// included -- that is the case where there is nothing else on screen to
/// infer the zone from.
const RUNS_HEADER: &str = "--- runs (perf stamps UTC, device stamps local time) ---";
const FAULTS_HEADER: &str = "--- faults (local time) ---";

/// Every perf run in the ggo database, newest first, one line each, with
/// the ggo-diag log path beside the board runs that have one, then the
/// daemon faults as a second newest-first section. Both sections are
/// headed with their zone -- see [`RUNS_HEADER`].
///
/// Device runs need no copying across any more: `ggo-diag` writes its
/// rows into this same database, so the list reads one source.
fn list_reports(db_url: &str, db_dir: &Path, limit: usize) -> Result<Vec<Value>, String> {
    use ggo_worldlib::charts::reports::{faults, perf_db};

    // Before the reads below: a machine that has only ever run the daemon
    // has no fault rows at all until this import writes them.
    let import_error = import_faults(db_url, db_dir);
    let none = |what: String| {
        let text = format!("{what}{}", import_failure_note(db_dir, import_error.as_ref()));
        Ok(vec![json!({ "type": "text", "text": text })])
    };
    // The aggregate-free index: `cart_runs` would scan every frame row of
    // every cart for averages this list never prints.
    let rows: Vec<_> = perf_db::run_index(db_url)
        .map_err(|e| format!("reading runs: {e:#}"))?
        .into_iter()
        .map(|r| (r.started_at, r.id, r.cart_name, r.label, r.frames))
        .collect();
    let logs_dir = db_dir.join("diag").join("logs");
    let run_lines: Vec<String> = rows
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
    let fault_lines: Vec<String> = faults::list(db_url, limit as i64)
        .map_err(|e| format!("reading faults: {e}"))?
        .iter()
        .map(|f| {
            format!(
                "fault {}  {}  {}: {}  run={}",
                f.id,
                f.at,
                f.kind,
                f.detail,
                f.run_id.as_deref().unwrap_or("-")
            )
        })
        .collect();
    if run_lines.is_empty() && fault_lines.is_empty() {
        return none("no runs yet".to_string());
    }
    // Two sources, two orders AND two zones: perf runs are ordered by
    // their start stamp and faults by the daemon's, so they are listed as
    // two newest-first sections rather than one interleaved list that
    // would claim an ordering between them.
    let mut lines: Vec<String> = Vec::new();
    if !run_lines.is_empty() {
        lines.push(RUNS_HEADER.to_string());
        lines.extend(run_lines);
    }
    if !fault_lines.is_empty() {
        lines.push(FAULTS_HEADER.to_string());
        lines.extend(fault_lines);
    } else if let Some(e) = &import_error {
        // Runs to show but no faults: the caller would otherwise read the
        // missing section as "no dumps", not "the dumps did not import".
        lines.push(format!("faults: import failed: {e}"));
    }
    Ok(vec![json!({ "type": "text", "text": lines.join("\n") })])
}

/// One perf run's paste-ready summary out of the ggo database, with the
/// ggo-diag log beside it from `db_dir`. Both are parameters so tests can
/// point at a fixture database and a temp directory; production passes
/// [`db_url`] and `~/.ggo`.
fn fetch_report(db_url: &str, db_dir: &Path, run: i64) -> Result<Vec<Value>, String> {
    use ggo_worldlib::charts::reports::perf_db;

    let detail = run_detail_or_missing(db_url, run)?;
    let frames =
        perf_db::run_frames(db_url, run).map_err(|e| format!("reading run {run} frames: {e:#}"))?;
    let mut text = perf_db::run_handoff_text(&detail, &frames);
    let log = ggo_emu_remote::diag_log_path(&db_dir.join("diag").join("logs"), &detail.started_at);
    text.push_str(&format!(
        "\nggo_diag_log: {}\n",
        log.map(|p| p.display().to_string()).unwrap_or_else(|| "- (emulator run, or log pruned)".to_string())
    ));
    Ok(vec![json!({ "type": "text", "text": text })])
}

/// `ggo-uartd`'s dump directory under `<db_dir>`.
fn faults_dir(db_dir: &Path) -> std::path::PathBuf {
    db_dir.join("uartd").join("faults")
}

/// Pull every dump the reports database has not seen into it, returning
/// why it could not when it could not. Best effort: an unreadable dump is
/// the daemon's problem and must not fail a tool the rows already answer
/// -- but a caller looking at an empty faults section is TOLD, since the
/// failure may be exactly why it is empty. Dumps that individually fail to
/// parse are skipped (and named on stderr) by `import` itself.
fn import_faults(db_url: &str, db_dir: &Path) -> Option<String> {
    let dir = faults_dir(db_dir);
    match ggo_worldlib::charts::reports::faults::import(&dir, db_url) {
        Ok(_) => None,
        Err(e) => {
            eprintln!("faults: importing {}: {e}", dir.display());
            Some(e)
        }
    }
}

/// The parenthetical both "nothing to show" messages carry when the
/// import is why there is nothing.
fn import_failure_note(db_dir: &Path, error: Option<&String>) -> String {
    match error {
        Some(e) => {
            format!(" (and importing faults from {} failed: {e})", faults_dir(db_dir).display())
        }
        None => String::new(),
    }
}

/// How much of the decoded window `fetch_fault` prints around the fault
/// line: the lead-up is what explains it, the tail only shows whether
/// anything came after.
const FAULT_LINES_BEFORE: usize = 20;
const FAULT_LINES_AFTER: usize = 5;

/// One daemon fault's paste-ready digest out of the ggo database: header,
/// boot stage, telemetry, parsed panics and asset failures, the fault line
/// in context, and the path of the raw dump under `db_dir`.
fn fetch_fault(db_url: &str, db_dir: &Path, id: &str) -> Result<Vec<Value>, String> {
    use ggo_worldlib::charts::reports::faults;

    // A dump written seconds ago is fetchable by id straight from the
    // list, without the panel having been opened in between.
    let import_error = import_faults(db_url, db_dir);
    let detail = faults::load(db_url, id)
        .map_err(|e| format!("reading fault {id}: {e}"))?
        .ok_or_else(|| {
            format!(
                "no fault {id} in {DB_NAME}{}",
                import_failure_note(db_dir, import_error.as_ref())
            )
        })?;
    let row = &detail.row;
    let number = |value: Option<i64>| match value {
        Some(n) => n.to_string(),
        None => "-".to_string(),
    };
    let mut text = format!(
        "fault {}\n{}: {}\nat {}  last {}s of {}\nboot stage: {}\nframes {}  cyc avg/max {}/{}\nprobable run: {}\n",
        row.id,
        row.kind,
        row.detail,
        row.at,
        detail.window_s,
        row.tty,
        row.boot_stage.as_deref().unwrap_or("-"),
        row.frames,
        number(detail.cyc_avg),
        number(detail.cyc_max),
        row.run_id.as_deref().unwrap_or("-"),
    );
    if !detail.panics.is_empty() {
        text.push_str("\npanics:\n");
        for panic in &detail.panics {
            text.push_str(&format!("  frame {}  {}\n", number(panic.frame), panic.message));
        }
    }
    if !detail.asset_failures.is_empty() {
        text.push_str("\nasset failures:\n");
        for failure in &detail.asset_failures {
            text.push_str(&format!("  {}  {}  x{}\n", failure.kind, failure.path, failure.count));
        }
    }
    let lines = &detail.text;
    match lines.len().checked_sub(1) {
        None => text.push_str("\n(the window decoded to no text lines)\n"),
        Some(end) => {
            let (first, last, what) = match detail.fault_line {
                Some(index) => (
                    index.saturating_sub(FAULT_LINES_BEFORE),
                    end.min(index + FAULT_LINES_AFTER),
                    "» marks the fault line",
                ),
                // A stall or re-enumeration dump is a window around an
                // absence: there is no line to centre on, so the tail --
                // where the window ended -- is what a reader wants.
                None => (
                    lines.len().saturating_sub(FAULT_LINES_BEFORE + FAULT_LINES_AFTER + 1),
                    end,
                    "no marker line in this dump; the tail of the window",
                ),
            };
            text.push_str(&format!(
                "\nlog lines {}-{} of {} ({what}):\n",
                first + 1,
                last + 1,
                lines.len()
            ));
            for (index, line) in lines.iter().enumerate().take(last + 1).skip(first) {
                let mark = if detail.fault_line == Some(index) { "» " } else { "  " };
                text.push_str(&format!("{mark}{line}\n"));
            }
        }
    }
    let raw = faults::raw_path(&faults_dir(db_dir), id);
    // The daemon prunes its dumps and the row outlives the file, so the
    // path alone would be a broken promise.
    let pruned = if raw.is_file() { "" } else { " (pruned; the bytes are in the report db)" };
    text.push_str(&format!("\nraw: {}{pruned}\n", raw.display()));
    Ok(vec![json!({ "type": "text", "text": text })])
}

/// What `open_ggo_report` sends, with the report checked to exist first:
/// a bad id has to be an error here, not a Reports tab that silently
/// lands on the runs list.
fn open_report_cmd(
    db_url: &str,
    db_dir: &Path,
    workspace: Option<String>,
    args: &Value,
) -> Result<Cmd, String> {
    use ggo_worldlib::charts::reports::faults;

    match (arg_i64(args, "run"), arg_str(args, "fault")) {
        (Some(run), _) => {
            run_detail_or_missing(db_url, run)?;
            Ok(Cmd::OpenReport { workspace, run: Some(run), fault: None })
        }
        (None, Some(fault)) => {
            let import_error = import_faults(db_url, db_dir);
            if faults::load(db_url, &fault)
                .map_err(|e| format!("reading fault {fault}: {e}"))?
                .is_none()
            {
                return Err(format!(
                    "no fault {fault} in {DB_NAME}{}",
                    import_failure_note(db_dir, import_error.as_ref())
                ));
            }
            Ok(Cmd::OpenReport { workspace, run: None, fault: Some(fault) })
        }
        (None, None) => Err(NEEDS_RUN_OR_FAULT.to_string()),
    }
}

/// Said by both report tools, which take either id.
const NEEDS_RUN_OR_FAULT: &str =
    "missing required argument: run (a perf run id) or fault (a fault id from list_ggo_reports)";

/// The run's row, or the "no run" error naming the database it looked in.
///
/// Board runs need no cloning to be found here: `ggo-diag` and the
/// emulator's ingest write into the one database this reads.
fn run_detail_or_missing(
    db_url: &str,
    run: i64,
) -> Result<ggo_worldlib::charts::reports::perf_db::RunDetail, String> {
    use ggo_worldlib::charts::reports::perf_db;

    perf_db::run_detail(db_url, run)
        .map_err(|e| format!("reading run {run}: {e:#}"))?
        .ok_or_else(|| format!("no run {run} in {DB_NAME}"))
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
    use ggo_db::sqlx;
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
    fn cart_pack_forwards_the_world_with_the_long_timeout() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, timeout: Duration| -> std::io::Result<String> {
            assert!(
                line.contains(r#""cmd":"pack_world""#) && line.contains(r#""world":"worlds/arena""#),
                "{line}"
            );
            assert_eq!(timeout, PACK_TIMEOUT);
            Ok(r#"{"id":1,"ok":true,"data":{"cart":"target/ggo-emulate/worlds-arena.ggo","world":"worlds/arena","lines":[]}}"#.to_string())
        };
        let (content, is_err) =
            call_tool("cart_pack", &json!({"world": "worlds/arena"}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains("worlds-arena.ggo"));
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
    fn emu_screenshot_is_png_and_emu_uart_is_joined_lines() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]);
        let connect = move |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            if line.contains(r#""cmd":"screenshot""#) {
                Ok(format!(
                    r#"{{"id":1,"ok":true,"data":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}"#
                ))
            } else {
                assert!(line.contains(r#""cmd":"uart""#) && line.contains(r#""tail":2"#), "{line}");
                Ok(r#"{"id":1,"ok":true,"data":{"lines":["a","panic: b"]}}"#.to_string())
            }
        };
        let (content, is_err) = call_tool("emu_screenshot", &json!({}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert_eq!(content[0]["type"], "image");
        let (content, is_err) = call_tool("emu_uart", &json!({"tail": 2}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert_eq!(content[0]["text"], "a\npanic: b");
    }

    #[test]
    fn emu_run_requires_a_cart_and_forwards_it() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"run""#) && line.contains(r#""cart":"a.ggo""#), "{line}");
            Ok(r#"{"id":1,"ok":true,"data":{"started":true,"frame":1,"running":true}}"#.to_string())
        };
        let (content, is_err) = call_tool("emu_run", &json!({}), dir.path(), &connect);
        assert!(is_err && content[0]["text"].as_str().unwrap().contains("cart"), "{content:?}");
        let (content, is_err) = call_tool("emu_run", &json!({"cart": "a.ggo"}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""running":true"#));
    }

    #[test]
    fn emu_debug_splits_the_image_out_as_png_content() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]);
        let connect = move |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""view":"map""#) && line.contains(r#""layer":2"#), "{line}");
            Ok(format!(
                r#"{{"id":1,"ok":true,"data":{{"view":"map","layer":2,"image":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}}}"#
            ))
        };
        let (content, is_err) =
            call_tool("emu_debug", &json!({"view": "map", "layer": 2}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(
            !content[0]["text"].as_str().unwrap().contains("bgra_base64"),
            "image moved out of the text"
        );
        assert_eq!(content[1]["type"], "image");
        let (content, is_err) =
            call_tool("emu_debug", &json!({"view": "sprites"}), dir.path(), &connect);
        assert!(is_err && content[0]["text"].as_str().unwrap().contains("view must be"), "{content:?}");
    }

    #[test]
    fn emu_debug_rejects_a_negative_index_instead_of_defaulting_it() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            panic!("a negative index must never reach the host")
        };
        let (content, is_err) =
            call_tool("emu_debug", &json!({"view": "map", "layer": -1}), dir.path(), &connect);
        assert!(is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains("layer must be >= 0"), "{content:?}");
        let (content, is_err) =
            call_tool("emu_debug", &json!({"view": "tiles", "bank": -2}), dir.path(), &connect);
        assert!(is_err && content[0]["text"].as_str().unwrap().contains("bank must be >= 0"), "{content:?}");
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
    fn world_tools_forward_their_commands() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            if line.contains(r#""cmd":"world_list""#) {
                Ok(r#"{"id":1,"ok":true,"data":{"worlds":[{"stem":"worlds/arena","rel_path":"worlds/arena.toml"}]}}"#.to_string())
            } else {
                assert!(line.contains(r#""cmd":"world_read""#) && line.contains(r#""world":"worlds/arena""#), "{line}");
                Ok(r#"{"id":1,"ok":true,"data":{"stem":"worlds/arena","dirty":false,"entities":[]}}"#.to_string())
            }
        };
        let (content, is_err) = call_tool("world_list", &json!({}), dir.path(), &connect);
        assert!(!is_err && content[0]["text"].as_str().unwrap().contains("worlds/arena"), "{content:?}");
        let (content, is_err) = call_tool("world_read", &json!({"world": "worlds/arena"}), dir.path(), &connect);
        assert!(!is_err && content[0]["text"].as_str().unwrap().contains(r#""dirty":false"#), "{content:?}");
    }

    #[test]
    fn world_screenshot_is_png_content() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]);
        let connect = move |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(
                line.contains(r#""cmd":"world_screenshot""#) && line.contains(r#""full":true"#),
                "{line}"
            );
            Ok(format!(
                r#"{{"id":1,"ok":true,"data":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}"#
            ))
        };
        let (content, is_err) =
            call_tool("world_screenshot", &json!({"full": true}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert_eq!(content[0]["type"], "image");
    }

    #[test]
    fn tool_list_names_exactly_the_agent_surface() {
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
                "emu_screenshot",
                "emu_uart",
                "emu_run",
                "emu_pause",
                "emu_resume",
                "emu_debug",
                "cart_pack",
                "hw_flash",
                "hw_flash_status",
                "hw_flash_wait",
                "hw_env",
                "hw_flash_cancel",
                "list_ggo_reports",
                "fetch_ggo_report",
                "open_ggo_report",
                "close_ggo_report",
                "world_list",
                "world_open",
                "world_read",
                "world_screenshot",
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
            Ok(r#"{"id":1,"ok":true,"data":{"started":true,"config":{"world":"worlds/chase_cam","rebuild_gateware":true,"tty":"/dev/ttyUSB0","baud":460800,"collect_seconds":30,"telemetry":false}}}"#.to_string())
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
    fn hw_flash_rejects_a_non_positive_knob_before_touching_zed() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            panic!("a bad knob never reaches the socket")
        };
        for (args, word) in [
            (json!({"world": "worlds/a", "baud": -1}), "baud"),
            (json!({"world": "worlds/a", "collect_seconds": 0}), "collect_seconds"),
        ] {
            let (content, is_err) = call_tool("hw_flash", &args, dir.path(), &connect);
            assert!(is_err, "{content:?}");
            assert!(content[0]["text"].as_str().unwrap().contains(word), "{content:?}");
        }
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

    #[test]
    fn hw_env_and_cancel_forward_their_commands() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            if line.contains(r#""cmd":"hw_env""#) {
                Ok(r#"{"id":1,"ok":true,"data":{"ready":false,"missing":[{"code":"port","label":"no serial device"}]}}"#.to_string())
            } else {
                assert!(line.contains(r#""cmd":"flash_cancel""#), "{line}");
                Ok(r#"{"id":1,"ok":true,"data":{"cancelled":true}}"#.to_string())
            }
        };
        let (content, is_err) = call_tool("hw_env", &json!({}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""code":"port""#));
        let (content, is_err) = call_tool("hw_flash_cancel", &json!({}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains(r#""cancelled":true"#));
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

    /// A throwaway PostgreSQL database on the real migrated schema,
    /// seeded with the same INSERT shape ggo-worldlib's `perf_db` tests
    /// use (one cart, one run, two frames), plus the `~/.ggo` FILE tree
    /// beside it -- `diag/logs` and `uartd/faults` are still files, and
    /// the tools read both.
    ///
    /// The `TestDb` is returned, not dropped: dropping it destroys the
    /// database, so every caller has to keep it alive for the whole test.
    fn seeded_db() -> (ggo_db::TestDb, tempfile::TempDir) {
        let db = ggo_db::TestDb::new();
        let pool = db.pool();
        ggo_db::block_on(async {
            for sql in [
                "INSERT INTO cart (id, name) VALUES (1, 'wilds')",
                "INSERT INTO run (id, cart_id, started_at, frames, frame_budget_cycles,
                                  scanout_wire_cycles, refill_cycles, writeback_cycles,
                                  wire_wait_cycles, label)
                 VALUES (7, 1, '2026-08-31T00:00:00Z', 2, 555549, 164400, 100, 65, 0, 'board')",
            ] {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
            for (n, wire_total, cyc) in [(0i64, 100_000i64, 700_000i64), (1, 600_000, 812_345)] {
                sqlx::query(
                    "INSERT INTO frame (run_id, n, wire_total, i_misses, d_misses, over_budget,
                                        apu_underruns, cyc)
                     VALUES (7, $1, $2, 5, 2, 0, 0, $3)",
                )
                .bind(n)
                .bind(wire_total)
                .bind(cyc)
                .execute(&pool)
                .await
                .unwrap();
            }
        });
        (db, tempfile::tempdir().unwrap())
    }

    /// A url whose socket directory does not exist, so no read through it
    /// can reach a server. The postgres analog of the old "point at a file
    /// that is not a database" fixture.
    const UNREACHABLE_DB_URL: &str = "postgres://ggo@localhost/ggo?host=/nonexistent/ggo-pg-socket";

    /// A run id no serial sequence will ever hand out, so the tests that
    /// go through `call_tool` -- which resolves the REAL database, the
    /// developer's own -- cannot collide with a run that is actually in it.
    const UNKNOWN_RUN: i64 = i64::MAX;

    #[test]
    fn fetch_report_renders_the_run_handoff_text_and_its_log_path() {
        let (db, dir) = seeded_db();
        let content = fetch_report(db.url(), dir.path(), 7).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("GemdropGo perf run #7"), "{text}");
        assert!(text.contains("wilds"), "{text}");
        assert!(text.contains("frame_rows_loaded: 2"), "{text}");
        assert!(text.contains("ggo_diag_log: -"), "{text}");

        let logs = dir.path().join("diag").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log = logs.join("main_abc1234_2026-08-31T00:00:00Z.log");
        std::fs::write(&log, "").unwrap();
        let content = fetch_report(db.url(), dir.path(), 7).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("ggo_diag_log: {}", log.display())), "{text}");
    }

    #[test]
    fn list_reports_lists_runs_newest_first_with_their_logs() {
        let (db, dir) = seeded_db();
        let logs = dir.path().join("diag").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log = logs.join("main_abc1234_2026-08-31T00:00:00Z.log");
        std::fs::write(&log, "").unwrap();
        let content = list_reports(db.url(), dir.path(), 20).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], RUNS_HEADER, "{text}");
        assert!(lines[1].starts_with("run 7  2026-08-31T00:00:00Z  wilds  label=board  frames=2  log="), "{text}");
        assert!(text.ends_with(&log.display().to_string()), "{text}");
        assert_eq!(list_reports(db.url(), dir.path(), 0).unwrap()[0]["text"], "no runs yet");
    }

    /// A migrated database nothing has been recorded into yet is the
    /// ordinary state of a fresh machine, not a failure.
    #[test]
    fn list_reports_of_an_empty_database_says_there_are_no_runs_yet() {
        let db = ggo_db::TestDb::new();
        let dir = tempfile::tempdir().unwrap();
        let text =
            list_reports(db.url(), dir.path(), 20).unwrap()[0]["text"].as_str().unwrap().to_string();
        assert_eq!(text, "no runs yet", "{text}");
    }

    /// A database the tool cannot REACH is a different thing from an
    /// empty one, and must not be reported as "no runs yet": the error
    /// carries the hint that says how to fix it.
    #[test]
    fn list_reports_of_an_unreachable_database_is_an_error_that_says_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let err = list_reports(UNREACHABLE_DB_URL, dir.path(), 20).unwrap_err();
        assert!(err.contains(ggo_db::INSTALL_HINT), "{err}");
    }

    /// The routing test: `call_tool` checks the id through
    /// `open_report_cmd` BEFORE it reaches for a socket, so an unknown run
    /// is a tool error rather than a Reports tab that lands on the list.
    ///
    /// This entry point resolves the database itself ([`db_url`]), i.e.
    /// the developer's own -- so the assertion is only that the error is
    /// ABOUT the run asked for, which holds whether the run is missing
    /// ("no run N") or the database could not be read at all ("reading run
    /// N: ..."). The exact sentence is pinned against a fixture database
    /// by `open_report_cmd_takes_a_run_or_a_fault_and_needs_one_of_them`.
    #[test]
    fn open_report_rejects_an_unknown_run_before_touching_zed() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            panic!("no socket call for a run that does not exist")
        };
        let (content, is_err) =
            call_tool("open_ggo_report", &json!({"run": UNKNOWN_RUN}), dir.path(), &connect);
        assert!(is_err, "{content:?}");
        assert!(
            content[0]["text"].as_str().unwrap().contains(&UNKNOWN_RUN.to_string()),
            "{content:?}"
        );
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
        let (db, dir) = seeded_db();
        let err = fetch_report(db.url(), dir.path(), 999).unwrap_err();
        assert!(err.contains("no run 999"), "{err}");
        assert!(err.contains(DB_NAME), "{err}");
    }

    /// An empty database answers the same way: the run is missing, and
    /// that is the whole error -- there is no second file to blame.
    #[test]
    fn fetch_report_against_an_empty_database_is_a_tool_error() {
        let db = ggo_db::TestDb::new();
        let dir = tempfile::tempdir().unwrap();
        let err = fetch_report(db.url(), dir.path(), 1).unwrap_err();
        assert!(err.contains("no run 1"), "{err}");
        assert!(err.contains(DB_NAME), "{err}");
    }

    /// ...and a database that cannot be reached is NOT "no such run": the
    /// read failed, and the caller is told how to fix it.
    #[test]
    fn fetch_report_of_an_unreachable_database_says_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let err = fetch_report(UNREACHABLE_DB_URL, dir.path(), 7).unwrap_err();
        assert!(err.contains("reading run 7"), "{err}");
        assert!(err.contains(ggo_db::INSTALL_HINT), "{err}");
    }

    /// The dump `ggo-uartd` would leave in `<db_dir>/uartd/faults`: a
    /// marker fault whose window carries a boot marker, an asset failure,
    /// a panic and the marker line itself. Returns the file's path.
    const FAULT_ID: &str = "2026-09-02_08-49-33_marker";

    fn seed_fault_dump(db_dir: &Path) -> PathBuf {
        let faults = db_dir.join("uartd").join("faults");
        std::fs::create_dir_all(&faults).unwrap();
        let path = faults.join(format!("{FAULT_ID}.log"));
        std::fs::write(
            &path,
            concat!(
                "# ggo-uartd marker <<<PANIC>>> — last 30s of /dev/ttyUSB0\n",
                "<<<BOOTROM alive>>>\n",
                "asset: MISS \"sprites/hero.spr\"\n",
                "f=2| panicked at 'index out of range', src/main.rs:1:1\n",
                "<<<PANIC>>> trap: mcause=0x2\n",
                "still draining\n",
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn list_reports_lists_faults_after_the_runs() {
        let (db, dir) = seeded_db();
        seed_fault_dump(dir.path());
        let content = list_reports(db.url(), dir.path(), 20).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // The two sections are in DIFFERENT zones -- a run's `started_at`
        // is ISO-UTC off ggo-server's ingest, a dump's stamp is the
        // daemon's local wall clock -- so each says which it is. Mixed
        // and unlabelled, the same afternoon reads as two.
        assert_eq!(lines[0], "--- runs (perf stamps UTC, device stamps local time) ---", "{text}");
        assert!(lines[1].starts_with("run 7  2026-08-31T00:00:00Z  wilds"), "{text}");
        assert_eq!(lines[2], "--- faults (local time) ---", "{text}");
        assert_eq!(
            lines[3],
            "fault 2026-09-02_08-49-33_marker  2026-09-02_08-49-33  marker: <<<PANIC>>>  run=-",
            "{text}"
        );
    }

    /// A machine that has only ever run the daemon still gets its faults:
    /// an empty perf list is not an empty report list. The faults section
    /// still names its zone -- a lone section is the case where a reader
    /// has nothing else to compare the stamps against.
    #[test]
    fn list_reports_lists_a_fault_with_no_perf_runs() {
        let db = ggo_db::TestDb::new();
        let dir = tempfile::tempdir().unwrap();
        seed_fault_dump(dir.path());
        let text =
            list_reports(db.url(), dir.path(), 20).unwrap()[0]["text"].as_str().unwrap().to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], FAULTS_HEADER, "{text}");
        assert!(lines[1].starts_with("fault 2026-09-02_08-49-33_marker  "), "{text}");
        assert!(!text.contains(RUNS_HEADER), "no runs, no runs section: {text}");
    }

    #[test]
    fn fetch_fault_renders_the_digest_the_marked_line_and_the_raw_path() {
        let (db, dir) = seeded_db();
        let raw = seed_fault_dump(dir.path());
        let content = fetch_fault(db.url(), dir.path(), FAULT_ID).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.starts_with("fault 2026-09-02_08-49-33_marker"), "{text}");
        assert!(text.contains("marker: <<<PANIC>>>"), "{text}");
        assert!(text.contains("last 30s of /dev/ttyUSB0"), "{text}");
        assert!(text.contains("boot stage: boot-rom alive"), "{text}");
        assert!(text.contains("panicked at"), "{text}");
        assert!(text.contains("sprites/hero.spr"), "{text}");
        assert!(text.contains("\n» <<<PANIC>>> trap: mcause=0x2\n"), "{text}");
        assert!(text.contains("  still draining"), "{text}");
        assert!(text.contains(&format!("raw: {}", raw.display())), "{text}");
    }

    /// A `fault` table the import cannot WRITE but both reads can still
    /// use: `imported_at` is the one column `import`'s INSERT names that
    /// neither `list` nor `load` selects, so dropping it fails the import
    /// on a missing column while leaving every read exactly as it was.
    ///
    /// This is the one way `faults::import` reports failure at all -- it
    /// skips a bad dump file itself, and an absent faults directory is
    /// `Ok(0)` -- so the error has to come from the database side.
    fn break_the_fault_import(db: &ggo_db::TestDb) {
        let pool = db.pool();
        ggo_db::block_on(async {
            sqlx::query("ALTER TABLE fault DROP COLUMN imported_at")
                .execute(&pool)
                .await
                .unwrap();
        });
    }

    /// A dump that cannot be imported must not read as "no dumps": the
    /// empty list has to say why it is empty.
    #[test]
    fn list_reports_says_why_the_faults_could_not_be_imported() {
        let db = ggo_db::TestDb::new();
        break_the_fault_import(&db);
        let dir = tempfile::tempdir().unwrap();
        seed_fault_dump(dir.path());
        let content = list_reports(db.url(), dir.path(), 20).unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.starts_with("no runs yet"), "{text}");
        assert!(text.contains("importing faults from"), "{text}");
        assert!(text.contains("uartd/faults"), "{text}");
    }

    /// Perf runs to show and no faults: the missing section reads as "no
    /// dumps" unless the failure is stated on its own line.
    #[test]
    fn list_reports_flags_a_failed_import_beside_the_runs() {
        let (db, dir) = seeded_db();
        seed_fault_dump(dir.path());
        break_the_fault_import(&db);
        let text =
            list_reports(db.url(), dir.path(), 20).unwrap()[0]["text"].as_str().unwrap().to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], RUNS_HEADER, "{text}");
        assert!(lines[1].starts_with("run 7  "), "{text}");
        assert!(lines[2].starts_with("faults: import failed: "), "{text}");
        assert!(!text.contains(FAULTS_HEADER), "{text}");
    }

    #[test]
    fn fetch_fault_unknown_id_names_the_failed_import_too() {
        let db = ggo_db::TestDb::new();
        break_the_fault_import(&db);
        let dir = tempfile::tempdir().unwrap();
        seed_fault_dump(dir.path());
        let err = fetch_fault(db.url(), dir.path(), FAULT_ID).unwrap_err();
        let open_err =
            open_report_cmd(db.url(), dir.path(), None, &json!({ "fault": FAULT_ID })).unwrap_err();
        assert!(err.contains(&format!("no fault {FAULT_ID}")), "{err}");
        assert!(err.contains(DB_NAME), "{err}");
        assert!(err.contains("importing faults from"), "{err}");
        assert!(open_err.contains("importing faults from"), "{open_err}");
    }

    #[test]
    fn fetch_fault_unknown_id_is_a_tool_error() {
        let (db, dir) = seeded_db();
        let err = fetch_fault(db.url(), dir.path(), "nope").unwrap_err();
        assert!(err.contains("no fault nope"), "{err}");
        assert!(err.contains(DB_NAME), "{err}");
    }

    #[test]
    fn open_report_cmd_takes_a_run_or_a_fault_and_needs_one_of_them() {
        let (db, dir) = seeded_db();
        seed_fault_dump(dir.path());
        let cmd =
            open_report_cmd(db.url(), dir.path(), None, &json!({ "fault": FAULT_ID })).unwrap();
        let line = serde_json::to_string(&Request { id: 1, cmd }).unwrap();
        assert!(line.contains(r#""cmd":"open_report""#), "{line}");
        assert!(line.contains(&format!(r#""fault":"{FAULT_ID}""#)), "{line}");

        assert_eq!(
            open_report_cmd(db.url(), dir.path(), None, &json!({ "run": 7 })).unwrap(),
            Cmd::OpenReport { workspace: None, run: Some(7), fault: None }
        );

        let err =
            open_report_cmd(db.url(), dir.path(), None, &json!({ "fault": "nope" })).unwrap_err();
        assert!(err.contains("no fault nope"), "{err}");
        let err = open_report_cmd(db.url(), dir.path(), None, &json!({})).unwrap_err();
        assert!(err.contains("run") && err.contains("fault"), "{err}");
    }

    #[test]
    fn open_report_without_a_run_or_a_fault_never_touches_zed() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str, _: Duration| -> std::io::Result<String> {
            panic!("no socket call without a report to open")
        };
        let (content, is_err) = call_tool("open_ggo_report", &json!({}), dir.path(), &connect);
        assert!(is_err, "{content:?}");
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("run") && text.contains("fault"), "{text}");
    }
}
