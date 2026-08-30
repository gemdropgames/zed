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
///
/// Emulation is script-only by design: one `Script` is a whole
/// start -> finish run — boot, frame-scheduled inputs/screenshots/events,
/// automatic stop — answered by one report. There is no interactive
/// drive surface to leave an emulator in a half-driven state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    /// Workspaces this Zed process has, and what each emu panel is doing.
    Status,
    /// Run one complete emulation script and report.
    Script { workspace: Option<String>, script: Script },
}

/// A complete emulation run. Frame numbers are relative to the script's
/// start (frame 0 = the first frame the script steps).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Script {
    /// Worktree-relative cart path (pack one with
    /// `emd pack-ggo [--world <stem>]` first).
    pub cart: String,
    /// Total frames to run before finishing. Capped by the host.
    pub frames: u32,
    /// Scheduled actions, applied in `at` order. A `screenshot` at frame
    /// N captures the framebuffer after N frames have elapsed; an `input`
    /// at frame N is latched before frame N runs.
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Step {
    pub at: u32,
    /// Latch these held buttons from this frame on (empty = release all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    /// Capture a labeled screenshot at this frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
    /// Scripted world event (component insertion/removal, world edits).
    /// Reserved: rejected at validation until the engine grows a mid-run
    /// mutation channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<serde_json::Value>,
}

/// One captured screenshot inside a script report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    pub label: String,
    /// Script-relative frame the capture happened at.
    pub at: u32,
    pub width: u32,
    pub height: u32,
    pub bgra_base64: String,
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
    fn script_request_round_trips_with_flattened_cmd() {
        let line = r#"{"id":7,"cmd":"script","workspace":"/w","script":{"cart":"wilds.ggo","frames":120,"steps":[{"at":0,"input":["right"]},{"at":60,"screenshot":"mid"}]}}"#;
        let req = parse_request(line).unwrap();
        assert_eq!(req.id, 7);
        let Cmd::Script { workspace, script } = &req.cmd else {
            panic!("not a script: {:?}", req.cmd);
        };
        assert_eq!(workspace.as_deref(), Some("/w"));
        assert_eq!(script.cart, "wilds.ggo");
        assert_eq!(script.frames, 120);
        assert_eq!(script.steps[0].input.as_deref(), Some(&["right".to_string()][..]));
        assert_eq!(script.steps[1].screenshot.as_deref(), Some("mid"));
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn script_steps_default_empty_and_status_has_no_workspace() {
        let req = parse_request(r#"{"id":1,"cmd":"script","script":{"cart":"a.ggo","frames":10}}"#).unwrap();
        let Cmd::Script { workspace, script } = req.cmd else { panic!() };
        assert_eq!(workspace, None);
        assert!(script.steps.is_empty());
        assert_eq!(parse_request(r#"{"id":2,"cmd":"status"}"#).unwrap().cmd, Cmd::Status);
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
