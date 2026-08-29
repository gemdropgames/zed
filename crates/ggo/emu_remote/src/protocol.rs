//! JSON-lines protocol over the per-session unix socket: one request
//! object per line in, one response object per line out, matched by `id`.

use serde::{Deserialize, Serialize};

/// One request line from the bridge to the Zed-side host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub cmd: Cmd,
}

/// What the bridge can ask a session to do. `workspace` (an absolute
/// project-root path) selects the emu panel when the Zed process has more
/// than one workspace; omitted, the host picks the only one and errors if
/// that is ambiguous.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    /// Workspaces this Zed process has, and what each emu panel is doing.
    Status,
    /// Start `cart` (worktree-relative path) in the workspace's emu panel.
    Boot { workspace: Option<String>, cart: String },
    /// Latch the pad mask (level-triggered; see `emu_panel::input`).
    Input { workspace: Option<String>, buttons: Vec<String> },
    Pause { workspace: Option<String> },
    Resume { workspace: Option<String> },
    /// While paused, run exactly `frames` more frames.
    Step { workspace: Option<String>, frames: u32 },
    /// Latest presented frame as raw BGRA8 (base64), plus dimensions.
    Screenshot { workspace: Option<String> },
    /// Tail of the run's diagnostic log.
    Uart { workspace: Option<String>, tail: Option<usize> },
    Stop { workspace: Option<String> },
}

/// One response line. `data` is command-specific JSON; `error` is set
/// (and `ok` false) on failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(id: u64, data: serde_json::Value) -> Self {
        Self { id, ok: true, error: None, data: Some(data) }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self { id, ok: false, error: Some(error.into()), data: None }
    }
}

/// Per-workspace status payload inside `Cmd::Status`'s response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    /// Absolute project-root path.
    pub workspace: String,
    /// Worktree-relative path of the running cart, if a run is live.
    pub cart: Option<String>,
    pub running: bool,
    pub paused: bool,
    /// Last delivered frame number.
    pub frame: u32,
}

/// Parse one request line. Errors name the parse failure so the host can
/// answer with a protocol-level error instead of dropping the connection.
pub fn parse_request(line: &str) -> Result<Request, String> {
    serde_json::from_str(line).map_err(|e| format!("bad request: {e}"))
}

/// Serialize a response to its wire line (no trailing newline).
pub fn response_line(resp: &Response) -> String {
    serde_json::to_string(resp).expect("Response is always serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_with_flattened_cmd() {
        let line = r#"{"id":7,"cmd":"boot","workspace":"/w","cart":"wilds.ggo"}"#;
        let req = parse_request(line).unwrap();
        assert_eq!(req.id, 7);
        assert_eq!(
            req.cmd,
            Cmd::Boot { workspace: Some("/w".to_string()), cart: "wilds.ggo".to_string() }
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn optional_workspace_defaults_to_none() {
        let req = parse_request(r#"{"id":1,"cmd":"screenshot"}"#).unwrap();
        assert_eq!(req.cmd, Cmd::Screenshot { workspace: None });
    }

    #[test]
    fn step_carries_frames() {
        let req = parse_request(r#"{"id":2,"cmd":"step","frames":10}"#).unwrap();
        assert_eq!(req.cmd, Cmd::Step { workspace: None, frames: 10 });
    }

    #[test]
    fn bad_json_is_a_named_error_not_a_panic() {
        let err = parse_request("not json").unwrap_err();
        assert!(err.starts_with("bad request:"), "{err}");
    }

    #[test]
    fn error_response_omits_data_and_ok_response_omits_error() {
        let e = response_line(&Response::err(3, "no such workspace"));
        assert!(e.contains(r#""error":"no such workspace""#) && !e.contains("data"));
        let o = response_line(&Response::ok(4, serde_json::json!({"frame": 9})));
        assert!(o.contains(r#""frame":9"#) && !o.contains("error"));
    }
}
