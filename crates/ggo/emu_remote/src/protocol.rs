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
/// Emulation is LOCK-STEP: `Start` boots a cart and pauses at a frame
/// boundary; each `NextFrame` latches the pad and runs exactly one frame;
/// every reply carries the cart's own world-inspection JSON (worlds that
/// declare `InspectWorld` — see emerald-world's `inspect`), so a script,
/// an AI, or any other caller can play the emulator frame by frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    /// Workspaces this Zed process has, and what each emu panel is doing.
    Status,
    /// Boot `cart` (worktree-relative; pack first with
    /// `emd pack-ggo [--world <stem>]`), pause at the first frame
    /// boundary, and report the initial world state.
    Start { workspace: Option<String>, cart: String },
    /// Latch `buttons` as the held pad (empty releases all), run exactly
    /// one frame, and report the new world state. `screenshot` also
    /// returns the presented framebuffer.
    NextFrame {
        workspace: Option<String>,
        #[serde(default)]
        buttons: Vec<String>,
        #[serde(default)]
        screenshot: bool,
    },
    /// End the run; the reply carries the cart's uart log.
    Stop { workspace: Option<String> },
    /// Start a hardware flash of `world` (stem, e.g. "worlds/chase_cam";
    /// None = the project's default world). `rebuild_gateware` runs full
    /// place-and-route (~20 min) instead of the cached bitstream.
    FlashWorld {
        workspace: Option<String>,
        world: Option<String>,
        #[serde(default)]
        rebuild_gateware: bool,
    },
    /// Snapshot of the current/last flash.
    FlashStatus { workspace: Option<String> },
    /// Open the Reports tab on perf run `run` (an id in ~/.ggo/ggo_ide.db).
    OpenReport { workspace: Option<String>, run: i64 },
    /// Close the Reports tab. With `run`, only if that is the run it shows.
    CloseReport { workspace: Option<String>, run: Option<i64> },
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

/// `Cmd::FlashStatus`'s response payload: what the board run in flight
/// (else the last one to end) reached. A flash outlives any single tool
/// call -- a cached-gateware flash is minutes, a rebuild is twenty -- so
/// the caller polls this rather than waiting on `FlashWorld`'s reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlashStatusPayload {
    /// A flash is currently running.
    pub active: bool,
    /// Current (or final) phase label, e.g. "Boot verify (UART)".
    pub phase: Option<String>,
    /// Some(true)=PASS, Some(false)=FAIL, None=still running / never ran.
    pub verdict: Option<bool>,
    /// ggo-diag's TEXT run id, once its `[db] run …` line streamed by.
    pub diag_run_id: Option<String>,
    /// The cloned perf run id in ~/.ggo/ggo_ide.db, once the run passed
    /// and the clone resolved (same id the reports page opens).
    pub perf_run_id: Option<i64>,
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
    fn lockstep_requests_round_trip_with_flattened_cmd() {
        let req = parse_request(r#"{"id":7,"cmd":"start","workspace":"/w","cart":"wilds.ggo"}"#).unwrap();
        assert_eq!(
            req.cmd,
            Cmd::Start { workspace: Some("/w".to_string()), cart: "wilds.ggo".to_string() }
        );
        let req = parse_request(r#"{"id":8,"cmd":"next_frame","buttons":["right","z"],"screenshot":true}"#).unwrap();
        assert_eq!(
            req.cmd,
            Cmd::NextFrame {
                workspace: None,
                buttons: vec!["right".to_string(), "z".to_string()],
                screenshot: true,
            }
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn next_frame_defaults_release_all_and_no_screenshot() {
        let req = parse_request(r#"{"id":1,"cmd":"next_frame"}"#).unwrap();
        assert_eq!(req.cmd, Cmd::NextFrame { workspace: None, buttons: vec![], screenshot: false });
        assert_eq!(parse_request(r#"{"id":2,"cmd":"status"}"#).unwrap().cmd, Cmd::Status);
    }

    #[test]
    fn flash_requests_round_trip_and_default_to_the_cached_gateware() {
        let req = parse_request(
            r#"{"id":1,"cmd":"flash_world","world":"worlds/chase_cam","rebuild_gateware":false}"#,
        )
        .unwrap();
        assert_eq!(
            req.cmd,
            Cmd::FlashWorld {
                workspace: None,
                world: Some("worlds/chase_cam".to_string()),
                rebuild_gateware: false,
            }
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        // Omitted is the safe half of the pair: a place-and-route is
        // twenty minutes, and no caller gets one by forgetting a field.
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"flash_world"}"#).unwrap().cmd,
            Cmd::FlashWorld { workspace: None, world: None, rebuild_gateware: false }
        );
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":"flash_status"}"#).unwrap().cmd,
            Cmd::FlashStatus { workspace: None }
        );
        assert_eq!(
            parse_request(r#"{"id":3,"cmd":"flash_status","workspace":"/w"}"#).unwrap().cmd,
            Cmd::FlashStatus { workspace: Some("/w".to_string()) }
        );
    }

    #[test]
    fn report_requests_round_trip() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"open_report","run":55}"#).unwrap().cmd,
            Cmd::OpenReport { workspace: None, run: 55 }
        );
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":"close_report"}"#).unwrap().cmd,
            Cmd::CloseReport { workspace: None, run: None }
        );
        assert_eq!(
            parse_request(r#"{"id":3,"cmd":"close_report","workspace":"/w","run":55}"#).unwrap().cmd,
            Cmd::CloseReport { workspace: Some("/w".to_string()), run: Some(55) }
        );
    }

    #[test]
    fn flash_status_payload_is_snake_case_on_the_wire() {
        let payload = FlashStatusPayload {
            active: false,
            phase: Some("Boot verify (UART)".to_string()),
            verdict: Some(true),
            diag_run_id: Some("20260831T120000Z-abc123def0".to_string()),
            perf_run_id: Some(12),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert_eq!(
            json,
            r#"{"active":false,"phase":"Boot verify (UART)","verdict":true,"diag_run_id":"20260831T120000Z-abc123def0","perf_run_id":12}"#
        );
        let back: FlashStatusPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
        // Never ran: every field but `active` is absent-as-null.
        let idle = serde_json::to_string(&FlashStatusPayload {
            active: false,
            phase: None,
            verdict: None,
            diag_run_id: None,
            perf_run_id: None,
        })
        .unwrap();
        assert_eq!(
            idle,
            r#"{"active":false,"phase":null,"verdict":null,"diag_run_id":null,"perf_run_id":null}"#
        );
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
