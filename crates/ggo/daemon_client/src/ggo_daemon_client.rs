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

/// The injection seam: anything that can carry one JSON-RPC request line
/// and return one response line.
///
/// A boxed `Fn` rather than a trait for the same reason
/// `ggo_common::ProcRunner` is one -- every implementation is a single
/// function, and a test's fake is a closure. `Send + Sync` because calls
/// happen on `cx.background_spawn`'s thread, never on the UI thread.
pub type Transport = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

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

    /// Run a hardware diagnostic (`ggo diag`).
    pub fn diag(&self, args: Vec<String>) -> Result<Value> {
        self.call_tool("ggo_diag", json!({"args": args}))
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
