# Live World View, Phase 2 (zed emulator link) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let another panel boot the emerald viewer cart in ZedGG's in-process emulator and talk to it over the UART link, receiving the cart's frames alongside, without depending on the emu panel crate.

**Architecture:** `ggo_common` gets a transport-neutral `LinkEndpoint` (host→cart wire bytes in, cart→host APP payloads out, latest frame slot, per-frame tick) plus a `ViewerBooter` registry shaped like the existing `WorldEmulators`. `ggo_emu_panel` registers the booter: it runs `emd editor-cart --ggo`, boots the `.ggo` through its ordinary run path, and the emulator thread pumps the endpoint at every frame boundary (`uart_inject` / `take_comm` + `ggo_comm::MessageReader`). Watch mode rebuilds the editor cart on source changes.

**Tech Stack:** Rust, gpui, `ggo-emu-core` (Phase 0 sandbox comm API), `ggo-comm` (`MessageReader`), `ggo-wire` (`encode_payload`), `async-channel`.

**Spec:** `docs/superpowers/specs/2026-09-04-live-world-view-design.md`, section "ZedGG changes / ggo_emu_panel". Prerequisites: Phase 0 on ggo `main`, Phase 1 on emerald `main`.

## Global Constraints

- Work in `~/projects/zed`, branch `live-world-view`. Gate before every commit: `./script/clippy -p ggo_common -p ggo_emu_panel && cargo test -p ggo_common -p ggo_emu_panel --lib`.
- Fork hook rule (repo `CLAUDE.md`): anything that reads or updates a pane from a workspace hook goes through `cx.defer_in(window, ..)`; decide synchronously, act deferred.
- Never `let _ =` a fallible call; `.log_err()` or handle.
- The emulator thread owns `Peripherals`; the endpoint crosses threads only through `Arc<Mutex<..>>`, atomics and `async_channel`. `RenderImage` is built on the UI thread (in `on_frame`), never on the emulator thread.
- New path deps in the root `Cargo.toml`, marked `# GGO` like the existing ones: `ggo-comm = { path = "../ggo/tools/ggo-comm" }`, `ggo-wire = { path = "../ggo/firmware/ggo-wire" }`.
- Commit messages: short imperative subject, no AI trailers. Line numbers are as of 2026-09-04; locate by symbol.

---

## File structure

| Path | Responsibility |
|------|----------------|
| `crates/ggo/common/src/ggo_common.rs` | `LinkEndpoint`, `ViewerState`, `ViewerBooter` registry (`register_viewer_booter`, `boot_viewer`). Pure + gpui globals, no emulator types. |
| `crates/ggo/common/Cargo.toml` | `async-channel` dependency. |
| `crates/ggo/emu_panel/src/drive.rs` | `Session` carries `Option<Arc<LinkEndpoint>>`; the thread pumps it at each `Vsync`. |
| `crates/ggo/emu_panel/src/link.rs` | New: `pump_link(p, endpoint, reader)` (pure over `Peripherals`), unit-tested without a cart. |
| `crates/ggo/emu_panel/src/ggo_emu_panel.rs` | `boot_viewer` (build `emd editor-cart --ggo`, run with the endpoint), `RunKind`, watch-mode rebuild of the editor cart. |
| `crates/ggo/emu_panel/src/menu.rs` | `editor_cart_args()`, JSON trailer parse for the `ggo` path. |
| `crates/ggo/emu_panel/Cargo.toml` | `ggo-comm`, `ggo-wire`, `async-channel`. |

---

### Task 1: `LinkEndpoint` and the viewer booter registry in `ggo_common`

**Files:**
- Modify: `crates/ggo/common/Cargo.toml`
- Modify: `crates/ggo/common/src/ggo_common.rs` (beside `WorldEmulators` ~line 514)
- Test: inline tests in `ggo_common.rs`

**Interfaces:**
- Produces:

```rust
pub struct LinkEndpoint {
    /// Host → cart, already ggo-wire framed. Drained by the emulator thread.
    outbound: Mutex<VecDeque<Vec<u8>>>,
    /// Cart → host CHANNEL_APP payloads, decoded on the emulator thread.
    inbound_tx: async_channel::Sender<Vec<u8>>,
    inbound_rx: async_channel::Receiver<Vec<u8>>,
    /// Latest presented frame: (cart frame number, BGRA image). Written by the
    /// emu panel on the UI thread.
    pub frame: Mutex<Option<(u32, Arc<gpui::RenderImage>)>>,
    tick_tx: async_channel::Sender<()>,
    tick_rx: async_channel::Receiver<()>,
    state: Mutex<ViewerState>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ViewerState { Building, Running, Stopped(String) }

impl LinkEndpoint {
    pub fn new() -> Arc<Self>;
    pub fn send_wire(&self, bytes: Vec<u8>);                 // host side
    pub fn take_outbound(&self) -> Vec<Vec<u8>>;             // emulator thread
    pub fn push_inbound(&self, payload: Vec<u8>);            // emulator thread (drops when full)
    pub fn try_recv_inbound(&self) -> Vec<Vec<u8>>;          // host side
    pub fn tick(&self);                                      // emu panel per frame (never blocks)
    pub fn ticks(&self) -> async_channel::Receiver<()>;      // host side
    pub fn set_state(&self, s: ViewerState);
    pub fn state(&self) -> ViewerState;
}

pub type ViewerBooter = fn(&mut Workspace, &str, Arc<LinkEndpoint>, &mut Window, &mut Context<Workspace>) -> bool;
pub fn register_viewer_booter(cx: &mut App, booter: ViewerBooter);
/// Ask the registered booters to boot the viewer cart for the emerald project
/// containing `world_rel`; returns the endpoint the caller keeps polling, or
/// `None` when no booter claimed it.
pub fn boot_viewer(workspace: &mut Workspace, world_rel: &str, window: &mut Window, cx: &mut Context<Workspace>) -> Option<Arc<LinkEndpoint>>;
```

- [ ] **Step 1: Write the failing tests** (in `ggo_common.rs`'s `tests` module)

```rust
    #[test]
    fn link_endpoint_queues_outbound_in_order_and_drains_once() {
        let ep = LinkEndpoint::new();
        ep.send_wire(vec![1, 2]);
        ep.send_wire(vec![3]);
        assert_eq!(ep.take_outbound(), vec![vec![1, 2], vec![3]]);
        assert!(ep.take_outbound().is_empty());
    }

    #[test]
    fn link_endpoint_inbound_is_bounded_and_drops_newest_when_full() {
        let ep = LinkEndpoint::new();
        for i in 0..(LINK_INBOUND_CAPACITY + 5) {
            ep.push_inbound(vec![i as u8]);
        }
        let got = ep.try_recv_inbound();
        assert_eq!(got.len(), LINK_INBOUND_CAPACITY);
        assert_eq!(got[0], vec![0u8]);
        assert!(ep.try_recv_inbound().is_empty());
    }

    #[test]
    fn link_endpoint_tick_never_blocks_and_coalesces() {
        let ep = LinkEndpoint::new();
        for _ in 0..10 {
            ep.tick();
        }
        let ticks = ep.ticks();
        assert!(ticks.try_recv().is_ok());
        assert!(ticks.try_recv().is_err(), "ticks coalesce into one pending wake");
    }

    #[test]
    fn link_endpoint_state_starts_building() {
        let ep = LinkEndpoint::new();
        assert_eq!(ep.state(), ViewerState::Building);
        ep.set_state(ViewerState::Stopped("cart exited".into()));
        assert_eq!(ep.state(), ViewerState::Stopped("cart exited".into()));
    }

    #[gpui::test]
    fn boot_viewer_returns_none_without_a_booter(cx: &mut gpui::TestAppContext) {
        // No registry global at all: the world panel must fall back to Design.
        let registered = cx.update(|cx| cx.try_global::<ViewerBooters>().map(|b| b.0.len()).unwrap_or(0));
        assert_eq!(registered, 0);
    }
```

- [ ] **Step 2: Run, verify fail**

Run: `cd ~/projects/zed && cargo test -p ggo_common --lib link_endpoint`
Expected: compile errors.

- [ ] **Step 3: Implement**

`Cargo.toml`: `async-channel.workspace = true # GGO -- LinkEndpoint's inbound/tick channels`.

```rust
// ------------------------------------------------- viewer link (world <-> emulator)

/// Payloads the cart may send between two host polls before the oldest
/// backlog is worth more than the newest: a frame's worth of entity diffs
/// plus statuses is a handful of datagrams; 256 is generous.
pub const LINK_INBOUND_CAPACITY: usize = 256;

pub struct LinkEndpoint {
    outbound: Mutex<VecDeque<Vec<u8>>>,
    inbound_tx: async_channel::Sender<Vec<u8>>,
    inbound_rx: async_channel::Receiver<Vec<u8>>,
    pub frame: Mutex<Option<(u32, Arc<gpui::RenderImage>)>>,
    tick_tx: async_channel::Sender<()>,
    tick_rx: async_channel::Receiver<()>,
    state: Mutex<ViewerState>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ViewerState {
    Building,
    Running,
    Stopped(String),
}

impl LinkEndpoint {
    pub fn new() -> Arc<Self> {
        let (inbound_tx, inbound_rx) = async_channel::bounded(LINK_INBOUND_CAPACITY);
        let (tick_tx, tick_rx) = async_channel::bounded(1);
        Arc::new(Self {
            outbound: Mutex::new(VecDeque::new()),
            inbound_tx,
            inbound_rx,
            frame: Mutex::new(None),
            tick_tx,
            tick_rx,
            state: Mutex::new(ViewerState::Building),
        })
    }
    pub fn send_wire(&self, bytes: Vec<u8>) {
        self.outbound.lock().unwrap().push_back(bytes);
    }
    pub fn take_outbound(&self) -> Vec<Vec<u8>> {
        self.outbound.lock().unwrap().drain(..).collect()
    }
    /// Full = the host has not polled for a while; the cart's newest
    /// datagram is dropped, exactly what the wire would do. The cart
    /// republishes anything that still differs on its next frame.
    pub fn push_inbound(&self, payload: Vec<u8>) {
        if self.inbound_tx.try_send(payload).is_err() {
            log::debug!("viewer link inbound full; dropping a datagram");
        }
    }
    pub fn try_recv_inbound(&self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.inbound_rx.try_recv().ok()).collect()
    }
    pub fn tick(&self) {
        // bounded(1): a pending wake already covers this frame.
        let _ = self.tick_tx.try_send(()); // ponytail: coalescing by design, a full channel is the success case
    }
    pub fn ticks(&self) -> async_channel::Receiver<()> {
        self.tick_rx.clone()
    }
    pub fn set_state(&self, s: ViewerState) {
        *self.state.lock().unwrap() = s;
        self.tick();
    }
    pub fn state(&self) -> ViewerState {
        self.state.lock().unwrap().clone()
    }
}

pub type ViewerBooter =
    fn(&mut Workspace, &str, Arc<LinkEndpoint>, &mut Window, &mut Context<Workspace>) -> bool;

#[derive(Default)]
struct ViewerBooters(Vec<ViewerBooter>);
impl gpui::Global for ViewerBooters {}

pub fn register_viewer_booter(cx: &mut App, booter: ViewerBooter) {
    cx.default_global::<ViewerBooters>().0.push(booter);
}

pub fn boot_viewer(
    workspace: &mut Workspace,
    world_rel: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Arc<LinkEndpoint>> {
    let booters = match cx.try_global::<ViewerBooters>() {
        Some(b) if !b.0.is_empty() => b.0.clone(),
        _ => return None,
    };
    let endpoint = LinkEndpoint::new();
    booters
        .iter()
        .any(|boot| boot(workspace, world_rel, endpoint.clone(), window, cx))
        .then_some(endpoint)
}
```

The `tick` comment is the one place a discarded result is intentional; keep the `// ponytail:` note so the `let _` rule's reviewer sees why.

- [ ] **Step 4: Run, verify pass**

Run: `cd ~/projects/zed && ./script/clippy -p ggo_common && cargo test -p ggo_common --lib`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
cd ~/projects/zed && git add crates/ggo/common
git commit -m "ggo_common: viewer LinkEndpoint and booter registry"
```

---

### Task 2: Pump the link on the emulator thread

**Files:**
- Create: `crates/ggo/emu_panel/src/link.rs`
- Modify: `crates/ggo/emu_panel/src/drive.rs` (`Session` ~line 179, `Controls` ~line 431, `start` ~line 370, the `Vsync` arm ~line 650)
- Modify: `crates/ggo/emu_panel/Cargo.toml`, root `Cargo.toml`
- Test: `link.rs` inline tests; `drive.rs` tests

**Interfaces:**
- Consumes: `Peripherals::{uart_inject, take_comm}`, `ggo_comm::{MessageReader, LinkItem}`, `ggo_common::LinkEndpoint`.
- Produces: `drive::start(cart_path, cart, audio, link: Option<Arc<LinkEndpoint>>)`; `link::pump_link(p: &mut Peripherals, endpoint: &LinkEndpoint, reader: &mut MessageReader)`.

- [ ] **Step 1: Write the failing tests** (`crates/ggo/emu_panel/src/link.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ggo_common::LinkEndpoint;
    use ggo_emu_core::peripherals::Peripherals;

    fn app_wire(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(ggo_wire::encode_payload(ggo_wire::channel::APP, payload, |b| out.push(b)));
        out
    }

    #[test]
    fn outbound_wire_bytes_reach_the_carts_comm_queue() {
        let mut p = Peripherals::new(0, 0);
        let ep = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        ep.send_wire(app_wire(b"hello"));
        pump_link(&mut p, &ep, &mut reader);
        assert_eq!(p.comm.pop_app().unwrap().payload(), b"hello");
    }

    #[test]
    fn cart_app_frames_are_decoded_into_inbound_payloads_and_other_channels_go_nowhere() {
        let mut p = Peripherals::new(0, 0);
        let ep = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        // What COMM_SEND would have produced, plus a LOG frame the console owns.
        let mut tx = app_wire(b"pong");
        ggo_wire::encode_payload(ggo_wire::channel::LOG, b"noise", |b| tx.push(b));
        p.comm_tx_test_push(&tx);
        pump_link(&mut p, &ep, &mut reader);
        assert_eq!(ep.try_recv_inbound(), vec![b"pong".to_vec()]);
    }

    #[test]
    fn a_frame_split_across_two_pumps_still_decodes() {
        let mut p = Peripherals::new(0, 0);
        let ep = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        let tx = app_wire(b"split");
        let (a, b) = tx.split_at(4);
        p.comm_tx_test_push(a);
        pump_link(&mut p, &ep, &mut reader);
        assert!(ep.try_recv_inbound().is_empty());
        p.comm_tx_test_push(b);
        pump_link(&mut p, &ep, &mut reader);
        assert_eq!(ep.try_recv_inbound(), vec![b"split".to_vec()]);
    }
}
```

`comm_tx_test_push` does not exist on `Peripherals`; Phase 0 made `comm_tx` `pub(crate)`. Add to `ggo-emu-core` (ggo repo, one-line follow-up commit on `main`): `#[doc(hidden)] pub fn comm_tx_test_push(&mut self, bytes: &[u8]) { self.comm_tx.extend_from_slice(bytes) }`. Do that first, gate it with `cargo test -p ggo-emu-core`, commit `emu-core: test hook to stage comm TX bytes`.

- [ ] **Step 2: Run, verify fail**

Run: `cd ~/projects/zed && cargo test -p ggo_emu_panel --lib link::`
Expected: compile errors (module missing).

- [ ] **Step 3: Implement**

Root `Cargo.toml`, next to the other `ggo-*` path deps:

```toml
ggo-comm = { path = "../ggo/tools/ggo-comm" } # GGO
ggo-wire = { path = "../ggo/firmware/ggo-wire" } # GGO
```

`crates/ggo/emu_panel/Cargo.toml`: `ggo-comm.workspace = true # GGO -- MessageReader over the cart's comm TX`, `ggo-wire.workspace = true # GGO -- channel consts; tests frame payloads`.

`link.rs`:

```rust
//! The viewer link's emulator side: at every frame boundary, hand the
//! host's queued wire bytes to the cart's comm RX and turn the cart's
//! comm TX into decoded `CHANNEL_APP` payloads for the host. Everything
//! else the cart puts on the wire (LOG/TELEMETRY) is not the link's
//! business; `take_log` still feeds the console separately.

use ggo_common::LinkEndpoint;
use ggo_emu_core::peripherals::Peripherals;

pub fn pump_link(p: &mut Peripherals, endpoint: &LinkEndpoint, reader: &mut ggo_comm::MessageReader) {
    for bytes in endpoint.take_outbound() {
        p.uart_inject(&bytes);
    }
    let tx = p.take_comm();
    if tx.is_empty() {
        return;
    }
    for item in reader.feed(&tx) {
        if let ggo_comm::LinkItem::Message(m) = item
            && m.channel == ggo_wire::channel::APP
        {
            endpoint.push_inbound(m.payload().to_vec());
        }
    }
}
```

`drive.rs`: add `link: Option<Arc<LinkEndpoint>>` to `Controls` and a `link` parameter to `start`; on the thread create `let mut link_reader = ggo_comm::MessageReader::default();` and in the `Vsync` arm, right after `publish_world_tap(...)`:

```rust
                if let Some(link) = &link {
                    crate::link::pump_link(&mut p, link, &mut link_reader);
                }
```

On every way out of the loop (after the `let (reason, is_error) = loop { .. }`): `if let Some(link) = &link { link.set_state(ggo_common::ViewerState::Stopped(reason.clone())); }`. `Session` gets `pub fn link(&self) -> Option<Arc<LinkEndpoint>>`. Update the three existing `drive::start(..)` call sites (`EmuPanel::run`, tests' `run_green_cart_briefly`) to pass `None`.

- [ ] **Step 4: Run, verify pass**

Run: `cd ~/projects/zed && ./script/clippy -p ggo_emu_panel && cargo test -p ggo_emu_panel --lib`
Expected: pass (the three new tests plus the existing drive suite).

- [ ] **Step 5: Commit**

```bash
cd ~/projects/zed && git add Cargo.toml Cargo.lock crates/ggo/emu_panel
git commit -m "ggo_emu_panel: pump the viewer link at each frame boundary"
```

---

### Task 3: Boot the viewer cart from the registry

**Files:**
- Modify: `crates/ggo/emu_panel/src/menu.rs` (beside `world_pack_args` users; `cart_selection` ~line 99)
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (`init`, `run` ~line 1530, `on_frame` ~line 1567, `emulate_world` ~line 1778, `prepare_world_build` ~line 1844, watch-mode rebuild)
- Test: `menu.rs` tests, `ggo_emu_panel.rs` tests

**Interfaces:**
- Consumes: `ggo_common::{register_viewer_booter, LinkEndpoint, ViewerState, ProcRequest, emerald_project_root}`, `ggo_worldlib::emerald::parse_emd_trailer`.
- Produces: `menu::editor_cart_args() -> Vec<String>` (`["editor-cart", "--ggo"]`), `menu::editor_cart_ggo_path(lines: &[String]) -> Option<PathBuf>` (the `ggo` field of the JSON trailer), `EmuPanel::boot_viewer(world_rel, endpoint, window, cx)`, `RunKind::{Cart, World(String), Viewer(String)}` on the panel.

- [ ] **Step 1: Write the failing tests**

`menu.rs`:

```rust
    #[test]
    fn editor_cart_args_ask_for_the_ggo_artifact() {
        assert_eq!(editor_cart_args(), ["editor-cart", "--ggo"]);
    }

    #[test]
    fn editor_cart_ggo_path_reads_the_json_trailer() {
        let lines = vec![
            "   Compiling demo_editor".to_string(),
            r#"{"cart":"/p/demo-editor.cart","elf":"/p/.tmp/x","ggo":"/p/demo-editor.ggo"}"#.to_string(),
        ];
        assert_eq!(editor_cart_ggo_path(&lines), Some(PathBuf::from("/p/demo-editor.ggo")));
        let no_ggo = vec![r#"{"cart":"/p/demo-editor.cart","elf":"/p/.tmp/x","ggo":null}"#.to_string()];
        assert_eq!(editor_cart_ggo_path(&no_ggo), None);
    }
```

`ggo_emu_panel.rs` (follow the existing `emulate_world` panel tests' fixture: a temp emerald project, `proc_runner` swapped for a recorder that answers the JSON trailer):

```rust
    #[gpui::test]
    async fn boot_viewer_builds_the_editor_cart_then_runs_it_with_the_link(cx: &mut TestAppContext) {
        let (panel, dir, runner_log, cx) = viewer_fixture(cx).await; // records ProcRequests; replies with a trailer naming <dir>/demo-editor.ggo and writes a green-screen .ggo there
        let endpoint = ggo_common::LinkEndpoint::new();
        panel.update_in(cx, |panel, window, cx| panel.boot_viewer("assets/worlds/main.toml", endpoint.clone(), window, cx));
        cx.run_until_parked();
        let requests = runner_log.borrow();
        assert_eq!(requests[0].args, ["editor-cart", "--ggo", "--json"]);
        assert_eq!(endpoint.state(), ggo_common::ViewerState::Running);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(panel.run_kind, RunKind::Viewer(ref w) if w == "assets/worlds/main.toml"));
            assert!(panel.session.as_ref().and_then(|s| s.link()).is_some());
        });
    }

    #[gpui::test]
    async fn a_failed_editor_cart_build_stops_the_endpoint_with_the_reason(cx: &mut TestAppContext) {
        let (panel, _dir, _log, cx) = viewer_fixture_failing_build(cx, "error: could not compile").await;
        let endpoint = ggo_common::LinkEndpoint::new();
        panel.update_in(cx, |panel, window, cx| panel.boot_viewer("assets/worlds/main.toml", endpoint.clone(), window, cx));
        cx.run_until_parked();
        match endpoint.state() {
            ggo_common::ViewerState::Stopped(reason) => assert!(reason.contains("could not compile"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[gpui::test]
    async fn viewer_frames_land_in_the_endpoint_slot_and_tick(cx: &mut TestAppContext) {
        let (panel, _dir, _log, cx) = viewer_fixture(cx).await;
        let endpoint = ggo_common::LinkEndpoint::new();
        panel.update_in(cx, |panel, window, cx| panel.boot_viewer("assets/worlds/main.toml", endpoint.clone(), window, cx));
        cx.executor().advance_clock(std::time::Duration::from_millis(100));
        cx.run_until_parked();
        assert!(endpoint.frame.lock().unwrap().is_some(), "on_frame published a frame");
        assert!(endpoint.ticks().try_recv().is_ok());
    }
```

Build `viewer_fixture` from the existing test helpers that stub `proc_runner` (see the `emulate_world` tests around the `recording_runner` pattern in this file) and `drive::tests::green_screen_cart()` for the `.ggo` bytes (a green-screen `.cart` is fine: `Cart::parse` accepts both, and a viewer run without a TOC just has no assets).

- [ ] **Step 2: Run, verify fail**

Run: `cd ~/projects/zed && cargo test -p ggo_emu_panel --lib editor_cart_ boot_viewer viewer_frames`
Expected: compile errors.

- [ ] **Step 3: Implement**

`menu.rs`:

```rust
/// `emd editor-cart --ggo`: the viewer cart with the project's assets
/// folded in, so the pane boots it exactly like any `.ggo`.
pub fn editor_cart_args() -> Vec<String> {
    vec!["editor-cart".to_string(), "--ggo".to_string()]
}

/// The `.ggo` path `emd editor-cart --ggo` printed in its JSON trailer.
pub fn editor_cart_ggo_path(lines: &[String]) -> Option<PathBuf> {
    let trailer = lines.iter().rev().find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())?;
    trailer.get("ggo")?.as_str().map(PathBuf::from)
}
```

`ggo_emu_panel.rs`:

- `enum RunKind { Cart, World(String), Viewer(String) }` field `run_kind` on the panel (`Cart` default); `emulate_world` sets `World(rel)`, `run` from a click sets `Cart`.
- `init`: `ggo_common::register_viewer_booter(cx, boot_viewer_hook);` where

```rust
fn boot_viewer_hook(
    workspace: &mut Workspace,
    world_rel: &str,
    endpoint: Arc<ggo_common::LinkEndpoint>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(panel) = workspace.panel::<EmuPanel>(cx) else {
        return false;
    };
    let world_rel = world_rel.to_string();
    // The registry call runs inside a workspace update from another panel's
    // listener; touching this panel inline would panic (see CLAUDE.md).
    cx.defer_in(window, move |_, window, cx| {
        panel.update(cx, |panel, cx| panel.boot_viewer(&world_rel, endpoint, window, cx));
    });
    true
}
```

- `boot_viewer`: mirrors `emulate_world` with `menu::editor_cart_args()` instead of `world_pack_args`, reads the `.ggo` path from `menu::editor_cart_ggo_path(&capture.lines)`, sets `self.run_kind = RunKind::Viewer(world_rel)`, stores `self.viewer_link = Some(endpoint.clone())`, and calls `self.run(window, cx)`. On any failure: `endpoint.set_state(ViewerState::Stopped(reason))` in addition to `report_failure`. `run` passes `self.viewer_link.clone()` to `drive::start` when `run_kind` is `Viewer`, else `None`; a new non-viewer run clears `viewer_link` after stopping the endpoint with `Stopped("replaced by another run")`. After a successful `drive::start` for a viewer: `endpoint.set_state(ViewerState::Running)`.
- `on_frame`: after building `self.latest_frame`, when `viewer_link` is set: `*link.frame.lock().unwrap() = Some((frame.number, image.clone())); link.tick();`.
- `finish_run`: when a viewer run ends, `set_state(Stopped(reason))` (the drive thread already did; this covers the ingest path's own reason) and keep `viewer_link` so the world panel can show why.
- Watch mode: `watched_world` already triggers `emulate_world` on saves; for `RunKind::Viewer` route the rebuild through `boot_viewer` with the same endpoint (the endpoint survives a reboot; the world panel re-sends `Hello` when it sees `Running` again). Only Rust sources and `assets/**` except `worlds/**.toml` should trigger a viewer rebuild (a world save goes over the link, not through a rebuild): extend the existing `warrants_repack` predicate with a `viewer: bool` argument that excludes `**/worlds/**/*.toml`.

- [ ] **Step 4: Run, verify pass**

Run: `cd ~/projects/zed && ./script/clippy -p ggo_emu_panel && cargo test -p ggo_emu_panel --lib`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
cd ~/projects/zed && git add crates/ggo/emu_panel
git commit -m "ggo_emu_panel: boot the emerald viewer cart on request with a link endpoint"
```

---

### Task 4: MCP visibility (small)

**Files:**
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs`, `crates/ggo/emu_remote/src/protocol.rs`, `crates/ggo/emu_mcp/src/tools.rs`
- Test: `protocol.rs` tests

- [ ] **Step 1: Add `run_kind` to the status reply**

`emu_status` already reports the running cart; add a `run_kind: "cart" | "world" | "viewer"` field and, for `viewer`, the `world` path. Write the protocol round-trip test beside the existing `Response` tests, implement, and run `cargo test -p ggo_emu_remote -p ggo_emu_mcp -p ggo_emu_panel --lib`.

- [ ] **Step 2: Commit**

```bash
cd ~/projects/zed && git add crates/ggo/emu_remote crates/ggo/emu_mcp crates/ggo/emu_panel
git commit -m "emu_mcp: report the run kind in emu_status"
```

---

### Task 5: Wrap

- [ ] **Step 1: Full gates**

```bash
cd ~/projects/zed && ./script/clippy -p ggo_common -p ggo_emu_panel -p ggo_emu_remote -p ggo_emu_mcp && cargo test -p ggo_common -p ggo_emu_panel -p ggo_emu_remote -p ggo_emu_mcp --lib && cargo check -p zed
```

- [ ] **Step 2: Review, then merge to `ggo`** per the branch workflow (review pass before every merge; push `main` too).

Phase 3 (world panel Live mode) consumes `ggo_common::{boot_viewer, LinkEndpoint, ViewerState}` from this phase and `emerald_editor_link::{LinkMailbox, LinkIo}` from Phase 1.
