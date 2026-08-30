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

use ggo_emu_remote::protocol::{Cmd, Request, Response};
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
        other => Err(format!("unknown tool {other:?}")),
    }
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
        assert_eq!(names, ["zed_sessions", "emu_status", "emu_start", "emu_next_frame", "emu_stop"]);
    }
}
