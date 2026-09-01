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
    /// Start a hardware flash; see [`FlashConfig`] for the knobs and
    /// their defaults.
    FlashWorld {
        workspace: Option<String>,
        #[serde(flatten)]
        config: FlashConfig,
    },
    /// Snapshot of the current/last flash.
    FlashStatus { workspace: Option<String> },
    /// Open the Reports tab on perf run `run` (an id in ~/.ggo/ggo_ide.db).
    OpenReport { workspace: Option<String>, run: i64 },
    /// Close the Reports tab. With `run`, only if that is the run it shows.
    CloseReport { workspace: Option<String>, run: Option<i64> },
}

/// What a flash runs with. Every field has a default, so `{}` is a
/// complete configuration: the project's own world, the cached
/// bitstream, the first serial port found, ggo-diag's own baud and
/// capture window.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FlashConfig {
    /// World stem baked in as the boot world, e.g. "worlds/chase_cam".
    /// None = the project's `default_world` (or what the panel last
    /// remembered).
    #[serde(default)]
    pub world: Option<String>,
    /// Full place-and-route (~20 min) instead of the cached bitstream.
    #[serde(default)]
    pub rebuild_gateware: bool,
    /// Serial device; None = the first port the panel's scan found.
    #[serde(default)]
    pub tty: Option<String>,
    /// UART baud; None = ggo-diag's default ([`DEFAULT_BAUD`]).
    #[serde(default)]
    pub baud: Option<u32>,
    /// How long the gameplay telemetry capture holds after boot before
    /// the run ends on its own; None = ggo-diag's default
    /// ([`DEFAULT_COLLECT_SECONDS`]).
    #[serde(default)]
    pub collect_seconds: Option<u64>,
    /// Build GemOS with `--features telemetry` forced on (`--telemetry`).
    /// Firmware defaults it on already; this only matters for a firmware
    /// build that turned it off.
    #[serde(default)]
    pub telemetry: bool,
}

/// ggo-diag's `--baud` default. Mirrored here (not read from the CLI)
/// so an effective configuration can be reported before the child runs;
/// when the caller leaves the field unset the flag is NOT passed, so
/// ggo-diag's own default still rules.
pub const DEFAULT_BAUD: u32 = 115_200;
/// ggo-diag's `--collect-seconds` default; same rule as [`DEFAULT_BAUD`].
pub const DEFAULT_COLLECT_SECONDS: u64 = 120;

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
    /// What is being run, e.g. "flashing worlds/chase_cam".
    #[serde(default)]
    pub what: Option<String>,
    /// Seconds since the run began (final total once it ended).
    #[serde(default)]
    pub elapsed_s: Option<u64>,
    /// The running phase's sub-line: which component is placing, which
    /// boot stage is up and the next stage's budget, which diag step runs.
    #[serde(default)]
    pub detail: Option<String>,
    /// Every phase of the run in order, pre-seeded with the ones still
    /// to come -- "how much is left" as well as "where are we".
    #[serde(default)]
    pub phases: Vec<FlashPhase>,
    /// Diagnostic-cart steps, latest status each.
    #[serde(default)]
    pub diag_steps: Vec<FlashDiagStep>,
    /// Why the run failed, in ggo-diag's own words (the last line that
    /// is not a progress banner). Only for a FAIL.
    #[serde(default)]
    pub failure: Option<String>,
    /// The run's full transcript on disk (`~/.zed/logs/ggo-run-*.log`),
    /// for when `console_tail` is not enough.
    #[serde(default)]
    pub transcript: Option<String>,
    /// The newest console lines, oldest first.
    #[serde(default)]
    pub console_tail: Vec<String>,
}

/// One phase of a flash, as [`FlashStatusPayload::phases`] reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlashPhase {
    pub title: String,
    /// "pending" | "running" | "done" | "failed".
    pub state: String,
    /// How long the phase has taken so far (or took).
    pub elapsed_s: u64,
    /// The phase's newest sub-line, if it printed one.
    #[serde(default)]
    pub detail: Option<String>,
}

/// One diagnostic-cart step: `diag step <index>: <status>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlashDiagStep {
    pub index: String,
    /// "running" | "PASS" | "FAIL" | "info".
    pub status: String,
}

impl FlashStatusPayload {
    /// The "never ran" payload: every field but `active` absent.
    pub fn idle() -> Self {
        Self {
            active: false,
            phase: None,
            verdict: None,
            diag_run_id: None,
            perf_run_id: None,
            what: None,
            elapsed_s: None,
            detail: None,
            phases: Vec::new(),
            diag_steps: Vec::new(),
            failure: None,
            transcript: None,
            console_tail: Vec::new(),
        }
    }
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
                config: FlashConfig { world: Some("worlds/chase_cam".to_string()), ..Default::default() },
            }
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        // Omitted is the safe half of the pair: a place-and-route is
        // twenty minutes, and no caller gets one by forgetting a field.
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"flash_world"}"#).unwrap().cmd,
            Cmd::FlashWorld { workspace: None, config: FlashConfig::default() }
        );
        // The knobs ride flat beside the command, as the bridge sends them.
        let req = parse_request(
            r#"{"id":4,"cmd":"flash_world","workspace":"/w","tty":"/dev/ttyUSB1","baud":9600,"collect_seconds":30,"telemetry":true}"#,
        )
        .unwrap();
        assert_eq!(
            req.cmd,
            Cmd::FlashWorld {
                workspace: Some("/w".to_string()),
                config: FlashConfig {
                    world: None,
                    rebuild_gateware: false,
                    tty: Some("/dev/ttyUSB1".to_string()),
                    baud: Some(9600),
                    collect_seconds: Some(30),
                    telemetry: true,
                },
            }
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
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
            active: true,
            phase: Some("Boot verify (UART)".to_string()),
            verdict: None,
            diag_run_id: Some("20260831T120000Z-abc123def0".to_string()),
            perf_run_id: None,
            what: Some("flashing worlds/chase_cam".to_string()),
            elapsed_s: Some(95),
            detail: Some("boot: SD ready — next: FAT32 mounted (10s budget)".to_string()),
            phases: vec![
                FlashPhase { title: "Flash board".into(), state: "done".into(), elapsed_s: 12, detail: None },
                FlashPhase {
                    title: "Boot verify (UART)".into(),
                    state: "running".into(),
                    elapsed_s: 4,
                    detail: Some("boot: SD ready — next: FAT32 mounted (10s budget)".into()),
                },
                FlashPhase { title: "Report".into(), state: "pending".into(), elapsed_s: 0, detail: None },
            ],
            diag_steps: vec![FlashDiagStep { index: "1".into(), status: "PASS".into() }],
            failure: None,
            transcript: Some("/home/x/.zed/logs/ggo-run-20260901-132908-flashing.log".into()),
            console_tail: vec!["  [boot] SD ready — next: FAT32 mounted (10s budget)".into()],
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""elapsed_s":95"#), "{json}");
        assert!(json.contains(r#""phases":[{"title":"Flash board","state":"done","elapsed_s":12,"detail":null}"#), "{json}");
        assert!(json.contains(r#""diag_steps":[{"index":"1","status":"PASS"}]"#), "{json}");
        let back: FlashStatusPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
    }

    /// A host that predates the context fields still parses: the bridge
    /// and the Zed build are installed separately.
    #[test]
    fn flash_status_payload_without_context_fields_is_the_idle_shape() {
        let old = r#"{"active":false,"phase":null,"verdict":null,"diag_run_id":null,"perf_run_id":null}"#;
        let back: FlashStatusPayload = serde_json::from_str(old).unwrap();
        assert_eq!(back, FlashStatusPayload::idle());
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
