//! The Zed fork's one way to reach GemdropGo.
//!
//! Every GGO capability this editor uses -- the emulator, the reports
//! database, the UART relay and its fault dumps, hardware diagnostics,
//! packing, sprite repair -- lives behind the `ggo` daemon
//! (`tools/ggo-daemon`, crate `ggo-daemon`). This crate speaks to it over
//! the daemon's unix socket, and it is the **only** crate under
//! `crates/ggo/` permitted a dependency on a `ggo-*` crate. See
//! `docs/daemon-api-plan.md` in the GGO repo for the full design; §5 is
//! this crate's remit and §6 the phases.
//!
//! Why a daemon at all: the fork used to link `ggo-emu-core`, open the
//! PostgreSQL database directly and read `~/.ggo/uartd/faults` off the
//! filesystem. That made the editor a second, independent host
//! implementation that had to agree with the real one and silently drifted
//! when it did not, while two processes competed for the same database,
//! serial port and dump directory.
//!
//! # Blocking, deliberately
//!
//! Every call here blocks. Callers run them inside `cx.background_spawn`,
//! never on the UI thread -- the same rule `ggo_common::run_capture` and
//! `ggo_charts_panel::loader`'s database reads already follow. Blocking is
//! what lets [`Transport`] stay a plain `Fn` with a one-line fake, which is
//! what keeps panel tests hermetic.
//!
//! This crate deliberately has no timeout of its own and no `gpui`
//! dependency: the only clock a GGO panel may race against is
//! `gpui::BackgroundExecutor::timer` (this checkout's `clippy.toml`
//! disallows `smol::Timer::after` outright), and an executor is exactly
//! what a plain synchronous helper does not have. A caller that needs a
//! budget imposes it by racing its own task against an executor timer.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

/// Env var naming a non-default daemon socket. Same convention as
/// `ggo_common`'s `GGO_EMD` / `GGO_EMU` binary overrides, for the same
/// reason: this fork has no settings surface, so an env var is the whole
/// story.
pub const SOCKET_ENV: &str = "GGO_DAEMON_SOCK";

/// Env var naming a non-default `ggo` binary to autostart.
pub const BIN_ENV: &str = "GGO_BIN";

/// Bare-name fallback, resolved against `PATH`. The only GemdropGo host
/// binary is `ggo` (crate `ggo-daemon`); every former standalone tool is a
/// subcommand of it.
pub const DEFAULT_BIN: &str = "ggo";

/// The subcommand that runs the daemon.
pub const SERVE_MODE_ARG: &str = "serve";

/// Socket path relative to `$HOME`, as `ggo serve` lays it out (the
/// daemon's `mcp::socket_path`).
const DEFAULT_SOCKET_REL: &str = ".ggo/ggo-daemon.sock";

/// MCP protocol version this client speaks. Sent in `initialize`; the
/// daemon echoes the client's version back, so a mismatch is visible in
/// [`Handshake`] rather than silently assumed.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Client name reported to the daemon, so `ggo serve`'s log says which
/// process connected.
const CLIENT_NAME: &str = "zed-ggo";

/// The socket to connect to: [`SOCKET_ENV`] when set and non-blank, else
/// `$HOME/.ggo/ggo-daemon.sock`.
///
/// Blank is treated as unset (the same filter `ggo_common::resolve_bin`
/// applies) so an accidentally-empty export does not become a connect to
/// `""`.
pub fn default_socket_path() -> Result<PathBuf> {
    if let Some(configured) = std::env::var(SOCKET_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(PathBuf::from(configured));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME is not set; set {SOCKET_ENV} to the daemon's socket"))?;
    Ok(PathBuf::from(home).join(DEFAULT_SOCKET_REL))
}

/// The `ggo` binary to autostart: [`BIN_ENV`] when set and non-blank, else
/// [`DEFAULT_BIN`].
pub fn daemon_bin() -> String {
    std::env::var(BIN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BIN.to_string())
}

/// What the daemon said about itself when the connection opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub server_name: String,
    pub server_version: String,
    /// The protocol version the daemon agreed to. Compared against
    /// [`PROTOCOL_VERSION`] by [`Client::connect`], which fails naming both
    /// when they differ -- two independently built processes sharing a
    /// wire format is exactly the case where "it mostly works" is worse
    /// than a clear refusal.
    pub protocol_version: String,
}

/// A daemon job's state, as `ggo_web_types::JobState` serialises it.
///
/// Declared here rather than imported: this crate must build without a
/// dependency on the GGO tree for everything P0 does, and the tag/rename
/// attributes below ARE the wire contract -- a mismatch would show as a
/// decode failure the moment a job finishes, which is the case the
/// `a_finished_job_decodes_its_exit_code` test pins.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    Running,
    Done { exit_code: i32 },
    Failed { error: String },
}

impl JobState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// Did this job end badly? A non-zero exit is the tool's own verdict
    /// on itself, which the hardware page styles differently from a
    /// spawn that never produced one.
    pub fn is_error(&self) -> bool {
        match self {
            Self::Running => false,
            Self::Done { exit_code } => *exit_code != 0,
            Self::Failed { .. } => true,
        }
    }
}

/// One job, as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobInfo {
    pub id: u64,
    pub args: Vec<String>,
    #[serde(flatten)]
    pub state: JobState,
    pub line_count: usize,
}

/// One line of a job's output. `index` is monotonic across the whole run,
/// so a gap in it means lines were evicted from the daemon's buffer --
/// the count kept counting even though the text is gone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobLine {
    pub index: usize,
    pub text: String,
}

/// One poll's worth of a job: what is new, and how the job is doing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobLines {
    pub lines: Vec<JobLine>,
    #[serde(flatten)]
    pub state: JobState,
}

impl JobLines {
    /// The `since` to pass to the next [`Client::job_lines`] call.
    ///
    /// One past the highest index seen, or the caller's own `since` when
    /// the batch was empty -- a caught-up poller must not rewind to 0 and
    /// replay the whole transcript.
    pub fn next_since(&self, current: usize) -> usize {
        self.lines
            .last()
            .map(|line| line.index + 1)
            .unwrap_or(current)
    }
}

// ------------------------------------------------------------- reports
//
// The row types are worldlib's own, re-exported rather than mirrored.
//
// This crate may link a `ggo-*` crate -- it is the one crate under
// `crates/ggo/` allowed to. What P1 removes is the DATABASE, not the
// shapes, and `ggo-worldlib` now keeps its queries behind a `db` feature
// this crate deliberately leaves off: the types come across, sqlx and the
// connection pool do not.
//
// Re-exporting rather than duplicating means there is no second
// definition to drift. The serde field names are the wire contract
// between two independently built processes, and now both processes name
// the same struct.

pub use ggo_worldlib::charts::reports::rows::{
    CartRow, FrameRow, ProfileRow, RunDetail, RunIndexRow, RunRow, UartLine,
};

// The fault types are pure dump-parsing shapes, so they come across the
// same way: only `faults`' import/list/load touch a database, and those
// are behind worldlib's `db` feature.
pub use ggo_worldlib::charts::reports::faults::{FaultDetail, FaultRow};
pub use ggo_worldlib::charts::reports::uart_diag::{AssetFailure, PanicRow};

/// One device (`ggo-diag`) run, as `diag_db::RunSummary` serialises it.
///
/// Declared here rather than re-exported: `diag_db` is one of the modules
/// behind worldlib's `db` feature, because every function in it issues
/// SQL. Only the row shape is needed on this side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagRunSummary {
    pub id: String,
    pub started_at: String,
    pub state: String,
    /// `None` until the run reaches a verdict.
    pub verdict: Option<String>,
}

/// What a completed ingest created -- `ingest::RunId`, which lives behind
/// the same `db` feature and for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngestedRun {
    pub run_id: i64,
    pub cart_id: i64,
    /// `Some(original frame count)` when the run was longer than the cap
    /// and its tail was dropped. The run IS stored -- this is not an error.
    pub truncated_frames: Option<usize>,
}

/// The injection seam: anything that can carry one JSON-RPC request line
/// and return one response line.
///
/// A boxed `Fn` rather than a trait for the same reason
/// `ggo_common::ProcRunner` is one -- every implementation is a single
/// function, and a test's fake is a closure. `Send + Sync` because calls
/// happen on `cx.background_spawn`'s thread, never on the UI thread.
pub type Transport = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

/// How a panel obtains a client.
///
/// Injected rather than called directly so a test can hand back a
/// [`FakeDaemon`]-backed client instead of needing a daemon installed and
/// running on the machine -- the same reason [`Transport`] is a seam. Lives
/// here rather than in any one panel because every panel that reaches the
/// daemon needs it.
pub type Connect = Arc<dyn Fn() -> Result<Arc<Client>> + Send + Sync>;

/// Connect to the daemon named by the environment, starting it if needed.
pub fn system_connect() -> Connect {
    Arc::new(|| Client::connect().map(Arc::new))
}

/// A connected daemon.
///
/// Holds the transport and the request-id counter. Cheap to clone-share
/// behind an `Arc`; the transport itself serialises concurrent calls.
pub struct Client {
    transport: Transport,
    handshake: Handshake,
    next_id: AtomicU64,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("handshake", &self.handshake)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connect to the daemon at [`default_socket_path`], starting it if it
    /// is not running, then handshake.
    pub fn connect() -> Result<Self> {
        let socket = default_socket_path()?;
        Self::connect_at(&socket)
    }

    /// [`Self::connect`] against an explicit socket path.
    pub fn connect_at(socket: &std::path::Path) -> Result<Self> {
        let transport = unix_transport(socket)?;
        Self::with_transport(transport)
    }

    /// Build a client over an already-established transport, performing the
    /// handshake. The seam tests use.
    pub fn with_transport(transport: Transport) -> Result<Self> {
        let client = Self {
            transport,
            handshake: Handshake {
                server_name: String::new(),
                server_version: String::new(),
                protocol_version: String::new(),
            },
            next_id: AtomicU64::new(1),
        };
        let result = client.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;

        let handshake = Handshake {
            server_name: text_at(&result, &["serverInfo", "name"]),
            server_version: text_at(&result, &["serverInfo", "version"]),
            protocol_version: text_at(&result, &["protocolVersion"]),
        };
        if handshake.protocol_version != PROTOCOL_VERSION {
            bail!(
                "GemdropGo daemon protocol mismatch: this build of Zed speaks \
                 {PROTOCOL_VERSION}, the daemon ({} {}) speaks {} -- rebuild both \
                 from the same checkout",
                handshake.server_name,
                handshake.server_version,
                handshake.protocol_version,
            );
        }
        Ok(Self {
            handshake,
            ..client
        })
    }

    pub fn handshake(&self) -> &Handshake {
        &self.handshake
    }

    /// One JSON-RPC call. Returns the `result` member, or the daemon's
    /// error message.
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .with_context(|| format!("encode {method} request"))?;

        let response = (self.transport)(&line).with_context(|| format!("daemon {method}"))?;
        let response: Value = serde_json::from_str(response.trim())
            .with_context(|| format!("decode {method} response: {response}"))?;

        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            bail!("GemdropGo daemon rejected {method}: {message}");
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("daemon {method} reply had no result: {response}"))
    }

    /// Call one MCP tool and return its decoded JSON payload.
    ///
    /// The daemon encodes a tool's result as MCP text content whose text
    /// happens to be JSON (`mcp::tool_text`), so the text is parsed back
    /// rather than handed to a caller as a string. A tool that legitimately
    /// returns prose comes back as a JSON string, which is still valid
    /// JSON, so no caller has to care which kind it asked for.
    pub fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}))?;

        // `isError` is how MCP reports a tool's own failure; the JSON-RPC
        // layer above was perfectly happy, so this must be checked
        // separately or a failed diag run reads as a success.
        let failed = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|content| content.first())
            .and_then(|entry| entry.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        if failed {
            bail!("GemdropGo {name} failed: {text}");
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        // A tool whose text is not JSON is still useful as a string; the
        // alternative is failing a caller that only wanted to show it.
        Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())))
    }

    /// Tool names the daemon advertises. The capability probe a panel uses
    /// to tell "daemon too old" from "call went wrong".
    pub fn tool_names(&self) -> Result<Vec<String>> {
        let result = self.request("tools/list", json!({}))?;
        Ok(result
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    // ------------------------------------------------- P0 tool wrappers
    //
    // One method per capability the fork used to reach by spawning `ggo`
    // as a child process. Thin on purpose: the argument shaping belongs
    // here, the policy stays in the panels.

    /// Run a hardware diagnostic (`ggo diag`) and block until it ends.
    ///
    /// Only for runs that finish quickly (`--help`, a probe). A flash is
    /// minutes of work and must use [`Self::diag_start`] instead, or the
    /// panel shows nothing until it is over.
    pub fn diag(&self, args: Vec<String>) -> Result<Value> {
        self.call_tool("ggo_diag", json!({"args": args}))
    }

    /// Start a diagnostic as a daemon job; returns its [`JobInfo`] at once.
    ///
    /// Follow it with [`Self::job_lines`]. The daemon runs one diag at a
    /// time -- two would fight over the board and its UART -- so this
    /// fails while another is in flight, and the message names the job.
    pub fn diag_start(&self, args: Vec<String>) -> Result<JobInfo> {
        let value = self.call_tool("ggo_diag_start", json!({"args": args}))?;
        serde_json::from_value(value).context("decode the started job")
    }

    /// Lines from `since` onward, with the job's state.
    ///
    /// The socket carries no server-initiated frames, so following a job
    /// means asking again from the last index seen. Pass
    /// `next_since` back as `since`; a caught-up poll returns no lines,
    /// which is not an error.
    pub fn job_lines(&self, id: u64, since: usize) -> Result<JobLines> {
        let value = self.call_tool("ggo_job_lines", json!({"id": id, "since": since}))?;
        serde_json::from_value(value).context("decode the job's lines")
    }

    /// SIGINT a running job. The reply says whether there was one.
    pub fn job_cancel(&self, id: u64) -> Result<bool> {
        let value = self.call_tool("ggo_job_cancel", json!({"id": id}))?;
        Ok(value
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// Every job this daemon has run.
    pub fn jobs(&self) -> Result<Vec<JobInfo>> {
        let value = self.call_tool("ggo_jobs", json!({}))?;
        serde_json::from_value(value).context("decode the job list")
    }

    // --------------------------------------------------------- reports
    //
    // The read side of the GemdropGo database. Every `SELECT` lives in
    // the daemon (in `ggo_worldlib::charts::reports`); these only carry
    // the answer back, so the editor never opens a pool of its own.

    /// Every perf run across every cart, newest first.
    pub fn run_index(&self) -> Result<Vec<RunIndexRow>> {
        let value = self.call_tool("ggo_run_index", json!({}))?;
        serde_json::from_value(value).context("decode the run index")
    }

    /// Every cart, with its run count and newest run stamp.
    pub fn carts(&self) -> Result<Vec<CartRow>> {
        let value = self.call_tool("ggo_carts", json!({}))?;
        serde_json::from_value(value).context("decode the cart list")
    }

    /// Every run of one cart, newest first, with frame aggregates.
    pub fn cart_runs(&self, cart_id: i64) -> Result<Vec<RunRow>> {
        let value = self.call_tool("ggo_cart_runs", json!({"cart_id": cart_id}))?;
        serde_json::from_value(value).context("decode the cart's runs")
    }

    /// One run's full detail, or `None` when there is no such run.
    ///
    /// Absence is not an error: a `run` row can vanish between listing and
    /// selecting, and the report header falls back to the picker's own
    /// listing rather than failing the whole view.
    pub fn run_detail(&self, run_id: i64) -> Result<Option<RunDetail>> {
        let value = self.call_tool("ggo_run_detail", json!({"run_id": run_id}))?;
        serde_json::from_value(value).context("decode the run detail")
    }

    /// One run's per-frame series -- the charts panel's bulk read.
    pub fn run_frames(&self, run_id: i64) -> Result<Vec<FrameRow>> {
        let value = self.call_tool("ggo_run_frames", json!({"run_id": run_id}))?;
        serde_json::from_value(value).context("decode the run's frames")
    }

    /// One run's UART lines.
    pub fn run_uart(&self, run_id: i64) -> Result<Vec<String>> {
        let value = self.call_tool("ggo_run_uart", json!({"run_id": run_id}))?;
        serde_json::from_value(value).context("decode the run's uart")
    }

    /// One run's cache-profile rows.
    pub fn run_profile(&self, run_id: i64) -> Result<Vec<ProfileRow>> {
        let value = self.call_tool("ggo_run_profile", json!({"run_id": run_id}))?;
        serde_json::from_value(value).context("decode the run's profile")
    }

    /// Device (`ggo-diag`) runs, newest first. `limit` omitted takes the
    /// daemon's own default.
    pub fn diag_runs(&self, limit: Option<i64>) -> Result<Vec<DiagRunSummary>> {
        let arguments = match limit {
            Some(limit) => json!({"limit": limit}),
            None => json!({}),
        };
        let value = self.call_tool("ggo_diag_runs", arguments)?;
        serde_json::from_value(value).context("decode the device runs")
    }

    /// One device run's pipeline narration (`run_log`), in `seq` order.
    pub fn diag_run_log(&self, run_id: &str) -> Result<Vec<String>> {
        let value = self.call_tool("ggo_diag_run_log", json!({"run_id": run_id}))?;
        serde_json::from_value(value).context("decode the device run's log")
    }

    /// The perf run holding a device run's telemetry, if it has one.
    ///
    /// `None` covers both "that run has no perf telemetry" and "there is
    /// no such device run" -- indistinguishable to every caller, which
    /// asks this only to decide whether a report exists to open.
    pub fn diag_perf_run_id(&self, run_id: &str) -> Result<Option<i64>> {
        let value = self.call_tool("ggo_diag_perf_run", json!({"run_id": run_id}))?;
        Ok(value.get("perf_run_id").and_then(Value::as_i64))
    }

    /// Write one finished run: its perf JSON verbatim, its UART lines, and
    /// an optional `run.label`.
    ///
    /// `perf_json` is the exact text the run emitted, passed through
    /// rather than re-encoded, so the daemon validates the same bytes.
    pub fn ingest_run(
        &self,
        perf_json: &str,
        uart: &[String],
        label: Option<&str>,
    ) -> Result<IngestedRun> {
        let mut arguments = json!({"perf_json": perf_json, "uart": uart});
        if let Some(label) = label {
            arguments["label"] = json!(label);
        }
        let value = self.call_tool("ggo_ingest_run", arguments)?;
        serde_json::from_value(value).context("decode the ingested run")
    }

    /// Apply pending migrations to the GemdropGo database.
    pub fn db_migrate(&self) -> Result<Value> {
        self.call_tool("ggo_db_migrate", json!({}))
    }

    /// The UART reader's state inside the daemon.
    pub fn uart_status(&self) -> Result<Value> {
        self.call_tool("ggo_uart_status", json!({}))
    }

    /// Write the physical UART ring buffer to a fault dump.
    pub fn uart_dump(&self, reason: &str) -> Result<Value> {
        self.call_tool("ggo_uart_dump", json!({"reason": reason}))
    }

    /// Inspect or repair Emerald `.spr` sidecar paths.
    pub fn repair_sprites(&self, path: &str, write: bool) -> Result<Value> {
        self.call_tool("ggo_repair_sprites", json!({"path": path, "write": write}))
    }

    /// Cartridge flashing availability.
    pub fn flash_status(&self) -> Result<Value> {
        self.call_tool("ggo_flash", json!({}))
    }

    // ---------------------------------------------------------- faults

    /// Every stored fault dump, newest first.
    ///
    /// Fresh dumps on disk are imported by the daemon on the way, so this
    /// is never stale -- and the editor never touches `~/.ggo` itself.
    /// `limit` omitted takes the daemon's own default.
    pub fn faults(&self, limit: Option<i64>) -> Result<Vec<FaultRow>> {
        let arguments = match limit {
            Some(limit) => json!({"limit": limit}),
            None => json!({}),
        };
        let value = self.call_tool("ggo_faults", arguments)?;
        serde_json::from_value(value).context("decode the fault list")
    }

    /// One fault in full, or `None` when the daemon has already pruned it.
    ///
    /// Absence is not an error: dumps are pruned, so a row a panel saw a
    /// moment ago can be gone, and that is a state to render.
    pub fn fault(&self, id: &str) -> Result<Option<FaultDetail>> {
        let value = self.call_tool("ggo_fault", json!({"id": id}))?;
        serde_json::from_value(value).context("decode the fault")
    }

    /// Where one fault's raw dump file lives, for showing or opening.
    pub fn fault_raw_path(&self, id: &str) -> Result<PathBuf> {
        let value = self.call_tool("ggo_fault_raw_path", json!({"id": id}))?;
        Ok(PathBuf::from(
            value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ))
    }
}

/// Look a string up by path, defaulting to empty -- a handshake field the
/// daemon omitted must not panic a connect.
fn text_at(value: &Value, path: &[&str]) -> String {
    let mut current = value;
    for key in path {
        match current.get(key) {
            Some(next) => current = next,
            None => return String::new(),
        }
    }
    current.as_str().unwrap_or_default().to_string()
}

/// Attempts to connect before giving up, once the daemon has been started.
/// Bounded rather than open-ended: a daemon that never comes up is a
/// failure to report, not something to wait on forever.
const CONNECT_ATTEMPTS: usize = 50;

/// Pause between connect attempts. `std::thread::sleep`, not
/// `smol::Timer::after` (disallowed by this checkout's `clippy.toml`) --
/// correct here precisely because this whole crate is called from
/// `cx.background_spawn`.
const CONNECT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// The real transport: one connect per request, to the daemon's socket.
///
/// A connection per call rather than one held open, deliberately: the
/// daemon's socket loop serves one request per line and a held connection
/// would have to be mutex-guarded against concurrent panels anyway. P4's
/// notification stream needs a persistent connection and will add one
/// beside this; P0's request/response calls do not.
pub fn unix_transport(socket: &std::path::Path) -> Result<Transport> {
    ensure_running(socket)?;
    let socket = socket.to_path_buf();
    Ok(Arc::new(move |line: &str| round_trip(&socket, line)))
}

/// Send one line, read one line.
fn round_trip(socket: &std::path::Path, line: &str) -> Result<String> {
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to the GemdropGo daemon at {}", socket.display()))?;
    let mut writer = stream
        .try_clone()
        .context("clone the daemon connection for writing")?;
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .context("write the daemon request")?;

    let mut response = String::new();
    let bytes = BufReader::new(stream)
        .read_line(&mut response)
        .context("read the daemon response")?;
    if bytes == 0 {
        bail!("the GemdropGo daemon closed the connection without replying");
    }
    Ok(response)
}

/// Make sure a daemon is listening on `socket`, starting one if not.
///
/// "It isn't running" is the single most likely first-run failure, and the
/// error text names the binary and the socket for exactly that reason --
/// the same courtesy `ggo_common::run_capture` extends to a missing `emd`.
fn ensure_running(socket: &std::path::Path) -> Result<()> {
    use std::os::unix::net::UnixStream;

    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    let bin = daemon_bin();
    spawn_daemon(&bin, socket)
        .with_context(|| format!("start the GemdropGo daemon (`{bin} {SERVE_MODE_ARG}`)"))?;

    for _ in 0..CONNECT_ATTEMPTS {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(CONNECT_RETRY);
    }
    bail!(
        "started `{bin} {SERVE_MODE_ARG}` but nothing is listening on {} after {}ms -- \
         run it by hand to see why, or set {SOCKET_ENV}",
        socket.display(),
        CONNECT_ATTEMPTS as u128 * CONNECT_RETRY.as_millis(),
    )
}

/// Spawn `ggo serve` detached, with its stdio discarded.
///
/// `smol::process::Command`, not `std`'s: this checkout's `clippy.toml`
/// disallows the latter's `spawn`/`output`/`status` outright ("can block
/// the current thread for an unknown duration").
fn spawn_daemon(bin: &str, socket: &std::path::Path) -> Result<()> {
    let mut command = smol::process::Command::new(bin);
    command
        .arg(SERVE_MODE_ARG)
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The daemon outlives this editor session on purpose -- other tools and
    // the web dashboard share it -- so the child is detached rather than
    // awaited. `kill_on_drop` is explicitly NOT set for the same reason.
    let child = command.spawn().with_context(|| {
        format!("spawn `{bin}` -- is it on PATH? (set {BIN_ENV} to name another)")
    })?;
    log::info!(
        "started the GemdropGo daemon: {bin} {SERVE_MODE_ARG} {} (pid {})",
        socket.display(),
        child.id()
    );
    Ok(())
}

/// A scripted daemon for tests: maps a method or tool name to its reply,
/// and records what was asked.
///
/// Lives in the production module rather than behind `#[cfg(test)]` so
/// panel crates can use it in their own tests -- a panel test must never
/// need a real daemon, a database or a serial port. The same reason
/// `ggo_common` exposes its `ProcRunner` fakes.
#[derive(Default)]
pub struct FakeDaemon {
    replies: std::sync::Mutex<std::collections::HashMap<String, Value>>,
    calls: std::sync::Mutex<Vec<(String, Value)>>,
}

impl FakeDaemon {
    pub fn new() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        fake.on(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "ggo-daemon", "version": "0.1.0"},
            }),
        );
        fake
    }

    /// Reply to `key` -- a JSON-RPC method (`initialize`, `tools/list`) or
    /// a tool name (`ggo_diag`) -- with `result`.
    pub fn on(self: &Arc<Self>, key: &str, result: Value) -> &Arc<Self> {
        if let Ok(mut replies) = self.replies.lock() {
            replies.insert(key.to_string(), result);
        }
        self
    }

    /// Reply to tool `name` with a payload, wrapped as the daemon wraps it.
    pub fn on_tool(self: &Arc<Self>, name: &str, payload: Value) -> &Arc<Self> {
        self.on(
            name,
            json!({"content": [{"type": "text", "text": payload.to_string()}]}),
        )
    }

    /// Reply to tool `name` with a failure, as `mcp::tool_error` encodes one.
    pub fn on_tool_error(self: &Arc<Self>, name: &str, message: &str) -> &Arc<Self> {
        self.on(
            name,
            json!({"content": [{"type": "text", "text": message}], "isError": true}),
        )
    }

    /// Every `(method-or-tool-name, params-or-arguments)` asked so far.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().map(|calls| calls.clone()).unwrap_or_default()
    }

    /// This fake as a [`Transport`].
    pub fn transport(self: &Arc<Self>) -> Transport {
        let fake = Arc::clone(self);
        Arc::new(move |line: &str| fake.handle(line))
    }

    fn handle(&self, line: &str) -> Result<String> {
        let request: Value = serde_json::from_str(line).context("fake daemon: decode request")?;
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        // A tool call is keyed by the tool's name, not by `tools/call`:
        // a test scripts `ggo_diag`, not the transport envelope.
        let (key, recorded) = if method == "tools/call" {
            (
                params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                params.get("arguments").cloned().unwrap_or(Value::Null),
            )
        } else {
            (method.to_string(), params)
        };
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((key.clone(), recorded));
        }

        let reply = self
            .replies
            .lock()
            .ok()
            .and_then(|replies| replies.get(&key).cloned());
        let response = match reply {
            Some(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            // An unscripted call is a test's own mistake; say which one.
            None => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("fake daemon has no reply for {key}")}
            }),
        };
        serde_json::to_string(&response).context("fake daemon: encode response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_with(fake: &Arc<FakeDaemon>) -> Client {
        Client::with_transport(fake.transport()).expect("the fake daemon handshakes")
    }

    #[test]
    fn connecting_handshakes_and_reports_the_daemons_identity() {
        let fake = FakeDaemon::new();
        let client = client_with(&fake);

        assert_eq!(client.handshake().server_name, "ggo-daemon");
        assert_eq!(client.handshake().protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            fake.calls().first().map(|(method, _)| method.clone()),
            Some("initialize".to_string()),
            "the first thing on the wire must be the handshake"
        );
    }

    /// Two independently built processes share this wire format, so a
    /// version that disagrees must stop the connection -- and the message
    /// must name BOTH versions, because the user has two builds installed
    /// and needs to know which to rebuild.
    #[test]
    fn a_protocol_mismatch_is_refused_naming_both_versions() {
        let fake = FakeDaemon::new();
        fake.on(
            "initialize",
            json!({
                "protocolVersion": "1999-01-01",
                "serverInfo": {"name": "ggo-daemon", "version": "0.0.9"},
            }),
        );

        let Err(error) = Client::with_transport(fake.transport()) else {
            panic!("a protocol mismatch must refuse the connection");
        };
        let message = error.to_string();
        assert!(message.contains(PROTOCOL_VERSION), "{message}");
        assert!(message.contains("1999-01-01"), "{message}");
    }

    #[test]
    fn a_tool_call_returns_its_decoded_payload() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_uart_status", json!({"attached": true, "port": "/dev/ttyUSB0"}));
        let client = client_with(&fake);

        let status = client.uart_status().expect("uart status");
        assert_eq!(status["attached"], json!(true));
        assert_eq!(status["port"], json!("/dev/ttyUSB0"));
    }

    /// MCP reports a tool's own failure inside a perfectly successful
    /// JSON-RPC response. Missing that check would read a failed hardware
    /// run as a passed one.
    #[test]
    fn a_tool_that_failed_is_an_error_even_though_the_rpc_succeeded() {
        let fake = FakeDaemon::new();
        fake.on_tool_error("ggo_diag", "diagnostics exited with status 2");
        let client = client_with(&fake);

        let Err(error) = client.diag(vec!["--launch".to_string()]) else {
            panic!("a failed tool must not read as success");
        };
        assert!(error.to_string().contains("status 2"), "{error}");
    }

    #[test]
    fn a_json_rpc_error_carries_the_daemons_message() {
        let fake = FakeDaemon::new();
        let client = client_with(&fake);

        let Err(error) = client.db_migrate() else {
            panic!("an unscripted tool must fail");
        };
        assert!(error.to_string().contains("ggo_db_migrate"), "{error}");
    }

    /// The arguments a panel passes must arrive verbatim -- this is the
    /// contract the daemon's schema validates against.
    #[test]
    fn tool_arguments_reach_the_daemon_as_sent() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_diag", json!({"exit_code": 0}));
        fake.on_tool("ggo_repair_sprites", json!({"changed": 3}));
        fake.on_tool("ggo_uart_dump", json!({"dump": "2026-09-02_08-49-33"}));
        let client = client_with(&fake);

        client
            .diag(vec!["--launch".into(), "--project".into()])
            .expect("diag");
        client
            .repair_sprites("assets/sprites/hero.spr", true)
            .expect("repair");
        client.uart_dump("panel button").expect("dump");

        let calls = fake.calls();
        let arguments = |name: &str| {
            calls
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, args)| args.clone())
                .unwrap_or(Value::Null)
        };
        assert_eq!(
            arguments("ggo_diag")["args"],
            json!(["--launch", "--project"])
        );
        assert_eq!(
            arguments("ggo_repair_sprites"),
            json!({"path": "assets/sprites/hero.spr", "write": true})
        );
        assert_eq!(arguments("ggo_uart_dump"), json!({"reason": "panel button"}));
    }

    /// Prose tools exist (`ggo_diag`'s help). A non-JSON body must come
    /// back as a string rather than failing a caller that only wanted to
    /// show it.
    #[test]
    fn a_tool_whose_text_is_not_json_comes_back_as_a_string() {
        let fake = FakeDaemon::new();
        fake.on(
            "ggo_flash",
            json!({"content": [{"type": "text", "text": "no board attached"}]}),
        );
        let client = client_with(&fake);

        assert_eq!(
            client.flash_status().expect("flash"),
            json!("no board attached")
        );
    }

    /// A flash is minutes of work, so the panel starts a job and polls it.
    /// The start must come back decoded, not as raw JSON.
    #[test]
    fn starting_a_diagnostic_returns_a_running_job() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_start",
            json!({"id": 7, "args": ["diag", "--launch"], "state": "running", "line_count": 0}),
        );
        let client = client_with(&fake);

        let job = client.diag_start(vec!["--launch".into()]).expect("start");
        assert_eq!(job.id, 7);
        assert_eq!(job.state, JobState::Running);
        assert!(job.state.is_running());
        assert!(!job.state.is_error(), "a running job has not failed yet");
    }

    /// Polling must hand back only what is new and say where to resume, or
    /// a flash transcript arrives duplicated on every poll.
    #[test]
    fn polling_a_job_reports_new_lines_and_where_to_resume() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_job_lines",
            json!({
                "lines": [
                    {"index": 4, "text": "==> Flash board"},
                    {"index": 5, "text": "==> Boot verify (UART)"},
                ],
                "state": "running",
            }),
        );
        let client = client_with(&fake);

        let batch = client.job_lines(7, 4).expect("lines");
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.lines[0].text, "==> Flash board");
        assert_eq!(batch.state, JobState::Running);
        assert_eq!(batch.next_since(4), 6, "resume one past the highest index");
    }

    /// A caught-up poller gets nothing, which is not an error -- and must
    /// NOT rewind to 0, which would replay the whole transcript.
    #[test]
    fn an_empty_poll_keeps_its_place_rather_than_rewinding() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_job_lines", json!({"lines": [], "state": "running"}));
        let client = client_with(&fake);

        let batch = client.job_lines(7, 42).expect("lines");
        assert!(batch.lines.is_empty());
        assert_eq!(batch.next_since(42), 42);
    }

    /// The exit code is the tool's own verdict on itself; the hardware
    /// page styles a failed run differently, so it has to survive decoding.
    #[test]
    fn a_finished_job_decodes_its_exit_code() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_job_lines",
            json!({"lines": [], "state": "done", "exit_code": 2}),
        );
        let client = client_with(&fake);

        let batch = client.job_lines(7, 0).expect("lines");
        assert_eq!(batch.state, JobState::Done { exit_code: 2 });
        assert!(!batch.state.is_running());
        assert!(batch.state.is_error(), "a non-zero exit is a failed run");
    }

    #[test]
    fn a_job_that_never_started_decodes_as_failed() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_job_lines",
            json!({"lines": [], "state": "failed", "error": "spawn ggo: No such file"}),
        );
        let client = client_with(&fake);

        let batch = client.job_lines(7, 0).expect("lines");
        assert!(batch.state.is_error());
        let JobState::Failed { error } = batch.state else {
            panic!("expected a failed job");
        };
        assert!(error.contains("No such file"), "{error}");
    }

    #[test]
    fn cancelling_a_job_reports_whether_there_was_one() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_job_cancel", json!({"cancelled": true}));
        let client = client_with(&fake);
        assert!(client.job_cancel(7).expect("cancel"));

        fake.on_tool("ggo_job_cancel", json!({"cancelled": false}));
        assert!(!client.job_cancel(7).expect("cancel"));
    }

    /// The daemon runs one diag at a time -- two would fight over the
    /// board and its UART -- so the refusal must reach the panel as text
    /// naming the job already running.
    #[test]
    fn a_second_diagnostic_is_refused_with_the_running_jobs_id() {
        let fake = FakeDaemon::new();
        fake.on_tool_error("ggo_diag_start", "a diag job is already running (id 3)");
        let client = client_with(&fake);

        let Err(error) = client.diag_start(vec!["--launch".into()]) else {
            panic!("a second diag must be refused");
        };
        assert!(error.to_string().contains("id 3"), "{error}");
    }

    /// The row types here are hand-mirrored from worldlib's, because this
    /// crate must not link `ggo-db`. That makes the serde field names a
    /// contract between two independently built processes, so a full row
    /// -- not a convenient subset -- has to survive the trip.
    #[test]
    fn a_full_frame_row_survives_the_wire_field_for_field() {
        let wire = json!({
            "n": 7, "instrs": 1_000, "i_hits": 10, "i_misses": 1,
            "d_hits": 20, "d_misses": 2, "scanout_wire": 30, "blit_wire": 4,
            "miss_wire": 5, "wire_total": 39, "over_budget": true,
            "frame_budget_cycles": 550_000, "apu_underruns": 1,
            "bg_evictions": 6, "fg_evictions": 7, "spr_evictions": 8,
            "tile_load_wire": 9, "apu_fetch_wire": 11, "sc_upload": 12,
            "sc_oam": 13, "sc_layer": 14, "sc_audio": 15, "sc_other": 16,
            "peak_spr_line": 17, "bg_tiles_distinct": 18,
            "spr_tiles_distinct": 19, "cyc": 20,
        });
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_run_frames", json!([wire]));
        let client = client_with(&fake);

        let frames = client.run_frames(42).expect("frames");
        let frame = frames.first().expect("one frame");
        assert_eq!(frame.n, 7);
        assert_eq!(frame.instrs, 1_000);
        assert!(frame.over_budget, "a bool must not decode as a number");
        assert_eq!(frame.frame_budget_cycles, Some(550_000));
        assert_eq!(frame.spr_tiles_distinct, 19);
        assert_eq!(frame.cyc, 20, "the last field is the one a drift drops");
    }

    /// A device run has no wire model, so its budget column is NULL. That
    /// must decode as `None` rather than failing the whole read.
    #[test]
    fn a_device_frame_decodes_its_null_budget_as_none() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_run_frames",
            json!([{
                "n": 0, "instrs": 0, "i_hits": 0, "i_misses": 0, "d_hits": 0,
                "d_misses": 0, "scanout_wire": 0, "blit_wire": 0, "miss_wire": 0,
                "wire_total": 0, "over_budget": false, "frame_budget_cycles": null,
                "apu_underruns": 0, "bg_evictions": 0, "fg_evictions": 0,
                "spr_evictions": 0, "tile_load_wire": 0, "apu_fetch_wire": 0,
                "sc_upload": 0, "sc_oam": 0, "sc_layer": 0, "sc_audio": 0,
                "sc_other": 0, "peak_spr_line": 0, "bg_tiles_distinct": 0,
                "spr_tiles_distinct": 0, "cyc": 0,
            }]),
        );
        let client = client_with(&fake);

        let frames = client.run_frames(1).expect("frames");
        assert_eq!(frames[0].frame_budget_cycles, None);
    }

    /// `avg_*` are floating point and the rest integral; a run with no
    /// frames yet has them all NULL.
    #[test]
    fn a_run_detail_decodes_its_aggregates_and_their_nulls() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_run_detail",
            json!({
                "id": 5, "cart_id": 2, "cart_name": "demo",
                "started_at": "2026-09-02T08:49:33Z", "frames": 120,
                "frame_budget_cycles": 550_000, "scanout_wire_cycles": 1,
                "refill_cycles": 2, "writeback_cycles": 3, "wire_wait_cycles": 4,
                "label": "worlds/arena", "over_budget_frames": 6,
                "avg_wire_total": 1234.5, "max_wire_total": 4000,
                "avg_i_misses": null, "avg_d_misses": null,
                "max_i_misses": null, "max_d_misses": null, "apu_underruns": 0,
            }),
        );
        let client = client_with(&fake);

        let detail = client.run_detail(5).expect("detail").expect("the run exists");
        assert_eq!(detail.cart_name, "demo");
        assert_eq!(detail.label.as_deref(), Some("worlds/arena"));
        assert_eq!(detail.avg_wire_total, Some(1234.5));
        assert_eq!(detail.avg_i_misses, None, "a run with no frames yet");
    }

    /// A run that is not there is `None`, not an error: a `run` row can
    /// vanish between listing and selecting, and the header degrades to
    /// the picker's listing rather than replacing the whole view with a
    /// failure.
    #[test]
    fn an_unknown_run_reads_as_no_detail_rather_than_an_error() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_run_detail", Value::Null);
        let client = client_with(&fake);

        assert_eq!(client.run_detail(999).expect("absence is not an error"), None);
    }

    /// A daemon that really failed still has to reach the caller.
    #[test]
    fn a_failed_detail_read_is_still_an_error() {
        let fake = FakeDaemon::new();
        fake.on_tool_error("ggo_run_detail", "connection refused");
        let client = client_with(&fake);

        let Err(error) = client.run_detail(1) else {
            panic!("a failed read must be an error");
        };
        assert!(error.to_string().contains("connection refused"), "{error}");
    }

    #[test]
    fn the_run_index_and_cart_list_decode() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_run_index",
            json!([{
                "id": 9, "started_at": "2026-09-02T08:49:33Z",
                "cart_name": "demo", "label": null, "frames": 60,
            }]),
        );
        fake.on_tool(
            "ggo_carts",
            json!([{"id": 1, "name": "demo", "runs": 3, "last_run_at": null}]),
        );
        let client = client_with(&fake);

        let index = client.run_index().expect("index");
        assert_eq!(index[0].id, 9);
        assert_eq!(index[0].label, None);
        let carts = client.carts().expect("carts");
        assert_eq!(carts[0].name, "demo");
        assert_eq!(carts[0].last_run_at, None, "a cart with no runs");
    }

    #[test]
    fn device_runs_and_their_log_decode() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_runs",
            json!([{
                "id": "2026-09-02_08-49-33", "started_at": "2026-09-02_08-49-33",
                "state": "done", "verdict": "PASS",
            }]),
        );
        fake.on_tool("ggo_diag_run_log", json!(["==> compile", "<== compile ok"]));
        let client = client_with(&fake);

        let runs = client.diag_runs(Some(10)).expect("diag runs");
        assert_eq!(runs[0].verdict.as_deref(), Some("PASS"));
        assert_eq!(
            client.diag_run_log("2026-09-02_08-49-33").expect("log").len(),
            2
        );
    }

    /// The fault list is the panel's rail feed; a full row has to survive
    /// the trip, nullable columns included.
    #[test]
    fn a_fault_row_decodes_with_its_nullable_columns() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_faults",
            json!([{
                "id": "2026-09-02_08-49-33_trap",
                "source": "ggo-uartd",
                "at": "2026-09-02_08-49-33",
                "kind": "trap",
                "detail": "mcause=0x2",
                "tty": "/dev/ttyUSB1",
                "boot_stage": null,
                "frames": 0,
                "run_id": null,
            }]),
        );
        let client = client_with(&fake);

        let faults = client.faults(Some(10)).expect("faults");
        assert_eq!(faults[0].kind, "trap");
        assert_eq!(faults[0].detail, "mcause=0x2");
        assert_eq!(faults[0].boot_stage, None, "a dump that never booted");
        assert_eq!(faults[0].run_id, None, "no run it can be linked to");
    }

    /// Omitting the limit must send no `limit` key, so the daemon applies
    /// its own default rather than being handed a guess.
    #[test]
    fn omitting_the_fault_limit_sends_no_limit() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_faults", json!([]));
        let client = client_with(&fake);
        client.faults(None).expect("faults");

        let calls = fake.calls();
        let (_, arguments) = calls
            .iter()
            .find(|(name, _)| name == "ggo_faults")
            .expect("asked");
        assert_eq!(arguments, &json!({}));
    }

    /// The daemon prunes its dumps, so a fault a panel saw a moment ago
    /// can be gone. That is a state to render, not a failure.
    #[test]
    fn a_pruned_fault_reads_as_none_rather_than_an_error() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_fault", Value::Null);
        let client = client_with(&fake);

        assert_eq!(client.fault("gone").expect("absence is not an error"), None);
    }

    /// A dump is hundreds of kilobytes of binary ring buffer, so the raw
    /// form crosses as a path the caller can show or open.
    #[test]
    fn the_raw_dump_comes_back_as_a_path() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_fault_raw_path",
            json!({"path": "/home/u/.ggo/uartd/faults/2026-09-02_08-49-33_trap.log"}),
        );
        let client = client_with(&fake);

        assert_eq!(
            client.fault_raw_path("2026-09-02_08-49-33_trap").expect("path"),
            PathBuf::from("/home/u/.ggo/uartd/faults/2026-09-02_08-49-33_trap.log")
        );
    }

    /// Omitting the limit must send no `limit` key at all, so the daemon
    /// applies its own default rather than being handed a guess.
    #[test]
    fn omitting_the_device_run_limit_sends_no_limit() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_diag_runs", json!([]));
        let client = client_with(&fake);
        client.diag_runs(None).expect("diag runs");

        let calls = fake.calls();
        let (_, arguments) = calls
            .iter()
            .find(|(name, _)| name == "ggo_diag_runs")
            .expect("asked");
        assert_eq!(arguments, &json!({}));
    }

    /// Both "no telemetry" and "no such run" arrive as null, and the
    /// caller only wants to know whether a report exists to open.
    #[test]
    fn a_device_run_without_telemetry_reports_no_perf_run() {
        let fake = FakeDaemon::new();
        fake.on_tool("ggo_diag_perf_run", json!({"perf_run_id": null}));
        let client = client_with(&fake);
        assert_eq!(client.diag_perf_run_id("nope").expect("ask"), None);

        fake.on_tool("ggo_diag_perf_run", json!({"perf_run_id": 77}));
        assert_eq!(client.diag_perf_run_id("real").expect("ask"), Some(77));
    }

    /// The perf JSON must cross as the exact text the run emitted, not
    /// re-encoded: the daemon validates the same bytes.
    #[test]
    fn an_ingest_sends_the_perf_json_verbatim_and_returns_the_new_ids() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_ingest_run",
            json!({"run_id": 12, "cart_id": 3, "truncated_frames": null}),
        );
        let client = client_with(&fake);

        let perf_json = r#"{"cart":"demo","frames":{"n":[0]}}"#;
        let ingested = client
            .ingest_run(perf_json, &["[run] started".to_string()], Some("worlds/arena"))
            .expect("ingest");
        assert_eq!(ingested.run_id, 12);
        assert_eq!(ingested.cart_id, 3);
        assert_eq!(ingested.truncated_frames, None);

        let calls = fake.calls();
        let (_, arguments) = calls
            .iter()
            .find(|(name, _)| name == "ggo_ingest_run")
            .expect("ingested");
        assert_eq!(
            arguments["perf_json"],
            json!(perf_json),
            "the JSON must not be re-encoded on the way out"
        );
        assert_eq!(arguments["uart"], json!(["[run] started"]));
        assert_eq!(arguments["label"], json!("worlds/arena"));
    }

    /// A run past the frame cap IS stored; the truncation is advice, not
    /// a failure, and must not read as one.
    #[test]
    fn a_truncated_ingest_still_succeeds_and_says_how_long_the_run_was() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_ingest_run",
            json!({"run_id": 1, "cart_id": 1, "truncated_frames": 250_000}),
        );
        let client = client_with(&fake);

        let ingested = client.ingest_run("{}", &[], None).expect("ingest");
        assert_eq!(ingested.truncated_frames, Some(250_000));
    }

    /// Malformed perf JSON is the caller's mistake and has to reach a
    /// status line as text.
    #[test]
    fn a_rejected_ingest_carries_the_daemons_reason() {
        let fake = FakeDaemon::new();
        fake.on_tool_error("ggo_ingest_run", "invalid JSON: expected value at line 1");
        let client = client_with(&fake);

        let Err(error) = client.ingest_run("not json", &[], None) else {
            panic!("malformed perf JSON must be rejected");
        };
        assert!(error.to_string().contains("invalid JSON"), "{error}");
    }

    /// An ingest with no label must omit the key rather than send null,
    /// so `run.label` stays absent instead of being written as one.
    #[test]
    fn an_ingest_without_a_label_omits_the_key() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_ingest_run",
            json!({"run_id": 1, "cart_id": 1, "truncated_frames": null}),
        );
        let client = client_with(&fake);
        client.ingest_run("{}", &[], None).expect("ingest");

        let calls = fake.calls();
        let (_, arguments) = calls
            .iter()
            .find(|(name, _)| name == "ggo_ingest_run")
            .expect("ingested");
        assert!(arguments.get("label").is_none(), "{arguments}");
    }

    #[test]
    fn the_advertised_tool_names_are_readable_as_a_capability_probe() {
        let fake = FakeDaemon::new();
        fake.on(
            "tools/list",
            json!({"tools": [{"name": "ggo_diag"}, {"name": "ggo_uart_status"}]}),
        );
        let client = client_with(&fake);

        assert_eq!(
            client.tool_names().expect("tools/list"),
            vec!["ggo_diag".to_string(), "ggo_uart_status".to_string()]
        );
    }

    /// Each request needs its own id, or a future pipelined transport
    /// cannot match replies to calls.
    #[test]
    fn every_request_carries_a_fresh_id() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport: Transport = {
            let seen = Arc::clone(&seen);
            Arc::new(move |line: &str| {
                let request: Value = serde_json::from_str(line)?;
                let id = request["id"].clone();
                if let Ok(mut seen) = seen.lock() {
                    seen.push(id.clone());
                }
                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": PROTOCOL_VERSION,
                        "serverInfo": {"name": "ggo-daemon", "version": "0.1.0"},
                        "content": [{"type": "text", "text": "{}"}],
                    }
                })
                .to_string())
            })
        };
        let client = Client::with_transport(transport).expect("handshake");
        client.db_migrate().expect("migrate");
        client.uart_status().expect("status");

        let ids = seen.lock().expect("ids").clone();
        assert_eq!(ids, vec![json!(1), json!(2), json!(3)]);
    }

    #[test]
    fn a_blank_socket_override_falls_back_to_the_default_path() {
        // SAFETY: single-threaded test; the var is restored below.
        unsafe { std::env::set_var(SOCKET_ENV, "   ") };
        let path = default_socket_path().expect("HOME resolves in the test env");
        unsafe { std::env::remove_var(SOCKET_ENV) };
        assert!(path.ends_with(DEFAULT_SOCKET_REL), "{}", path.display());
    }

    #[test]
    fn a_blank_binary_override_falls_back_to_the_bare_name() {
        // SAFETY: single-threaded test; the var is restored below.
        unsafe { std::env::set_var(BIN_ENV, "") };
        let bin = daemon_bin();
        unsafe { std::env::remove_var(BIN_ENV) };
        assert_eq!(bin, DEFAULT_BIN);
    }

    /// "It isn't running" is the likeliest first-run failure, so a dead
    /// socket with no binary to start must say what to run and where it
    /// looked -- not fail blank.
    #[test]
    fn a_daemon_that_cannot_start_reports_the_binary_and_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("absent.sock");
        // SAFETY: single-threaded test; the var is restored below.
        unsafe { std::env::set_var(BIN_ENV, "ggo-not-a-real-binary") };
        let result = unix_transport(&socket);
        unsafe { std::env::remove_var(BIN_ENV) };

        let Err(error) = result else {
            panic!("connecting to a socket with no daemon must fail");
        };
        let message = format!("{error:#}");
        assert!(message.contains("ggo-not-a-real-binary"), "{message}");
    }
}
