//! Tool definitions and execution: each MCP tool resolves a target Zed
//! session from the registry, sends one protocol line over its socket,
//! and shapes the answer into MCP content.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use ggo_emu_remote::protocol::{Cmd, Request, Response};
use ggo_emu_remote::registry::{self, SessionInfo};
use serde_json::{Value, json};

use crate::png::bgra_to_png;

/// One request/response over `socket`. The production connector; tests
/// substitute a fake.
pub type Connector = dyn Fn(&Path, &str) -> std::io::Result<String>;

pub fn socket_call(socket: &Path, line: &str) -> std::io::Result<String> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}

/// The MCP `tools/list` payload.
pub fn tool_list() -> Value {
    let session_props = json!({
        "session": { "type": "number", "description": "Target Zed process id from zed_sessions (optional when only one is live)" },
        "workspace": { "type": "string", "description": "Absolute project-root path (optional when the session has only one workspace)" },
    });
    let with = |extra: Value| -> Value {
        let mut props = session_props.as_object().unwrap().clone();
        if let Some(map) = extra.as_object() {
            props.extend(map.clone());
        }
        json!({ "type": "object", "properties": props })
    };
    json!({ "tools": [
        { "name": "zed_sessions",
          "description": "List running Zed sessions hosting a GGO emulator panel: pid, workspaces, and what each panel is doing.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "emu_boot",
          "description": "Boot a cart (project-relative path, e.g. a .ggo/.cart file) in the Zed emulator panel. Restarts any run in flight.",
          "inputSchema": with(json!({ "cart": { "type": "string" } })) },
        { "name": "emu_input",
          "description": "Latch the held pad buttons (level-triggered, like held keys). Names: z x a s up down left right q w e r t y u i enter select. Empty array releases everything.",
          "inputSchema": with(json!({ "buttons": { "type": "array", "items": { "type": "string" } } })) },
        { "name": "emu_pause", "description": "Pause the run at the next frame boundary.", "inputSchema": with(json!({})) },
        { "name": "emu_resume", "description": "Resume a paused run.", "inputSchema": with(json!({})) },
        { "name": "emu_step",
          "description": "While paused, run exactly N more frames (deterministic frame-stepping; combine with emu_input + emu_screenshot).",
          "inputSchema": with(json!({ "frames": { "type": "number" } })) },
        { "name": "emu_screenshot", "description": "The last presented frame as a PNG image.", "inputSchema": with(json!({})) },
        { "name": "emu_uart",
          "description": "The run's diagnostic log (cart log() output plus run markers). Optionally only the last N lines.",
          "inputSchema": with(json!({ "tail": { "type": "number" } })) },
        { "name": "emu_status", "description": "What the target session's emulator panels are doing.", "inputSchema": with(json!({})) },
        { "name": "emu_stop", "description": "Stop the run.", "inputSchema": with(json!({})) },
    ] })
}

/// Pick the target session: explicit pid, else the only live one.
pub fn resolve_session(sessions: &[SessionInfo], pid: Option<u32>) -> Result<SessionInfo, String> {
    match pid {
        Some(pid) => sessions
            .iter()
            .find(|s| s.pid == pid)
            .cloned()
            .ok_or_else(|| format!("no live zed session with pid {pid}; run zed_sessions")),
        None => match sessions {
            [only] => Ok(only.clone()),
            [] => Err("no live zed session found — is Zed running with a GGO project open?".to_string()),
            many => Err(format!(
                "multiple zed sessions live (pids {:?}) — pass `session`",
                many.iter().map(|s| s.pid).collect::<Vec<_>>()
            )),
        },
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
            let status = send(&s.socket, Cmd::Status, connect)
                .map(|d| d.to_string())
                .unwrap_or_else(|e| format!("(unreachable: {e})"));
            rows.push(format!("pid {} workspaces {:?} status {status}", s.pid, s.workspaces));
        }
        let text = if rows.is_empty() { "no live zed sessions".to_string() } else { rows.join("\n") };
        return Ok(vec![json!({ "type": "text", "text": text })]);
    }

    let session = resolve_session(&sessions, arg_u32(args, "session"))?;
    let workspace = arg_str(args, "workspace");
    let cmd = match name {
        "emu_boot" => Cmd::Boot {
            workspace,
            cart: arg_str(args, "cart").ok_or("missing required argument: cart")?,
        },
        "emu_input" => Cmd::Input {
            workspace,
            buttons: args
                .get("buttons")
                .and_then(|b| b.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        },
        "emu_pause" => Cmd::Pause { workspace },
        "emu_resume" => Cmd::Resume { workspace },
        "emu_step" => Cmd::Step {
            workspace,
            frames: arg_u32(args, "frames").ok_or("missing required argument: frames")?,
        },
        "emu_screenshot" => Cmd::Screenshot { workspace },
        "emu_uart" => Cmd::Uart { workspace, tail: arg_u32(args, "tail").map(|n| n as usize) },
        "emu_status" => Cmd::Status,
        "emu_stop" => Cmd::Stop { workspace },
        other => return Err(format!("unknown tool {other:?}")),
    };
    let data = send(&session.socket, cmd, connect)?;

    if name == "emu_screenshot" {
        use base64::Engine as _;
        let b64 = data
            .get("bgra_base64")
            .and_then(|v| v.as_str())
            .ok_or("host returned no frame data")?;
        let bgra = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| e.to_string())?;
        let (w, h) = (
            data.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            data.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        );
        let png = bgra_to_png(w, h, &bgra)?;
        return Ok(vec![json!({
            "type": "image",
            "mimeType": "image/png",
            "data": base64::engine::general_purpose::STANDARD.encode(&png),
        })]);
    }

    Ok(vec![json!({ "type": "text", "text": data.to_string() })])
}

fn send(socket: &Path, cmd: Cmd, connect: &Connector) -> Result<Value, String> {
    let req = Request { id: 1, cmd };
    let line = serde_json::to_string(&req).expect("Request serializes");
    let reply = connect(socket, &line).map_err(|e| format!("zed session unreachable: {e}"))?;
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
        assert_eq!(resolve_session(&[a.clone(), b.clone()], Some(2)).unwrap().pid, 2);
        assert_eq!(resolve_session(std::slice::from_ref(&a), None).unwrap().pid, 1);
        assert!(resolve_session(&[a, b], None).unwrap_err().contains("multiple"));
        assert!(resolve_session(&[], None).unwrap_err().contains("no live"));
    }

    #[test]
    fn emu_boot_sends_boot_cmd_and_reports_host_data() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"boot""#) && line.contains(r#""cart":"wilds.ggo""#), "{line}");
            Ok(r#"{"id":1,"ok":true,"data":{"booted":true}}"#.to_string())
        };
        let (content, is_err) =
            call_tool("emu_boot", &json!({"cart": "wilds.ggo"}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains("booted"));
    }

    #[test]
    fn host_error_becomes_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str| -> std::io::Result<String> {
            Ok(r#"{"id":1,"ok":false,"error":"no run live"}"#.to_string())
        };
        let (content, is_err) = call_tool("emu_pause", &json!({}), dir.path(), &connect);
        assert!(is_err);
        assert_eq!(content[0]["text"], "no run live");
    }

    #[test]
    fn screenshot_returns_png_image_content() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]); // red pixel
        let reply = format!(r#"{{"id":1,"ok":true,"data":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}"#);
        let connect = move |_: &Path, _: &str| -> std::io::Result<String> { Ok(reply.clone()) };
        let (content, is_err) = call_tool("emu_screenshot", &json!({}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mimeType"], "image/png");
        let png = base64::engine::general_purpose::STANDARD
            .decode(content[0]["data"].as_str().unwrap())
            .unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn missing_required_argument_is_a_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, _: &str| -> std::io::Result<String> { panic!("must not connect") };
        let (content, is_err) = call_tool("emu_boot", &json!({}), dir.path(), &connect);
        assert!(is_err);
        assert!(content[0]["text"].as_str().unwrap().contains("cart"));
    }

    #[test]
    fn tool_list_names_every_tool_once() {
        let list = tool_list();
        let names: Vec<&str> =
            list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "zed_sessions", "emu_boot", "emu_input", "emu_pause", "emu_resume",
                "emu_step", "emu_screenshot", "emu_uart", "emu_status", "emu_stop"
            ]
        );
    }
}
