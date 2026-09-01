# MCP Tier 1 Agent Surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Per the user's global rules, implementation subagents run on **opus**, and a fresh **opus** reviewer runs after the whole plan lands.

**Goal:** Give an agent driving ZedGG through `zedgg-emu-mcp` the Tier 1 tools from the agent-surface review: board readiness and flash cancel, the rest of the emulator surface (screenshot, UART, free-running run/pause/resume, PPU inspector), cart packing without a shell, and read access to the authored world (list, open, read, screenshot).

**Architecture:** Every tool is wiring over machinery that already exists. Each one is a `Cmd` variant in `crates/ggo/emu_remote/src/protocol.rs`, a dispatch arm in `crates/ggo/emu_panel/src/agent_remote.rs` (`dispatch_inner`), a `pub(crate) fn remote_*` accessor on the panel that owns the state (`EmuPanel` or `WorldPanel`), and a tool entry + dispatch arm in `crates/ggo/emu_mcp/src/tools.rs`. Panel accessors are unit-tested with gpui tests in the panel crate; the bridge is tested with the existing fake-connector pattern; the protocol with serde round-trips. No new logic is invented: readers already exist as `test_*` fns or `remote_*` fns, image producers already exist in `debug.rs` and `ggo_worldlib::render`, the pack argv already exists in `ggo_common`.

**Tech Stack:** Rust 2024, gpui, serde/serde_json, base64, `ggo_worldlib` (path dep into `~/projects/ggo/tools/ggo-worldlib`), `ggo_emu_core`, the `image` crate in `emu_mcp/src/png.rs`.

**Spec:** ZedGG Agent Surface Review, https://claude.ai/code/artifact/08702ff3-f5f7-4a84-af30-34195ee8d268 (Tier 1 section). Its requirements are restated in each task.

## Global Constraints

- Every socket command that touches a panel runs inside `window.update(cx, |_, window, app| panel.update(app, ...))` exactly as `Cmd::FlashWorld` does in `agent_remote.rs`. Never read or update a `Pane` inside a `Workspace` lease except through `pane.update` after `workspace.pane_for`/pane scan (see `ggo_charts_panel::close_charts_item`).
- Verification gates every commit: `./script/clippy -p <crates> && cargo test -p <crates> && git commit ...`. `ggo_emu_mcp` is a bin crate: test it with `cargo test -p ggo_emu_mcp` (no `--lib`).
- Crate names: `ggo_emu_remote`, `ggo_emu_panel`, `ggo_emu_mcp`, `ggo_world_panel`, `ggo_common`. Binary: `zedgg-emu-mcp`, installed with `cargo install --path crates/ggo/emu_mcp --force`.
- No `let _ =` on fallible ops, no `unwrap()` outside tests, no summarizing comments, full-word identifiers, no new files except where a task says so.
- Commit messages: short imperative subject, no AI co-author trailer (user's global rule).
- Tool names use domain prefixes: `hw_`, `emu_`, `cart_`, `world_`. The `tool_list_names_exactly_the_lockstep_surface` test in `tools.rs` asserts the exact ordered list; every task appends to it.
- Image replies on the wire are `{ "width": u32, "height": u32, "bgra_base64": String }`; the bridge converts to PNG image content with `bgra_to_png` (`emu_mcp/src/png.rs`), the same way `emu_next_frame` does.
- Every new `Cmd` variant carries `workspace: Option<String>` first and is added to the `workspace_arg` match in `dispatch_inner`.
- `crates/ggo/emu_mcp/AGENTS.md` tool table gets one row per new tool in the task that adds it.

---

## File map

| File | Responsibility in this plan |
|---|---|
| `crates/ggo/emu_remote/src/protocol.rs` | New `Cmd` variants, `HwEnvPayload`, `DebugView`, `WorldReadPayload` + round-trip tests |
| `crates/ggo/emu_panel/src/agent_remote.rs` | `workspace_arg` match arms, dispatch arms, `world_panel_for(workspace)` helper, `bgra_reply` helper |
| `crates/ggo/emu_panel/src/ggo_emu_panel.rs` | `remote_env`, `remote_flash_cancel`, `remote_resume`, `remote_debug_view`, `remote_pack_plan`; extract `cancel_flash` from `flash_to_board_with` |
| `crates/ggo/emu_panel/src/hardware.rs` | `Missing::code()`, `HardwareEnv::remote_payload()` |
| `crates/ggo/world_panel/src/ggo_world_panel.rs` | `remote_list`, `remote_open`, `remote_read`, `remote_screenshot` on `WorldPanel`; `fn composite_scene` pure helper |
| `crates/ggo/emu_mcp/src/tools.rs` | Tool schema entries, dispatch arms, `image_content` helper, tests |
| `crates/ggo/emu_mcp/AGENTS.md` | Tool table rows |

---

### Task 1: `hw_env` and `hw_flash_cancel`

**Files:**
- Modify: `crates/ggo/emu_remote/src/protocol.rs` (Cmd enum ~line 27; add payload struct after `FlashDiagStep`)
- Modify: `crates/ggo/emu_panel/src/hardware.rs` (`impl Missing` ~line 60; `impl HardwareEnv` ~line 135)
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (`flash_to_board_with` ~line 772; `remote_*` block ~line 3300)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs` (`workspace_arg` match ~line 410; the pre-window `FlashStatus` early return ~line 426; dispatch arms ~line 540)
- Modify: `crates/ggo/emu_mcp/src/tools.rs` (`tool_list` after `hw_flash_wait`; dispatch after `"hw_flash_status"`; tests)
- Modify: `crates/ggo/emu_mcp/AGENTS.md` (tool table)

**Interfaces:**
- Produces: `Cmd::HwEnv { workspace }`, `Cmd::FlashCancel { workspace }`, `HwEnvPayload`, `Missing::code(self) -> &'static str`, `HardwareEnv::remote_payload(&self) -> HwEnvPayload`, `EmuPanel::remote_env(&mut self) -> HwEnvPayload`, `EmuPanel::remote_flash_cancel(&mut self, cx) -> bool`, `EmuPanel::cancel_flash(&mut self, cx) -> bool`.

- [ ] **Step 1: Protocol variants and payload with a round-trip test**

In `protocol.rs`, add to `Cmd` after `CloseReport`:

```rust
    /// The machine's board-readiness probe: what is missing, which
    /// serial ports were found, whether the repo and the in-IDE
    /// emulator are at different commits.
    HwEnv { workspace: Option<String> },
    /// Cancel the flash in flight. The reply says whether there was one.
    FlashCancel { workspace: Option<String> },
```

After `FlashDiagStep`, add:

```rust
/// `Cmd::HwEnv`'s reply: `HardwareEnv` as the agent needs it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HwEnvPayload {
    /// Nothing is missing; a flash would start.
    pub ready: bool,
    /// Every unmet precondition in fix order: `{code, label}` where
    /// `code` is one of project | repo | diag | emd | port | port_stuck.
    pub missing: Vec<HwMissing>,
    pub ports: Vec<String>,
    pub stuck_board: bool,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub diag_bin: Option<String>,
    pub emd_bin: Option<String>,
    /// `Some((repo_short, emu_short))` when the flash source and the
    /// in-IDE emulator are at different commits.
    pub version_skew: Option<(String, String)>,
    pub emu_commit_in_repo: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HwMissing {
    pub code: String,
    pub label: String,
}
```

Add a test in the protocol `tests` module:

```rust
    #[test]
    fn hw_env_and_flash_cancel_requests_round_trip() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"hw_env"}"#).unwrap().cmd,
            Cmd::HwEnv { workspace: None }
        );
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":"flash_cancel","workspace":"/w"}"#).unwrap().cmd,
            Cmd::FlashCancel { workspace: Some("/w".to_string()) }
        );
        let payload = HwEnvPayload {
            ready: false,
            missing: vec![HwMissing { code: "port".into(), label: "no serial device".into() }],
            ports: vec![],
            stuck_board: false,
            project: Some("/game".into()),
            repo: Some("/repo".into()),
            diag_bin: Some("ggo-diag".into()),
            emd_bin: Some("emd".into()),
            version_skew: Some(("5370a5a".into(), "7fe694e".into())),
            emu_commit_in_repo: Some(true),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""missing":[{"code":"port","label":"no serial device"}]"#), "{json}");
        let back: HwEnvPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
    }
```

- [ ] **Step 2: Run the protocol tests; the new one fails to compile until the types exist, then passes**

Run: `cargo test -p ggo_emu_remote --lib`
Expected: PASS (15 tests).

- [ ] **Step 3: `Missing::code` and `HardwareEnv::remote_payload` in `hardware.rs`**

In `impl Missing`, after `label`:

```rust
    /// The wire code for [`Self::label`]'s prose.
    pub fn code(self) -> &'static str {
        match self {
            Missing::Repo => "repo",
            Missing::Diag => "diag",
            Missing::Emd => "emd",
            Missing::Port => "port",
            Missing::PortStuck => "port_stuck",
            Missing::Project => "project",
        }
    }
```

In `impl HardwareEnv`, after `version_skew`:

```rust
    /// This probe as the agent socket reports it.
    pub fn remote_payload(&self) -> ggo_emu_remote::protocol::HwEnvPayload {
        use ggo_emu_remote::protocol::{HwEnvPayload, HwMissing};
        HwEnvPayload {
            ready: self.ready(),
            missing: self
                .missing()
                .into_iter()
                .map(|missing| HwMissing { code: missing.code().to_string(), label: missing.label() })
                .collect(),
            ports: self.ports.clone(),
            stuck_board: self.stuck_board,
            project: self.project.as_ref().map(|path| path.display().to_string()),
            repo: self.repo.as_ref().map(|path| path.display().to_string()),
            diag_bin: self.diag_bin.clone(),
            emd_bin: self.emd_bin.clone(),
            version_skew: self.version_skew(),
            emu_commit_in_repo: self.emu_commit_in_repo,
        }
    }
```

Add a test in the `hardware.rs` tests module (uses the existing `ready_env()` helper):

```rust
    #[test]
    fn the_remote_payload_names_every_missing_prerequisite_by_code() {
        let ready = ready_env().remote_payload();
        assert!(ready.ready && ready.missing.is_empty(), "{ready:?}");
        assert_eq!(ready.ports, vec!["/dev/ttyUSB0".to_string()]);

        let mut env = ready_env();
        env.ports.clear();
        env.stuck_board = true;
        env.emd_bin = None;
        let payload = env.remote_payload();
        assert!(!payload.ready);
        let codes: Vec<&str> = payload.missing.iter().map(|m| m.code.as_str()).collect();
        assert_eq!(codes, ["emd", "port_stuck"]);
        assert!(payload.missing[1].label.contains("replug") || !payload.missing[1].label.is_empty());
    }
```

- [ ] **Step 4: `cancel_flash`, `remote_env`, `remote_flash_cancel` on `EmuPanel`**

In `ggo_emu_panel.rs`, replace the body of the `if let Some(flash) = self.flash.take()` block in `flash_to_board_with` with a call, extracting it:

```rust
    pub fn flash_to_board_with(
        &mut self,
        world: Option<&str>,
        rebuild_gateware: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cancel_flash(cx) {
            return;
        }
        let config = hardware::FlashConfig {
            world: world.map(str::to_string),
            rebuild_gateware,
            ..Default::default()
        };
        self.start_flash(config, window, cx).ok();
    }

    /// Cancel the flash in flight, if any. Cancelling is not failing: the
    /// timeline stays as it was, with the phase it got to still marked
    /// running. Before any world is remembered, too: a cancel started
    /// nothing, so it has no business changing what the next one boots.
    fn cancel_flash(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(flash) = self.flash.take() else {
            return false;
        };
        let elapsed = flash.started.elapsed();
        self.last_flash = Some((flash.progress.clone(), elapsed));
        self.status = Some("flash cancelled".to_string());
        self.status_is_error = false;
        cx.notify();
        true
    }
```

(Keep the existing comment text; move it onto `cancel_flash`.)

In the `remote_*` block, after `remote_flash_status`:

```rust
    /// The board-readiness probe, re-run now: plugging the board in
    /// changes the answer, and the agent asks exactly when it wonders.
    pub(crate) fn remote_env(&mut self) -> ggo_emu_remote::protocol::HwEnvPayload {
        self.invalidate_hardware();
        self.hardware_env_cached().remote_payload()
    }

    /// The button's cancel, for the agent. `false` when nothing was running.
    pub(crate) fn remote_flash_cancel(&mut self, cx: &mut Context<Self>) -> bool {
        self.cancel_flash(cx)
    }
```

Add a gpui test next to `test_remote_flash_refuses_while_one_is_running`:

```rust
    /// The agent's cancel is the button's cancel: the timeline is kept
    /// with its phase still running, and a second cancel finds nothing.
    #[gpui::test]
    async fn test_remote_flash_cancel_retires_the_run_without_failing_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (streamer, _calls) = fake_streamer(vec!["==> Flash board"], true);
        let (panel, cx) = flashable_panel(cx, dir.path(), streamer);
        let request =
            hardware::flash_request(&ready_hardware(dir.path()), &hardware::FlashConfig::default()).expect("ready");
        panel.update_in(cx, |panel, _window, cx| {
            panel.start_board_run(
                vec![request],
                "flashing".to_string(),
                hardware::FlashProgress::flash(),
                cx,
            );
            assert!(panel.remote_flash_status().active);
            assert!(panel.remote_flash_cancel(cx), "a live flash was cancelled");
            let status = panel.remote_flash_status();
            assert!(!status.active);
            assert_eq!(status.verdict, None, "cancelled is not failed");
            assert_eq!(panel.status.as_deref(), Some("flash cancelled"));
            assert!(!panel.remote_flash_cancel(cx), "nothing left to cancel");
        });
    }

    #[gpui::test]
    async fn test_remote_env_reports_the_probe(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            let env = panel.remote_env();
            // A temp worktree with no repo, no binaries on PATH: nothing
            // is ready, and every reason has a code.
            assert!(!env.ready);
            assert!(env.missing.iter().all(|m| !m.code.is_empty() && !m.label.is_empty()), "{env:?}");
        });
    }
```

- [ ] **Step 5: Host dispatch in `agent_remote.rs`**

Add to the `workspace_arg` match:

```rust
        | Cmd::HwEnv { workspace }
        | Cmd::FlashCancel { workspace }
```

Both need a panel but no window for `HwEnv`. Add, directly after the `FlashStatus` early return and before `let window = ...`:

```rust
    if let Cmd::HwEnv { .. } = cmd {
        let panel = target.panel.ok_or("no emu panel open in this workspace — open the emulator or hardware tab once")?;
        let payload = panel.update(cx, |p, _| p.remote_env()).map_err(|e| e.to_string())?;
        return Ok(serde_json::to_value(payload).expect("HwEnvPayload serializes"));
    }
```

Add to the `unreachable!` arm: `Cmd::Status | Cmd::FlashStatus { .. } | Cmd::HwEnv { .. } => unreachable!("handled above"),`

Add a dispatch arm after `Cmd::CloseReport`:

```rust
        Cmd::FlashCancel { .. } => {
            let panel = target.panel.ok_or("no emu panel open in this workspace")?;
            let cancelled = panel
                .update(cx, |p, cx| p.remote_flash_cancel(cx))
                .map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "cancelled": cancelled }))
        }
```

- [ ] **Step 6: Bridge tools**

In `tool_list()`, after the `hw_flash_wait` entry:

```rust
        { "name": "hw_env",
          "description": "Is this machine ready to flash? {ready, missing[{code,label}], ports, stuck_board, project, repo, diag_bin, emd_bin, version_skew:[repo_commit, emu_commit]|null, emu_commit_in_repo}. Call before hw_flash: a missing prerequisite here is what hw_flash would fail on, and version_skew means the board would render a different PPU than the in-IDE emulator. Probes fresh each call.",
          "inputSchema": with(json!({})) },
        { "name": "hw_flash_cancel",
          "description": "Cancel the flash in flight, the same as the user's Cancel button. Returns {cancelled: bool}; false when nothing was running. The timeline keeps the phase it reached; a cancelled run is not a failed one.",
          "inputSchema": with(json!({})) },
```

In `call_tool_inner`, after the `"hw_flash_status"` arm:

```rust
        "hw_env" => {
            let data = send(&session.socket, Cmd::HwEnv { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "hw_flash_cancel" => {
            let data = send(&session.socket, Cmd::FlashCancel { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
```

Append `"hw_env", "hw_flash_cancel",` to the expected names in `tool_list_names_exactly_the_lockstep_surface` (after `"hw_flash_wait"`). Add a test:

```rust
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
```

Add rows to the AGENTS.md tool table after `hw_flash_wait`:

```
| `hw_env` | board readiness: `{ ready, missing[{code,label}], ports, version_skew }` — call before `hw_flash` |
| `hw_flash_cancel` | cancel the flash in flight; `{ cancelled }` |
```

- [ ] **Step 7: Verify and commit**

```bash
./script/clippy -p ggo_emu_remote -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp \
  && git add -A && git commit -m "feat: hw_env and hw_flash_cancel MCP tools"
```

---

### Task 2: `emu_screenshot` and `emu_uart`

**Files:**
- Modify: `crates/ggo/emu_remote/src/protocol.rs` (Cmd enum)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs` (`workspace_arg`; extract `bgra_reply`; dispatch arms)
- Modify: `crates/ggo/emu_mcp/src/tools.rs` (extract `image_content`; tools; tests)
- Modify: `crates/ggo/emu_mcp/AGENTS.md`

**Interfaces:**
- Consumes: `EmuPanel::remote_screenshot(&self) -> Option<(u32, u32, Vec<u8>)>`, `EmuPanel::remote_uart(&self, tail: Option<usize>) -> Vec<String>` (both exist, `ggo_emu_panel.rs` ~lines 3434, 3440).
- Produces: `Cmd::Screenshot { workspace }`, `Cmd::Uart { workspace, tail }`, `fn bgra_reply(w, h, bgra) -> serde_json::Value` in `agent_remote.rs`, `fn image_content(shot: &Value) -> Result<Value, String>` in `tools.rs`. Task 4 reuses both.

- [ ] **Step 1: Protocol**

Add to `Cmd`:

```rust
    /// The last presented frame, `{width, height, bgra_base64}`. Works
    /// whether the run is lock-step or free-running.
    Screenshot { workspace: Option<String> },
    /// The newest `tail` lines of the run's UART/console log (whole log
    /// when omitted). Readable mid-run: `Stop` is not the only way to
    /// see a panic.
    Uart {
        workspace: Option<String>,
        #[serde(default)]
        tail: Option<usize>,
    },
```

Test:

```rust
    #[test]
    fn screenshot_and_uart_requests_round_trip() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"screenshot"}"#).unwrap().cmd,
            Cmd::Screenshot { workspace: None }
        );
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":"uart","tail":40}"#).unwrap().cmd,
            Cmd::Uart { workspace: None, tail: Some(40) }
        );
        assert_eq!(
            parse_request(r#"{"id":3,"cmd":"uart"}"#).unwrap().cmd,
            Cmd::Uart { workspace: None, tail: None }
        );
    }
```

- [ ] **Step 2: Host: extract `bgra_reply`, add arms**

In `agent_remote.rs`, add a free function near `world_value`:

```rust
/// A framebuffer as the wire carries it; the bridge turns it into PNG.
fn bgra_reply(width: u32, height: u32, bgra: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "width": width,
        "height": height,
        "bgra_base64": base64::engine::general_purpose::STANDARD.encode(bgra),
    })
}
```

Replace the inline `reply["screenshot"] = serde_json::json!({...})` in `Cmd::NextFrame` with `reply["screenshot"] = bgra_reply(w, h, &bgra);`.

`workspace_arg` match: add `| Cmd::Screenshot { workspace } | Cmd::Uart { workspace, .. }`.

Neither needs a window. Add before `let window = ...`, after the `HwEnv` block:

```rust
    if let Cmd::Screenshot { .. } = cmd {
        let panel = target.panel.ok_or("no emu panel open in this workspace")?;
        let shot = panel.update(cx, |p, _| p.remote_screenshot()).map_err(|e| e.to_string())?;
        let (w, h, bgra) = shot.ok_or("no frame presented yet — start a run first")?;
        return Ok(bgra_reply(w, h, &bgra));
    }
    if let Cmd::Uart { tail, .. } = &cmd {
        let panel = target.panel.ok_or("no emu panel open in this workspace")?;
        let lines = panel.update(cx, |p, _| p.remote_uart(*tail)).map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({ "lines": lines }));
    }
```

Extend the `unreachable!` arm with `| Cmd::Screenshot { .. } | Cmd::Uart { .. }`.

- [ ] **Step 3: Bridge: extract `image_content`, add tools**

In `tools.rs`, add a free function before `call_tool`:

```rust
/// A `{width, height, bgra_base64}` reply as MCP PNG image content.
fn image_content(shot: &Value) -> Result<Value, String> {
    use base64::Engine as _;
    let bgra = base64::engine::general_purpose::STANDARD
        .decode(shot["bgra_base64"].as_str().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let (w, h) = (
        shot["width"].as_u64().unwrap_or(0) as u32,
        shot["height"].as_u64().unwrap_or(0) as u32,
    );
    let png = bgra_to_png(w, h, &bgra)?;
    Ok(json!({
        "type": "image",
        "mimeType": "image/png",
        "data": base64::engine::general_purpose::STANDARD.encode(&png),
    }))
}
```

Replace the inline PNG block in the `"emu_next_frame"` arm with `content.push(image_content(&shot)?);`.

Tool entries after `emu_stop`:

```rust
        { "name": "emu_screenshot",
          "description": "The emulator's last presented frame as PNG (320×240), for a lock-step or free-running run. Errors when no frame has been presented yet.",
          "inputSchema": with(json!({})) },
        { "name": "emu_uart",
          "description": "The run's UART/console log, newest `tail` lines (default all), readable while the run is live — read a panic without stopping the run.",
          "inputSchema": with(json!({ "tail": { "type": "number", "description": "Newest N lines (omit for the whole log)" } })) },
```

Dispatch arms after `"emu_stop"`:

```rust
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
```

Append `"emu_screenshot", "emu_uart",` after `"emu_stop"` in the names test. Add a test:

```rust
    #[test]
    fn emu_screenshot_is_png_and_emu_uart_is_joined_lines() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]);
        let connect = move |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            if line.contains(r#""cmd":"screenshot""#) {
                Ok(format!(r#"{{"id":1,"ok":true,"data":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}"#))
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
```

AGENTS.md rows after `emu_stop`:

```
| `emu_screenshot` | last presented frame as PNG, any run mode |
| `emu_uart { tail? }` | the run's UART/console log, readable mid-run |
```

- [ ] **Step 4: Verify and commit**

```bash
./script/clippy -p ggo_emu_remote -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp \
  && git add -A && git commit -m "feat: emu_screenshot and emu_uart MCP tools"
```

---

### Task 3: `emu_run`, `emu_pause`, `emu_resume`

**Files:**
- Modify: `crates/ggo/emu_remote/src/protocol.rs`
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (`remote_*` block; add `remote_resume`)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs`
- Modify: `crates/ggo/emu_mcp/src/tools.rs`, `crates/ggo/emu_mcp/AGENTS.md`

**Interfaces:**
- Consumes: `EmuPanel::remote_boot(cart, root, window, cx) -> Result<(), String>`, `remote_pause() -> Result<(), String>`, `remote_progress() -> (u32, bool, Option<String>)`, `drive::Session::resume()`, `is_paused()`, `crate::open_emu_item`, `RemotePanels`, `await_frame` (all exist).
- Produces: `Cmd::Run { workspace, cart }`, `Cmd::Pause { workspace }`, `Cmd::Resume { workspace }`, `EmuPanel::remote_resume(&mut self) -> Result<(), String>`.

- [ ] **Step 1: Protocol**

```rust
    /// Boot `cart` free-running -- the panel's own Run button, no
    /// lock-step, no inspection tap. Pair with `Pause`/`Resume`,
    /// `Screenshot` and `Uart` to watch a game play itself.
    Run { workspace: Option<String>, cart: String },
    /// Pause the live run at the next frame boundary.
    Pause { workspace: Option<String> },
    /// Resume a paused run.
    Resume { workspace: Option<String> },
```

Test:

```rust
    #[test]
    fn run_pause_resume_requests_round_trip() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"run","cart":"target/ggo-emulate/worlds-arena.ggo"}"#).unwrap().cmd,
            Cmd::Run { workspace: None, cart: "target/ggo-emulate/worlds-arena.ggo".to_string() }
        );
        assert_eq!(parse_request(r#"{"id":2,"cmd":"pause"}"#).unwrap().cmd, Cmd::Pause { workspace: None });
        assert_eq!(parse_request(r#"{"id":3,"cmd":"resume"}"#).unwrap().cmd, Cmd::Resume { workspace: None });
    }
```

- [ ] **Step 2: `remote_resume` on the panel with a test**

After `remote_pause`:

```rust
    /// Resume like `toggle_pause` does from a paused run.
    pub(crate) fn remote_resume(&mut self) -> Result<(), String> {
        self.auto_paused = false;
        let session = self.remote_session()?;
        if !session.is_paused() {
            return Err("not paused".to_string());
        }
        session.resume();
        Ok(())
    }
```

Test, next to the existing lock-step tests (find `test_remote_boot` or the first test that calls `remote_boot`; copy its setup — it boots a fixture cart through `run_menu_workspace` and a fixture written by the test's helper. Use the same cart fixture and helper names as that test):

```rust
    #[gpui::test]
    async fn test_remote_resume_only_resumes_a_paused_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            assert!(panel.remote_resume().is_err(), "no run live");
        });
    }
```

(If the crate has a booting fixture helper — search for `remote_boot(` in the tests module — extend the test: boot, `remote_pause().unwrap()`, `remote_resume().unwrap()`, then `remote_resume()` errors with "not paused". If no such helper exists, the no-run assertion above is the test.)

- [ ] **Step 3: Host arms**

`workspace_arg`: add `| Cmd::Run { workspace, .. } | Cmd::Pause { workspace } | Cmd::Resume { workspace }`.

`Pause`/`Resume` need no window; add after the `Uart` block:

```rust
    if matches!(cmd, Cmd::Pause { .. } | Cmd::Resume { .. }) {
        let panel = target.panel.ok_or("no run live — emu_run or emu_start first")?;
        let paused = matches!(cmd, Cmd::Pause { .. });
        panel
            .update(cx, |p, _| if paused { p.remote_pause() } else { p.remote_resume() })
            .map_err(|e| e.to_string())??;
        let (frame, running, _) = panel.update(cx, |p, _| p.remote_progress()).map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({ "paused": paused, "frame": frame, "running": running }));
    }
```

Extend `unreachable!` with `| Cmd::Pause { .. } | Cmd::Resume { .. }`.

`Run` is `Start` without the tap and the pause. Add a dispatch arm after `Cmd::Start`, reusing its boot block verbatim up to and including the `RemotePanels` registration, then:

```rust
        Cmd::Run { cart, .. } => {
            let root = std::path::PathBuf::from(&target_root);
            // Same boot as `Start` (a closed panel is reopened), minus the
            // tap and the pause: this run plays itself.
            let booted: Result<WeakEntity<EmuPanel>, String> = match (target.panel, target.workspace) {
                (Some(panel), _) => window
                    .update(cx, |_, window, app| {
                        panel
                            .update(app, |p, cx| p.remote_boot(cart, root, window, cx))
                            .map_err(|e| e.to_string())
                            .and_then(|r| r)
                            .map(|()| panel.clone())
                    })
                    .map_err(|e| e.to_string())?,
                (None, Some(workspace)) => window
                    .update(cx, |_, window, app| {
                        workspace.update(app, |workspace, cx| {
                            let mut outcome = Err("emu panel did not open".to_string());
                            crate::open_emu_item(workspace, window, cx, |panel, window, cx| {
                                outcome = panel.remote_boot(cart, root, window, cx);
                            });
                            let panel = workspace
                                .items_of_type::<crate::EmulatorItem>(cx)
                                .next()
                                .map(|item| item.read(cx).panel().downgrade());
                            outcome.and_then(|()| panel.ok_or("emu panel did not open".to_string()))
                        })
                    })
                    .map_err(|e| e.to_string())?,
                (None, None) => Err("workspace vanished".to_string()),
            };
            let panel = booted?;
            cx.update(|cx| {
                if cx.try_global::<RemotePanels>().is_none() {
                    cx.set_global(RemotePanels::default());
                }
                cx.global_mut::<RemotePanels>()
                    .panels
                    .insert(target_root.clone(), (panel.clone(), Some(window)));
            });
            let frame = await_frame(&panel, cx, 1, std::time::Duration::from_secs(5)).await?;
            Ok(serde_json::json!({ "started": true, "frame": frame, "running": true }))
        }
```

Check how `Cmd::Start` obtains `root` (it is computed above the match from `target_root`); reuse that binding instead of recomputing if it is in scope.

- [ ] **Step 4: Bridge tools**

After `emu_uart` in `tool_list`:

```rust
        { "name": "emu_run",
          "description": "Boot a cart free-running in the Zed emulator panel — the panel's own Run button, no lock-step. The game plays itself; watch it with emu_screenshot and emu_uart, pause with emu_pause. Use emu_start instead when you need to drive input frame by frame. Pack first: cart_pack (or emd pack-ggo).",
          "inputSchema": with(json!({ "cart": { "type": "string", "description": "Worktree-relative packed cart path, e.g. target/ggo-emulate/worlds-arena.ggo" } })) },
        { "name": "emu_pause", "description": "Pause the live run at the next frame boundary. Returns {paused, frame, running}.", "inputSchema": with(json!({})) },
        { "name": "emu_resume", "description": "Resume a paused run. Returns {paused, frame, running}.", "inputSchema": with(json!({})) },
```

Arms:

```rust
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
```

Names test: append `"emu_run", "emu_pause", "emu_resume",` after `"emu_uart"`. Test:

```rust
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
```

AGENTS.md rows:

```
| `emu_run { cart }` | boot a cart free-running (the Run button); watch with `emu_screenshot`/`emu_uart` |
| `emu_pause` / `emu_resume` | pause/resume the live run; `{ paused, frame, running }` |
```

- [ ] **Step 5: Verify and commit**

```bash
./script/clippy -p ggo_emu_remote -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp \
  && git add -A && git commit -m "feat: emu_run, emu_pause, emu_resume MCP tools"
```

---

### Task 4: `emu_debug` (PPU inspector)

**Files:**
- Modify: `crates/ggo/emu_remote/src/protocol.rs` (`DebugView` enum, `Cmd::Debug`)
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (`remote_debug_view`)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs`
- Modify: `crates/ggo/emu_mcp/src/tools.rs`, `crates/ggo/emu_mcp/AGENTS.md`

**Interfaces:**
- Consumes: `drive::Session::snapshot(&self) -> Option<Arc<PpuSnapshot>>`; `debug::{tile_sheet_bgra, map_bgra, oam_composite_bgra, oam_rows, oam_row_label, rgb565_label, layer_labels, SHEET_PX, MAP_PX}`; `ggo_emu_core::ppu::{PpuSnapshot, LAYER_COUNT}`; `drive::{WIDTH, HEIGHT}`; `bgra_reply` (Task 2); `image_content` (Task 2).
- Produces: `DebugView { Tiles, Map, Oam, Palettes }`, `Cmd::Debug { workspace, view, bank, palette, layer }`, `EmuPanel::remote_debug_view(&self, view, bank, palette, layer) -> Result<serde_json::Value, String>`.

- [ ] **Step 1: Protocol**

```rust
/// Which PPU inspector view `Cmd::Debug` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugView {
    /// Every VRAM tile at 1x, colored by `bank`/`palette`.
    Tiles,
    /// One background layer composed at 1x.
    Map,
    /// The OAM sprites composited onto a blank screen, plus a row table.
    Oam,
    /// Both palette banks as RGB565 hex, no image.
    Palettes,
}
```

To `Cmd`:

```rust
    /// The frame-step debugger's views, for the run in flight (paused or
    /// not): `{image?, rows?, palettes?, labels}`.
    Debug {
        workspace: Option<String>,
        view: DebugView,
        #[serde(default)]
        bank: usize,
        #[serde(default)]
        palette: usize,
        #[serde(default)]
        layer: usize,
    },
```

Test:

```rust
    #[test]
    fn debug_requests_default_their_indices() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"debug","view":"tiles"}"#).unwrap().cmd,
            Cmd::Debug { workspace: None, view: DebugView::Tiles, bank: 0, palette: 0, layer: 0 }
        );
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":"debug","view":"map","layer":2}"#).unwrap().cmd,
            Cmd::Debug { workspace: None, view: DebugView::Map, bank: 0, palette: 0, layer: 2 }
        );
        assert!(parse_request(r#"{"id":3,"cmd":"debug","view":"sprites"}"#).is_err());
    }
```

- [ ] **Step 2: `remote_debug_view` on the panel**

After `remote_uart`:

```rust
    /// One inspector view over the run's latest PPU snapshot. Bounds are
    /// checked here so a bad index is a named error, not a panic in the
    /// decoder.
    pub(crate) fn remote_debug_view(
        &self,
        view: ggo_emu_remote::protocol::DebugView,
        bank: usize,
        palette: usize,
        layer: usize,
    ) -> Result<serde_json::Value, String> {
        use ggo_emu_core::ppu::LAYER_COUNT;
        use ggo_emu_remote::protocol::DebugView;
        let snapshot = self
            .remote_session()?
            .snapshot()
            .ok_or("no PPU snapshot yet — the run has not presented a frame")?;
        let palette_count = snapshot.palettes[0].len() / 16;
        let image = |width: usize, height: usize, bgra: Vec<u8>| {
            serde_json::json!({
                "width": width as u32,
                "height": height as u32,
                "bgra_base64": {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.encode(&bgra)
                },
            })
        };
        Ok(match view {
            DebugView::Tiles => {
                if bank > 1 {
                    return Err("bank must be 0 (bg/fg) or 1 (sprites)".to_string());
                }
                if palette >= palette_count {
                    return Err(format!("palette must be < {palette_count}"));
                }
                serde_json::json!({
                    "view": "tiles", "bank": bank, "palette": palette,
                    "image": image(debug::SHEET_PX, debug::SHEET_PX, debug::tile_sheet_bgra(&snapshot, bank, palette)),
                })
            }
            DebugView::Map => {
                if layer >= LAYER_COUNT {
                    return Err(format!("layer must be < {LAYER_COUNT}"));
                }
                let labels = debug::layer_labels(&snapshot);
                serde_json::json!({
                    "view": "map", "layer": layer,
                    "label": labels[layer],
                    "scroll": [snapshot.scroll[layer].0, snapshot.scroll[layer].1],
                    "enabled": snapshot.layer_enable[layer],
                    "priority": snapshot.layer_prio[layer],
                    "image": image(debug::MAP_PX, debug::MAP_PX, debug::map_bgra(&snapshot, layer)),
                })
            }
            DebugView::Oam => {
                let rows: Vec<String> = debug::oam_rows(&snapshot)
                    .into_iter()
                    .map(|(index, entry)| debug::oam_row_label(index, &entry))
                    .collect();
                serde_json::json!({
                    "view": "oam",
                    "rows": rows,
                    "image": image(drive::WIDTH as usize, drive::HEIGHT as usize, debug::oam_composite_bgra(&snapshot)),
                })
            }
            DebugView::Palettes => {
                let bank_hex = |bank: &[u16]| -> Vec<Vec<String>> {
                    bank.chunks(16)
                        .map(|palette| palette.iter().map(|c| debug::rgb565_label(*c)).collect())
                        .collect()
                };
                serde_json::json!({
                    "view": "palettes",
                    "bg_fg": bank_hex(&snapshot.palettes[0]),
                    "sprites": bank_hex(&snapshot.palettes[1]),
                })
            }
        })
    }
```

Check `debug::oam_rows`' return type against `debug.rs:284` (`Vec<(usize, OamEntry)>`) and `oam_row_label(index, &entry)` at `:292`; adjust the borrow if `OamEntry` is `Copy`. Check `drive::WIDTH`/`HEIGHT` types (they are used as `u32` in `remote_screenshot`); cast as needed. Confirm `SHEET_PX`/`MAP_PX` are `pub` in `debug.rs:25,27`; make them `pub` if not.

Test in the panel tests module:

```rust
    #[gpui::test]
    async fn test_remote_debug_view_needs_a_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            let err = panel
                .remote_debug_view(ggo_emu_remote::protocol::DebugView::Tiles, 0, 0, 0)
                .unwrap_err();
            assert!(err.contains("no run live"), "{err}");
        });
    }
```

If the tests module has a helper that boots a fixture cart (search `remote_boot(` in tests), add a second test that boots, awaits a frame with `cx.run_until_parked()`, then asserts `remote_debug_view(DebugView::Palettes, 0, 0, 0)` returns a value with `bg_fg` of 16-entry rows and `remote_debug_view(DebugView::Tiles, 2, 0, 0)` errors with "bank must be".

- [ ] **Step 3: Host arm**

`workspace_arg`: `| Cmd::Debug { workspace, .. }`. No window needed; after the `Pause`/`Resume` block:

```rust
    if let Cmd::Debug { view, bank, palette, layer, .. } = &cmd {
        let panel = target.panel.ok_or("no run live — emu_run or emu_start first")?;
        return panel
            .update(cx, |p, _| p.remote_debug_view(*view, *bank, *palette, *layer))
            .map_err(|e| e.to_string())?;
    }
```

Extend `unreachable!` with `| Cmd::Debug { .. }`.

- [ ] **Step 4: Bridge tool**

After `emu_resume`:

```rust
        { "name": "emu_debug",
          "description": "The PPU inspector for the run in flight — what the frame-step debugger shows a human. view=tiles: every VRAM tile at 1× as PNG, colored by bank (0 bg/fg, 1 sprites) and palette; view=map: one background layer (0–3) composed at 1× as PNG plus scroll/enable/priority; view=oam: the sprites composited on a blank 320×240 screen as PNG plus one text row per OAM entry; view=palettes: both palette banks as RGB565 hex (no image). Use it for 'why is this sprite garbage' — the answer is usually a wrong palette or tile index visible here.",
          "inputSchema": with(json!({
              "view": { "type": "string", "enum": ["tiles", "map", "oam", "palettes"] },
              "bank": { "type": "number", "description": "tiles: 0 bg/fg, 1 sprites (default 0)" },
              "palette": { "type": "number", "description": "tiles: palette index (default 0)" },
              "layer": { "type": "number", "description": "map: layer 0–3 (default 0)" }
          })) },
```

Arm:

```rust
        "emu_debug" => {
            let view = match arg_str(args, "view").as_deref() {
                Some("tiles") => ggo_emu_remote::protocol::DebugView::Tiles,
                Some("map") => ggo_emu_remote::protocol::DebugView::Map,
                Some("oam") => ggo_emu_remote::protocol::DebugView::Oam,
                Some("palettes") => ggo_emu_remote::protocol::DebugView::Palettes,
                other => return Err(format!("view must be tiles|map|oam|palettes, got {other:?}")),
            };
            let index = |key: &str| arg_i64(args, key).filter(|n| *n >= 0).unwrap_or(0) as usize;
            let mut data = send(
                &session.socket,
                Cmd::Debug { workspace, view, bank: index("bank"), palette: index("palette"), layer: index("layer") },
                CALL_TIMEOUT,
                connect,
            )?;
            let image = data.as_object_mut().and_then(|o| o.remove("image"));
            let mut content = vec![json!({ "type": "text", "text": data.to_string() })];
            if let Some(image) = image {
                content.push(image_content(&image)?);
            }
            Ok(content)
        }
```

Names test: append `"emu_debug",`. Test:

```rust
    #[test]
    fn emu_debug_splits_the_image_out_as_png_content() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]);
        let connect = move |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""view":"map""#) && line.contains(r#""layer":2"#), "{line}");
            Ok(format!(r#"{{"id":1,"ok":true,"data":{{"view":"map","layer":2,"image":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}}}"#))
        };
        let (content, is_err) = call_tool("emu_debug", &json!({"view": "map", "layer": 2}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(!content[0]["text"].as_str().unwrap().contains("bgra_base64"), "image moved out of the text");
        assert_eq!(content[1]["type"], "image");
        let (content, is_err) = call_tool("emu_debug", &json!({"view": "sprites"}), dir.path(), &connect);
        assert!(is_err && content[0]["text"].as_str().unwrap().contains("view must be"), "{content:?}");
    }
```

AGENTS.md row:

```
| `emu_debug { view, bank?, palette?, layer? }` | PPU inspector: tiles / map / oam as PNG + data, palettes as hex |
```

- [ ] **Step 5: Verify and commit**

```bash
./script/clippy -p ggo_emu_remote -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp \
  && git add -A && git commit -m "feat: emu_debug MCP tool exposes the PPU inspector"
```

---

### Task 5: `cart_pack`

**Files:**
- Modify: `crates/ggo/emu_remote/src/protocol.rs`
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (`prepare_world_build` ~line 1840; new `remote_pack`)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs`
- Modify: `crates/ggo/emu_mcp/src/tools.rs`, `crates/ggo/emu_mcp/AGENTS.md`

**Interfaces:**
- Consumes: `EmuPanel::prepare_world_build(&mut self, world_rel, cx) -> Option<(ProcRequest, ProcRunner, String)>` (exists; reports failures on the status row), `ggo_common::{ProcRunner, ProcCapture, failure_reason}`, `ggo_world_panel::world_stem`.
- Produces: `Cmd::PackWorld { workspace, world }`, `EmuPanel::remote_pack_plan(&mut self, world: &str, cx) -> Result<(ProcRequest, ProcRunner, String), String>`.

- [ ] **Step 1: Protocol**

```rust
    /// `emd pack-ggo` for `world` (a stem like `worlds/arena` or a rel
    /// path like `assets/worlds/arena.toml`), into the project's
    /// `target/ggo-emulate/`. The reply names the cart for `Start`/`Run`.
    PackWorld { workspace: Option<String>, world: String },
```

Test:

```rust
    #[test]
    fn pack_world_request_round_trips() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"pack_world","world":"worlds/arena"}"#).unwrap().cmd,
            Cmd::PackWorld { workspace: None, world: "worlds/arena".to_string() }
        );
    }
```

- [ ] **Step 2: `remote_pack_plan` on the panel**

`prepare_world_build` takes a worktree-relative world FILE path and reports failures to the status row, returning `None`. The agent needs the reason as a value. Add after `prepare_world_build`:

```rust
    /// [`Self::prepare_world_build`] for the agent socket: the same plan,
    /// with the reason it could not be made RETURNED rather than shown.
    /// `world` may be a stem (`worlds/arena`) or a rel path
    /// (`assets/worlds/arena.toml`); a stem is resolved against the
    /// project's world listing.
    pub(crate) fn remote_pack_plan(
        &mut self,
        world: &str,
        cx: &mut Context<Self>,
    ) -> Result<(ggo_common::ProcRequest, ggo_common::ProcRunner, String), String> {
        let world_rel = if ggo_world_panel::world_stem(world).is_some() {
            world.to_string()
        } else {
            // A bare stem: find the file. `assets/worlds/x.toml` first (real
            // projects), then `worlds/x.toml` (fixtures).
            let root = self.project_root.clone().ok_or("no project folder is open")?;
            ["assets/", ""]
                .iter()
                .map(|prefix| format!("{prefix}{world}.toml"))
                .find(|rel| root.join(rel).is_file())
                .ok_or_else(|| format!("no world file for stem {world} (looked for assets/{world}.toml and {world}.toml)"))?
        };
        let before = self.status.clone();
        let plan = self.prepare_world_build(&world_rel, cx);
        match plan {
            Some(plan) => Ok(plan),
            None => {
                let reason = self.status.clone().unwrap_or_else(|| "could not plan the pack".to_string());
                self.status = before;
                Err(reason)
            }
        }
    }
```

Test in the panel tests module (there are existing `prepare_world_build`/emulate tests near line 7873 that build a fixture project with `emerald.toml` and a world; copy their fixture setup — look for the helper that writes `emerald.toml`, likely named `write_emerald_project` or used in `test_emulate_world_*`):

```rust
    #[gpui::test]
    async fn test_remote_pack_plan_resolves_a_stem_and_returns_the_reason_on_failure(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, _worktree_id, cx) = run_menu_workspace(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let err = panel.remote_pack_plan("worlds/nope", cx).unwrap_err();
            assert!(err.contains("no world file for stem worlds/nope"), "{err}");
        });
        // A real world under an emerald project packs.
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\nname = \"g\"\ndefault_world = \"worlds/arena\"\n").unwrap();
        std::fs::write(dir.path().join("assets/worlds/arena.toml"), "").unwrap();
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            let (request, _runner, cart) = panel.remote_pack_plan("worlds/arena", cx).expect("plans");
            assert!(request.args.iter().any(|a| a == "pack-ggo"), "{:?}", request.args);
            assert!(request.args.iter().any(|a| a == "worlds/arena"), "{:?}", request.args);
            assert_eq!(cart, "target/ggo-emulate/worlds-arena.ggo");
        });
    }
```

Adjust `emerald.toml` contents to whatever `ggo_common::emerald_project_root` needs (it looks for `ggo_common::EMERALD_MANIFEST` by name; the file's contents may be irrelevant — check `emerald_project_root` at `ggo_common.rs:625`).

- [ ] **Step 3: Host arm**

`workspace_arg`: `| Cmd::PackWorld { workspace, .. }`. Needs a panel (reopen if closed, like `FlashWorld`) and a window. Dispatch arm:

```rust
        Cmd::PackWorld { world, .. } => {
            let panel = panel_or_open(&target_root, target.panel, target.workspace, window, cx)?;
            let (request, runner, cart) = panel
                .update(cx, |p, cx| p.remote_pack_plan(&world, cx))
                .map_err(|e| e.to_string())??;
            // BLOCKING child: the runner is the panel's own (a test's fake
            // or the system one), run off the UI thread.
            let capture = cx.background_spawn(async move { runner(request) }).await;
            if !capture.ok {
                return Err(format!("pack failed: {}", ggo_common::failure_reason(&capture)));
            }
            let tail: Vec<&String> = capture.lines.iter().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
            Ok(serde_json::json!({ "cart": cart, "world": world, "lines": tail }))
        }
```

`ggo_common::failure_reason` exists (`menu.rs` re-exports it as `menu::failure_reason`); use whichever is public from `agent_remote.rs`.

- [ ] **Step 4: Bridge tool**

Insert before `hw_flash` in `tool_list`:

```rust
        { "name": "cart_pack",
          "description": "Build one world into a runnable cart: `emd pack-ggo --world <stem>` into the project's target/ggo-emulate/. Takes a world stem (worlds/arena) or file (assets/worlds/arena.toml). Returns {cart, world, lines}: hand `cart` to emu_start or emu_run. Blocks until the pack finishes (a cold build can take minutes; the 15s socket timeout is raised for this call). Errors carry emd's own failure line.",
          "inputSchema": with(json!({ "world": { "type": "string", "description": "World stem, e.g. worlds/arena" } })) },
```

Arm, with a longer timeout constant added near `CALL_TIMEOUT`:

```rust
/// A pack is a cargo build; the lock-step bound would cut it off.
const PACK_TIMEOUT: Duration = Duration::from_secs(600);
```

```rust
        "cart_pack" => {
            let world = arg_str(args, "world").ok_or("missing required argument: world (a stem like worlds/arena)")?;
            let data = send(&session.socket, Cmd::PackWorld { workspace, world }, PACK_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
```

Names test: insert `"cart_pack",` before `"hw_flash"`. Test:

```rust
    #[test]
    fn cart_pack_forwards_the_world_with_the_long_timeout() {
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let connect = |_: &Path, line: &str, timeout: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"pack_world""#) && line.contains(r#""world":"worlds/arena""#), "{line}");
            assert_eq!(timeout, PACK_TIMEOUT);
            Ok(r#"{"id":1,"ok":true,"data":{"cart":"target/ggo-emulate/worlds-arena.ggo","world":"worlds/arena","lines":[]}}"#.to_string())
        };
        let (content, is_err) = call_tool("cart_pack", &json!({"world": "worlds/arena"}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert!(content[0]["text"].as_str().unwrap().contains("worlds-arena.ggo"));
    }
```

Also update the `emu_start` description: replace `Pack first: emd pack-ggo [--world <stem>].` with `Pack first with cart_pack (or emd pack-ggo --world <stem>).`

AGENTS.md row before `hw_flash`:

```
| `cart_pack { world }` | `emd pack-ggo` one world into `target/ggo-emulate/`; `{ cart }` feeds `emu_start`/`emu_run` |
```

- [ ] **Step 5: Verify and commit**

```bash
./script/clippy -p ggo_emu_remote -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp \
  && git add -A && git commit -m "feat: cart_pack MCP tool packs a world without a shell"
```

---

### Task 6: `world_list`, `world_open`, `world_read`

**Files:**
- Modify: `crates/ggo/emu_remote/src/protocol.rs`
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (new `remote_*` block after the `test_*` readers ~line 2640)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs` (`world_panel_for` helper; arms)
- Modify: `crates/ggo/emu_mcp/src/tools.rs`, `crates/ggo/emu_mcp/AGENTS.md`

**Interfaces:**
- Consumes: `WorldPanel::{refresh_worlds(cx), load_rel_path(rel, cx), open_rel_path(rel, window, cx), open_rel_path_now() -> Option<&str>, open_world_stem() -> Option<String>, dirty_world_name()}`, `OpenWorld::{store.state(), selected, listing, source_rel}`, `WorldState`, `ggo_worldlib::render::Selection`, `inspector::entity_pos`. `Workspace::panel::<WorldPanel>(cx)`.
- Produces: `Cmd::WorldList { workspace }`, `Cmd::WorldOpen { workspace, world }`, `Cmd::WorldRead { workspace, world }`, `WorldPanel::remote_list(&mut self, cx) -> Vec<(String, String)>`, `WorldPanel::remote_open(&mut self, world: &str, window, cx) -> Result<String, String>`, `WorldPanel::remote_read(&self) -> Result<serde_json::Value, String>`, `fn world_panel_for(workspace: &Entity<Workspace>, window, cx) -> Result<Entity<WorldPanel>, String>` in `agent_remote.rs`. Task 7 reuses `world_panel_for` and `remote_open`.

- [ ] **Step 1: Protocol**

```rust
    /// Every world file in the project: `[{stem, rel_path}]`.
    WorldList { workspace: Option<String> },
    /// Open `world` (stem or rel path) in the World panel, as a click
    /// would; the reply names the rel path that opened.
    WorldOpen { workspace: Option<String>, world: String },
    /// The open world as authored: entities with their components,
    /// instances, backgrounds, selection, dirty flag. With `world`, opens
    /// it first.
    WorldRead {
        workspace: Option<String>,
        #[serde(default)]
        world: Option<String>,
    },
```

Test:

```rust
    #[test]
    fn world_requests_round_trip() {
        assert_eq!(parse_request(r#"{"id":1,"cmd":"world_list"}"#).unwrap().cmd, Cmd::WorldList { workspace: None });
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":"world_open","world":"worlds/arena"}"#).unwrap().cmd,
            Cmd::WorldOpen { workspace: None, world: "worlds/arena".to_string() }
        );
        assert_eq!(
            parse_request(r#"{"id":3,"cmd":"world_read"}"#).unwrap().cmd,
            Cmd::WorldRead { workspace: None, world: None }
        );
    }
```

- [ ] **Step 2: `remote_*` on `WorldPanel` with tests**

After `test_selected_count` (outside any `#[cfg(feature = "test-support")]`):

```rust
    // ------------------------------------------------------------ agent socket

    /// Every world in the project, `(stem, rel_path)`, refreshed now.
    pub fn remote_list(&mut self, cx: &mut Context<Self>) -> Vec<(String, String)> {
        self.refresh_worlds(cx);
        self.worlds.iter().map(|w| (w.stem.clone(), w.rel_path.clone())).collect()
    }

    /// The listing entry `world` names -- a stem (`worlds/arena`) or its
    /// rel path -- or the reason there is none.
    fn remote_resolve(&mut self, world: &str, cx: &mut Context<Self>) -> Result<String, String> {
        self.refresh_worlds(cx);
        self.worlds
            .iter()
            .find(|w| w.stem == world || w.rel_path == world)
            .map(|w| w.rel_path.clone())
            .ok_or_else(|| {
                let stems: Vec<&str> = self.worlds.iter().map(|w| w.stem.as_str()).collect();
                format!("no world {world}; the project has {stems:?}")
            })
    }

    /// Open `world` as a click on its file would (the already-open world
    /// is left alone: no reload, no prompt). Returns the rel path opened.
    pub fn remote_open(
        &mut self,
        world: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let rel = self.remote_resolve(world, cx)?;
        self.open_rel_path(&rel, window, cx);
        Ok(rel)
    }

    /// The open world as authored. `Err` while nothing is open or a load
    /// is still in flight, so the caller can wait and ask again.
    pub fn remote_read(&self) -> Result<serde_json::Value, String> {
        let open = match &self.state {
            ViewerState::Ready(open) => open,
            ViewerState::Empty => return Err("no world open — world_open first".to_string()),
            ViewerState::Loading { stem } => return Err(format!("{stem} is still loading")),
            ViewerState::Error(error) => return Err(format!("the open world failed to load: {error}")),
        };
        let state = open.store.state();
        let entities: Vec<serde_json::Value> = state
            .entities
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                serde_json::json!({
                    "index": index,
                    "pos": inspector::entity_pos(&state, index),
                    "components": serde_json::Value::Object(entity.components.clone()),
                })
            })
            .collect();
        let instances: Vec<serde_json::Value> = state
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| {
                serde_json::json!({
                    "index": index,
                    "world": instance.world,
                    "pos": instance.pos,
                    "background_priority": instance.background_priority,
                    "error": instance.error,
                })
            })
            .collect();
        let backgrounds: Vec<serde_json::Value> = state
            .backgrounds
            .iter()
            .map(|background| serde_json::json!({ "layer": background.layer, "map": background.map }))
            .collect();
        let selected: Vec<serde_json::Value> = open
            .selected
            .iter()
            .map(|selection| match selection {
                Selection::Entity(index) => serde_json::json!({ "entity": index }),
                Selection::Instance(index) => serde_json::json!({ "instance": index }),
            })
            .collect();
        Ok(serde_json::json!({
            "stem": open.listing.stem,
            "rel_path": open.source_rel,
            "dirty": state.dirty,
            "entities": entities,
            "instances": instances,
            "backgrounds": backgrounds,
            "selected": selected,
        }))
    }
```

Check that `Selection` is imported in `ggo_world_panel.rs` (it is used by `OpenWorld::selected`); check `WorldInstance.error`'s type (`Option<String>` per `WorldDocWire`); check `inspector::entity_pos` is reachable (module `inspector` is a sibling file in the crate).

Tests in the world panel tests module, using the existing `write_fixture` and `ready_panel` helpers (`ready_panel` loads `worlds/test` and returns a Ready panel):

```rust
    #[gpui::test]
    async fn test_remote_list_and_read_report_the_authored_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let listed = panel.remote_list(cx);
            let stems: Vec<&str> = listed.iter().map(|(stem, _)| stem.as_str()).collect();
            assert_eq!(stems, ["worlds/sub", "worlds/test"]);

            let read = panel.remote_read().expect("a Ready world reads");
            assert_eq!(read["stem"], "worlds/test");
            assert_eq!(read["dirty"], false);
            assert_eq!(read["entities"].as_array().unwrap().len(), 3);
            assert_eq!(read["entities"][1]["pos"], serde_json::json!([40.0, 8.0]));
            assert_eq!(read["entities"][1]["components"]["Text"]["content"], "hello");
            assert_eq!(read["instances"][0]["world"], "worlds/sub");
            assert_eq!(read["selected"].as_array().unwrap().len(), 0);

            assert!(panel.remote_resolve("worlds/nope", cx).unwrap_err().contains("worlds/test"));
        });
    }

    #[gpui::test]
    async fn test_remote_read_names_the_reason_when_nothing_is_open(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });
        panel.update(cx, |panel, _cx| {
            assert!(panel.remote_read().unwrap_err().contains("world_open first"));
        });
    }
```

- [ ] **Step 3: Host: `world_panel_for` and arms**

In `agent_remote.rs`, add a helper near `panel_or_open`:

```rust
/// The workspace's World panel (a dock panel `ggo_world_panel::init`
/// registers on every workspace), leased through its window.
fn world_panel_for(
    workspace: &Entity<Workspace>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<Entity<ggo_world_panel::WorldPanel>, String> {
    window
        .update(cx, |_, _window, app| {
            workspace
                .read(app)
                .panel::<ggo_world_panel::WorldPanel>(app)
                .ok_or_else(|| "no World panel in this workspace".to_string())
        })
        .map_err(|e| e.to_string())?
}
```

`workspace_arg`: `| Cmd::WorldList { workspace } | Cmd::WorldOpen { workspace, .. } | Cmd::WorldRead { workspace, .. }`.

Dispatch arms (after `let window = ...`, inside the match):

```rust
        Cmd::WorldList { .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let panel = world_panel_for(&workspace, window, cx)?;
            let worlds = panel.update(cx, |p, cx| p.remote_list(cx)).map_err(|e| e.to_string())?;
            let rows: Vec<serde_json::Value> = worlds
                .into_iter()
                .map(|(stem, rel_path)| serde_json::json!({ "stem": stem, "rel_path": rel_path }))
                .collect();
            Ok(serde_json::json!({ "worlds": rows }))
        }
        Cmd::WorldOpen { world, .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let panel = world_panel_for(&workspace, window, cx)?;
            let rel = window
                .update(cx, |_, window, app| {
                    panel.update(app, |p, cx| p.remote_open(&world, window, cx))
                })
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "opened": rel }))
        }
        Cmd::WorldRead { world, .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let panel = world_panel_for(&workspace, window, cx)?;
            if let Some(world) = world {
                window
                    .update(cx, |_, window, app| {
                        panel.update(app, |p, cx| p.remote_open(&world, window, cx))
                    })
                    .map_err(|e| e.to_string())??;
            }
            // The load is off-thread; give it a bounded moment to land.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let read = panel.update(cx, |p, _| p.remote_read()).map_err(|e| e.to_string())?;
                match read {
                    Ok(value) => return Ok(value),
                    Err(reason) if reason.contains("still loading") && std::time::Instant::now() < deadline => {
                        cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
                    }
                    Err(reason) => return Err(reason),
                }
            }
        }
```

Check whether `open_rel_path` requires the world to be worktree-relative (`assets/worlds/x.toml`) while the listing's `rel_path` is root-relative; `remote_resolve` returns the listing's `rel_path`, which is what `load_rel_path` takes in the fixture test — if `open_rel_path` expects the clicked (worktree) form, use `self.load_rel_path(&rel, cx)` in `remote_open` instead and drop the `window` parameter (then simplify the host arms accordingly).

- [ ] **Step 4: Bridge tools**

Insert after `close_ggo_report` in `tool_list`:

```rust
        { "name": "world_list",
          "description": "Every world file in the open project: [{stem, rel_path}]. Stems are what emd, cart_pack and hw_flash take (worlds/arena).",
          "inputSchema": with(json!({})) },
        { "name": "world_open",
          "description": "Open a world in Zed's World panel, as clicking its file would. Returns {opened: rel_path}. The already-open world is left alone (no reload, no prompt).",
          "inputSchema": with(json!({ "world": { "type": "string", "description": "World stem (worlds/arena) or rel path" } })) },
        { "name": "world_read",
          "description": "The world as the designer authored it, from the World panel: {stem, rel_path, dirty, entities[{index, pos, components}], instances[{index, world, pos, background_priority, error}], backgrounds[{layer, map}], selected[]}. Pass `world` to open one first. This is the level layout — what world_screenshot draws and what the cart boots — not the running game (that is emu_next_frame's world JSON).",
          "inputSchema": with(json!({ "world": { "type": "string", "description": "Open this world first (stem or rel path); omit to read the one already open" } })) },
```

Arms:

```rust
        "world_list" => {
            let data = send(&session.socket, Cmd::WorldList { workspace }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "world_open" => {
            let world = arg_str(args, "world").ok_or("missing required argument: world")?;
            let data = send(&session.socket, Cmd::WorldOpen { workspace, world }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
        "world_read" => {
            let world = arg_str(args, "world");
            let data = send(&session.socket, Cmd::WorldRead { workspace, world }, CALL_TIMEOUT, connect)?;
            Ok(vec![json!({ "type": "text", "text": data.to_string() })])
        }
```

Names test: append `"world_list", "world_open", "world_read",` at the end. Test:

```rust
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
```

AGENTS.md rows (new section header `## World` or rows at the table end):

```
| `world_list` | every world in the project: `[{ stem, rel_path }]` |
| `world_open { world }` | open a world in the World panel |
| `world_read { world? }` | the authored world: entities/components/pos, instances, backgrounds, selection, dirty |
```

- [ ] **Step 5: Verify and commit**

```bash
./script/clippy -p ggo_emu_remote -p ggo_world_panel -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_world_panel -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp \
  && git add -A && git commit -m "feat: world_list, world_open, world_read MCP tools"
```

---

### Task 7: `world_screenshot`

**Files:**
- Modify: `crates/ggo/emu_remote/src/protocol.rs`
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (pure `composite_scene`; `remote_screenshot`)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs`
- Modify: `crates/ggo/emu_mcp/src/tools.rs`, `crates/ggo/emu_mcp/AGENTS.md`

**Interfaces:**
- Consumes: `fn draw_items(open: &OpenWorld) -> Vec<DrawItem>` (`ggo_world_panel.rs:932`), `ggo_worldlib::render::{DrawItem, DrawKind, RgbaImage, active_camera_origin, DEVICE_SCREEN_W, DEVICE_SCREEN_H}`, `world_panel_for` and `remote_open` (Task 6), `bgra_reply` (Task 2), `image_content` (Task 2).
- Produces: `Cmd::WorldScreenshot { workspace, world, full }`, `pub fn composite_scene(items: &[DrawItem], origin: [f64; 2], width: u32, height: u32) -> Vec<u8>` (BGRA), `WorldPanel::remote_screenshot(&self, full: bool) -> Result<(u32, u32, Vec<u8>), String>`.

- [ ] **Step 1: Protocol**

```rust
    /// The authored world drawn to pixels: the 320x240 device screen at
    /// the active camera by default, the whole scene's bounding box with
    /// `full`. `{width, height, bgra_base64}`.
    WorldScreenshot {
        workspace: Option<String>,
        #[serde(default)]
        world: Option<String>,
        #[serde(default)]
        full: bool,
    },
```

Test:

```rust
    #[test]
    fn world_screenshot_defaults_to_the_device_screen() {
        assert_eq!(
            parse_request(r#"{"id":1,"cmd":"world_screenshot"}"#).unwrap().cmd,
            Cmd::WorldScreenshot { workspace: None, world: None, full: false }
        );
        assert_eq!(
            parse_request(r#"{"id":2,"cmd":"world_screenshot","world":"worlds/a","full":true}"#).unwrap().cmd,
            Cmd::WorldScreenshot { workspace: None, world: Some("worlds/a".to_string()), full: true }
        );
    }
```

- [ ] **Step 2: Pure compositor with a unit test**

Add near `draw_items` in `ggo_world_panel.rs` (free function, no gpui):

```rust
/// Software-composite a draw list into a BGRA canvas whose top-left is
/// world point `origin`. Images blit with source alpha; the gizmo kinds
/// (`Marker`, `Placeholder`, `InstanceOrigin`, `Text`) draw as flat
/// boxes -- the agent's picture is of the LAYOUT, not the editor's
/// chrome, so `SelectionOutline` is skipped. Items paint in draw-list
/// order, which `build_draw_list_multi` has already sorted by z.
pub fn composite_scene(items: &[DrawItem], origin: [f64; 2], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as i64, height as i64);
    let mut canvas = vec![0u8; (w * h * 4) as usize];
    let mut put = |x: i64, y: i64, rgba: [u8; 4]| {
        if x < 0 || y < 0 || x >= w || y >= h || rgba[3] == 0 {
            return;
        }
        let i = ((y * w + x) * 4) as usize;
        let a = rgba[3] as u32;
        for (c, src) in [(2usize, rgba[0]), (1, rgba[1]), (0, rgba[2])] {
            let dst = canvas[i + c] as u32;
            canvas[i + c] = ((src as u32 * a + dst * (255 - a)) / 255) as u8;
        }
        canvas[i + 3] = 255;
    };
    for item in items {
        let x0 = (item.x - origin[0]).round() as i64;
        let y0 = (item.y - origin[1]).round() as i64;
        match &item.kind {
            DrawKind::Image { image } => {
                for sy in 0..image.h as i64 {
                    for sx in 0..image.w as i64 {
                        let s = ((sy * image.w as i64 + sx) * 4) as usize;
                        let px = [image.rgba[s], image.rgba[s + 1], image.rgba[s + 2], image.rgba[s + 3]];
                        put(x0 + sx, y0 + sy, px);
                    }
                }
            }
            DrawKind::SelectionOutline => {}
            DrawKind::Text { .. } | DrawKind::Marker | DrawKind::Placeholder { .. } | DrawKind::InstanceOrigin => {
                let color = match &item.kind {
                    DrawKind::Text { .. } => [255, 255, 255, 160],
                    DrawKind::Placeholder { .. } => [255, 64, 64, 200],
                    _ => [96, 200, 255, 160],
                };
                for dy in 0..item.h.round() as i64 {
                    for dx in 0..item.w.round() as i64 {
                        put(x0 + dx, y0 + dy, color);
                    }
                }
            }
        }
    }
    canvas
}
```

Unit test in the tests module (pure, no gpui):

```rust
    #[test]
    fn composite_scene_blits_images_and_boxes_relative_to_the_origin() {
        let red: Arc<[u8]> = vec![255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255].into();
        let items = vec![
            DrawItem {
                kind: DrawKind::Image { image: RgbaImage { rgba: red, w: 2, h: 2 } },
                x: 10.0, y: 10.0, w: 2.0, h: 2.0, z: 0.0, order: 0, sel: None,
            },
            DrawItem {
                kind: DrawKind::SelectionOutline,
                x: 0.0, y: 0.0, w: 100.0, h: 100.0, z: 1.0, order: 1, sel: None,
            },
        ];
        let canvas = composite_scene(&items, [8.0, 8.0], 8, 8);
        // Image landed at canvas (2,2): BGRA red, opaque.
        let i = (2 * 8 + 2) * 4;
        assert_eq!(&canvas[i..i + 4], &[0, 0, 255, 255]);
        // Outline drew nothing: (0,0) stays transparent black.
        assert_eq!(&canvas[0..4], &[0, 0, 0, 0]);
        // Outside the image: untouched.
        let j = (5 * 8 + 5) * 4;
        assert_eq!(&canvas[j..j + 4], &[0, 0, 0, 0]);
    }
```

Check `RgbaImage`/`DrawItem`/`DrawKind` imports in the tests module (`use ggo_worldlib::render::{...}`); `RgbaImage.rgba` is `Arc<[u8]>`.

- [ ] **Step 3: `remote_screenshot` on the panel with a test**

In the agent-socket block added in Task 6:

```rust
    /// The open world drawn to pixels. Default framing is the device
    /// screen (320x240) at the active camera -- what the board shows on
    /// boot; `full` frames the whole scene's bounding box instead.
    /// `(width, height, BGRA)`.
    pub fn remote_screenshot(&self, full: bool) -> Result<(u32, u32, Vec<u8>), String> {
        let ViewerState::Ready(open) = &self.state else {
            return Err(self.remote_read().err().unwrap_or_else(|| "no world open".to_string()));
        };
        let items = draw_items(open);
        let (origin, width, height) = if full {
            let mut min = [f64::INFINITY; 2];
            let mut max = [f64::NEG_INFINITY; 2];
            for item in items.iter().filter(|i| !matches!(i.kind, DrawKind::SelectionOutline)) {
                min = [min[0].min(item.x), min[1].min(item.y)];
                max = [max[0].max(item.x + item.w), max[1].max(item.y + item.h)];
            }
            if !min[0].is_finite() {
                return Err("the world draws nothing".to_string());
            }
            let width = ((max[0] - min[0]).ceil() as u32).clamp(1, 4096);
            let height = ((max[1] - min[1]).ceil() as u32).clamp(1, 4096);
            (min, width, height)
        } else {
            (
                active_camera_origin(&open.store.state()),
                DEVICE_SCREEN_W as u32,
                DEVICE_SCREEN_H as u32,
            )
        };
        Ok((width, height, composite_scene(&items, origin, width, height)))
    }
```

Check `active_camera_origin`, `DEVICE_SCREEN_W/H`, `DrawKind` are imported at the top of `ggo_world_panel.rs` (`draw_items` already imports `build_draw_list_multi` from `ggo_worldlib::render`). Test:

```rust
    #[gpui::test]
    async fn test_remote_screenshot_frames_the_device_screen_or_the_whole_scene(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            let (w, h, bgra) = panel.remote_screenshot(false).expect("Ready draws");
            assert_eq!((w, h), (320, 240));
            assert_eq!(bgra.len(), 320 * 240 * 4);
            // The fixture's Text at (40,8) 40x12 paints a box: a pixel
            // inside it is opaque, one far outside the scene is not.
            let inside = ((10 * 320 + 45) * 4) as usize;
            assert_eq!(bgra[inside + 3], 255, "text box painted");
            let outside = ((230 * 320 + 300) * 4) as usize;
            assert_eq!(bgra[outside + 3], 0, "empty world pixel");

            let (w, h, _) = panel.remote_screenshot(true).expect("full frames the bbox");
            assert!(w > 0 && h > 0 && w <= 320, "the fixture scene is smaller than a screen: {w}x{h}");
        });
    }
```

If the fixture's Camera at `(0,0)` is not what `active_camera_origin` returns as the top-left (it may center the screen on the camera), adjust the `inside` pixel by reading `active_camera_origin(&state)` in the test and offsetting (45 - origin.x, 10 - origin.y).

- [ ] **Step 4: Host arm**

`workspace_arg`: `| Cmd::WorldScreenshot { workspace, .. }`. Arm:

```rust
        Cmd::WorldScreenshot { world, full, .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let panel = world_panel_for(&workspace, window, cx)?;
            if let Some(world) = world {
                window
                    .update(cx, |_, window, app| {
                        panel.update(app, |p, cx| p.remote_open(&world, window, cx))
                    })
                    .map_err(|e| e.to_string())??;
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let shot = panel.update(cx, |p, _| p.remote_screenshot(full)).map_err(|e| e.to_string())?;
                match shot {
                    Ok((w, h, bgra)) => return Ok(bgra_reply(w, h, &bgra)),
                    Err(reason) if reason.contains("still loading") && std::time::Instant::now() < deadline => {
                        cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
                    }
                    Err(reason) => return Err(reason),
                }
            }
        }
```

The wait loop is the same as `WorldRead`'s; extract `async fn await_world_ready<T>(panel, cx, read: impl Fn(&WorldPanel) -> Result<T, String>) -> Result<T, String>` and use it in both arms.

- [ ] **Step 5: Bridge tool**

After `world_read`:

```rust
        { "name": "world_screenshot",
          "description": "The authored world drawn to a PNG from the World panel — the level layout as designed, not a running frame. Default: the 320×240 device screen at the world's active camera (what the board shows on boot). full=true: the whole scene's bounding box (capped 4096²). Sprites, tilemaps and backgrounds render with real pixels; text and placeholder entities draw as flat boxes. Pass `world` to open one first.",
          "inputSchema": with(json!({
              "world": { "type": "string", "description": "Open this world first (stem or rel path); omit for the open one" },
              "full": { "type": "boolean", "description": "Frame the whole scene instead of the device screen" }
          })) },
```

Arm:

```rust
        "world_screenshot" => {
            let world = arg_str(args, "world");
            let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
            let data = send(&session.socket, Cmd::WorldScreenshot { workspace, world, full }, CALL_TIMEOUT, connect)?;
            Ok(vec![image_content(&data)?])
        }
```

Names test: append `"world_screenshot",`. Test:

```rust
    #[test]
    fn world_screenshot_is_png_content() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        fake_session(dir.path(), std::process::id());
        let bgra = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 255, 255]);
        let connect = move |_: &Path, line: &str, _: Duration| -> std::io::Result<String> {
            assert!(line.contains(r#""cmd":"world_screenshot""#) && line.contains(r#""full":true"#), "{line}");
            Ok(format!(r#"{{"id":1,"ok":true,"data":{{"width":1,"height":1,"bgra_base64":"{bgra}"}}}}"#))
        };
        let (content, is_err) = call_tool("world_screenshot", &json!({"full": true}), dir.path(), &connect);
        assert!(!is_err, "{content:?}");
        assert_eq!(content[0]["type"], "image");
    }
```

AGENTS.md row:

```
| `world_screenshot { world?, full? }` | the authored world as PNG: device screen at the camera, or the whole scene |
```

- [ ] **Step 6: Verify and commit**

```bash
./script/clippy -p ggo_emu_remote -p ggo_world_panel -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_world_panel -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp \
  && git add -A && git commit -m "feat: world_screenshot MCP tool draws the authored world"
```

---

### Task 8: Sweep, install, review

**Files:**
- Modify: `crates/ggo/emu_mcp/AGENTS.md` (tool table order matches `tool_list`; the `## Hardware` paragraph mentions `hw_env` before `hw_flash`)
- Modify: `crates/ggo/emu_mcp/src/tools.rs` (only if the names test order and `tool_list` disagree)

- [ ] **Step 1: Confirm the tool list order and the doc table agree**

Run: `grep -n '"name": "' crates/ggo/emu_mcp/src/tools.rs | sed 's/.*"name": "\([a-z_]*\)".*/\1/'` and compare with the table in `AGENTS.md`. Expected final order:

```
zed_sessions, emu_status, emu_start, emu_next_frame, emu_stop, emu_screenshot, emu_uart, emu_run, emu_pause, emu_resume, emu_debug, cart_pack, hw_flash, hw_flash_status, hw_flash_wait, hw_env, hw_flash_cancel, list_ggo_reports, fetch_ggo_report, open_ggo_report, close_ggo_report, world_list, world_open, world_read, world_screenshot
```

Edit `AGENTS.md`'s `## Hardware` paragraph to begin: "Call `hw_env` first: a missing prerequisite there is what `hw_flash` would fail on." Fix any drift.

- [ ] **Step 2: Full verification across the four crates**

```bash
./script/clippy -p ggo_emu_remote -p ggo_world_panel -p ggo_emu_panel -p ggo_emu_mcp \
  && cargo test -p ggo_emu_remote -p ggo_world_panel -p ggo_emu_panel --lib \
  && cargo test -p ggo_emu_mcp
```

Expected: all pass. Fix anything that does not before continuing.

- [ ] **Step 3: Install the bridge**

```bash
cargo install --path crates/ggo/emu_mcp --force && zedgg-emu-mcp </dev/null; echo "exit $?"
```

Expected: `Replaced package ...`; the binary exits 0 on empty stdin.

- [ ] **Step 4: Commit any doc drift**

```bash
git add -A && git diff --cached --quiet || git commit -m "docs: MCP tool table matches the Tier 1 surface"
```

- [ ] **Step 5: Fresh-context review (user's global rule)**

Dispatch a fresh **opus** subagent to review `git diff <commit-before-task-1>..HEAD` for (1) best practices per `CLAUDE.md` and `crates/ggo/.rules` — entity leasing, no `let _ =`, no unwraps outside tests, no summarizing comments — and (2) whether each Tier 1 goal is met: `hw_env` reports every missing prerequisite by code; `hw_flash_cancel` never marks a run failed; `emu_uart` works mid-run; `emu_debug` bounds-checks bank/palette/layer; `cart_pack` returns emd's failure line on error and the cart path on success; `world_read` reports pos, components, instances, backgrounds, selection, dirty; `world_screenshot` frames the device screen at the camera by default. Fix findings, re-run Step 2, commit, then `git push origin ggo ggo:main` only if the user has asked for a push in this session.

---

## Self-review notes

- Spec coverage: Tier 1 rows in the review map to Tasks 1 (hw_env, hw_flash_cancel), 2–4 (emu_screenshot, emu_uart, emu_run/pause/resume, emu_debug), 5 (cart_pack), 6–7 (world_list/open/read/screenshot). The review's `world_open` is in Task 6. Nothing from Tier 2 is included.
- Type consistency: `bgra_reply` (Task 2) is reused by Tasks 4 and 7; `image_content` (Task 2) by Tasks 4 and 7; `world_panel_for` and `remote_open` (Task 6) by Task 7; `HwEnvPayload` is produced in Task 1 and consumed only there. `DebugView` variants match the bridge's string mapping in Task 4.
- Known verification points the implementer must resolve on contact (each called out in its task): whether `open_rel_path` takes the listing's rel path or the worktree-relative click path (Task 6, Step 3); `oam_rows` element ownership (Task 4, Step 2); `active_camera_origin`'s framing convention for the screenshot test (Task 7, Step 3); the `emerald.toml` contents `emerald_project_root` needs (Task 5, Step 2).
