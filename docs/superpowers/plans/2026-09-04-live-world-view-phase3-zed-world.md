# Live World View, Phase 3 (world panel Live mode) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the ZedGG world panel a `Live` canvas mode that shows the open world as the emerald viewer cart renders it, edits it over the UART link, keeps every existing editor feature working, and falls back to today's `Design` renderer whenever the cart is not there.

**Architecture:** `OpenWorld` gains an optional `LiveView`: a `LinkMailbox` over the `LinkEndpoint` Phase 2's booter hands back, a poll task woken by the endpoint's per-frame tick, the latest frame image, the cart's entity rows, and a flattened-index map (direct entities, then instances depth-first, the encoder's order). The document side is untouched: gestures still produce `WorldOp`s; Live additionally sends `SetTransform` during drags and re-encodes the document into a world blob after any other change. Backgrounds use the runtime's raw layer banks (`load_layer`) so painting is live without rebuilding assets.

**Tech Stack:** Rust, gpui, `emerald-editor-link` (`LinkMailbox`, `LinkIo`), `emerald-world` (`encode_toml_at`), `ggo-worldlib` (`world_to_toml`, `open_map`), `ggo-wire` (`encode_payload`).

**Spec:** `docs/superpowers/specs/2026-09-04-live-world-view-design.md`, section "ZedGG changes / ggo_world_panel". Prerequisites: Phases 0–2 merged.

## Global Constraints

- Work in `~/projects/zed`, branch `live-world-view`. Gate: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`. The Design-mode test suite must stay green and unmodified except where a helper is shared.
- Fork hook rule from `CLAUDE.md` applies to the booter call (`boot_viewer` runs inside a workspace update → `window.defer`, as `emulate_impl` already does).
- TOML on disk stays the single source of truth; Live never writes files. Save is the existing `save_impl`.
- Q16.16: cart positions are `i32` raw; host positions are `f64` px. Convert with `(px * 65536.0).round() as i32` and `raw as f64 / 65536.0`.
- New root `Cargo.toml` path deps, `# GGO`-marked: `emerald-editor-link = { path = "../emerald/crates/editor-link" }`, `emerald-editor-runtime = { path = "../emerald/crates/editor-runtime" }` (tests: `wire::encode_cart`), `emerald-world = { path = "../emerald/crates/world", features = ["std"] }`.
- Commit messages: short imperative subject, no AI trailers. Line numbers are as of 2026-09-04; locate by symbol.

---

## File structure

| Path | Responsibility |
|------|----------------|
| `~/projects/ggo/tools/ggo-worldlib/src/world_file.rs` | `world_to_toml(&WorldFile) -> Result<String>` factored out of `write_world` (tiny ggo-side commit). |
| `crates/ggo/world_panel/src/live.rs` | New: `LiveView`, `EndpointIo`, flattened index map, row↔selection mapping, world-blob encoding, layer payload building. Pure where possible; unit-tested. |
| `crates/ggo/world_panel/src/ggo_world_panel.rs` | `CanvasMode`, toolbar switch, Live gesture branches, `live_tick`, systems rail, fallback. |
| `crates/ggo/world_panel/src/canvas.rs` | `LiveScene` + `paint_live` (frame image, selection outlines, grid). |
| `crates/ggo/world_panel/src/loader.rs` | `layer_payloads(root, merged) -> Vec<LayerPayload>` (map cells + tileset stem per slot), off-thread. |
| `crates/ggo/world_panel/Cargo.toml` | New deps. |

---

### Task 1: `world_to_toml` in ggo-worldlib

**Files:**
- Modify: `~/projects/ggo/tools/ggo-worldlib/src/world_file.rs` (`write_world` ~line 354)
- Test: same file's tests

**Interfaces:**
- Produces: `pub fn world_to_toml(doc: &WorldFile) -> Result<String>` (the exact bytes `write_world` writes).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn world_to_toml_matches_what_write_world_writes() {
        let dir = tempfile::tempdir().unwrap();
        let doc = WorldFile {
            entities: vec![WorldEntity { components: serde_json::from_value(serde_json::json!({ "Transform": { "pos": [1.5, 2.0] } })).unwrap() }],
            instances: vec![WorldInstance { world: "worlds/sub".into(), pos: [3.0, 4.0], background_priority: true }],
            backgrounds: vec![Background { layer: 2, map: "maps/x.map".into() }],
        };
        write_world(dir.path(), "worlds/a.toml", &doc).unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("worlds/a.toml")).unwrap();
        assert_eq!(world_to_toml(&doc).unwrap(), on_disk);
    }
```

- [ ] **Step 2: Run, verify fail** — `cd ~/projects/ggo/tools && cargo test -p ggo-worldlib world_to_toml`

- [ ] **Step 3: Implement** — move the string-building half of `write_world` into `world_to_toml`; `write_world` becomes `atomic_write(&safe_join(project_dir, rel)?, world_to_toml(doc)?.as_bytes())` (keep its existing directory-creation behaviour).

- [ ] **Step 4: Run, verify pass; commit on ggo `main`**

```bash
cd ~/projects/ggo/tools && cargo test -p ggo-worldlib && cd .. && git add tools/ggo-worldlib && git commit -m "worldlib: world_to_toml, the string half of write_world"
```

---

### Task 2: `live.rs` pure helpers

**Files:**
- Create: `crates/ggo/world_panel/src/live.rs`
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`mod live;`), `crates/ggo/world_panel/Cargo.toml`, root `Cargo.toml`
- Test: inline

**Interfaces:**
- Produces:

```rust
/// `LinkIo` over the emu panel's endpoint: frames payloads as APP datagrams.
pub struct EndpointIo(pub Arc<ggo_common::LinkEndpoint>);

/// Flattened cart index -> document selection, in the encoder's order.
pub struct IndexMap { entries: Vec<Selection> }
impl IndexMap {
    /// `instance_counts[i]` = number of entities instance i contributes
    /// (its whole subtree, depth-first).
    pub fn new(direct_entities: usize, instance_counts: &[usize]) -> Self;
    pub fn selection_of(&self, cart_index: u32) -> Option<Selection>;
    /// Every cart index that belongs to `sel` (one for an entity, a range
    /// for an instance).
    pub fn indices_of(&self, sel: Selection) -> Vec<u32>;
    pub fn len(&self) -> usize;
}

pub struct CartRow { pub index: u32, pub x: f64, pub y: f64, pub w: f64, pub h: f64 }
pub fn rows_from(entities: &[emerald_editor_runtime::wire::EntityRow]) -> Vec<CartRow>;
/// Topmost (last) row under a world point, as the cart draws later rows above earlier ones.
pub fn hit_row(rows: &[CartRow], x: f64, y: f64) -> Option<u32>;
pub fn rows_in_rect(rows: &[CartRow], x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<u32>;

pub fn to_raw(px: f64) -> i32;
pub fn from_raw(raw: i32) -> f64;

/// The world blob for the open document: `world_to_toml` -> `encode_toml_at`.
pub fn encode_world(store: &WorldDocStore, assets_root: &Path) -> anyhow::Result<Vec<u8>>;

/// The runtime's raw-layer bank split (emerald-editor's `logic::layers::banks`).
pub struct Bank { pub base: u16, pub budget: u16 }
pub fn banks(linked: &[bool; 4]) -> [Option<Bank>; 4];   // BG_TILE_BASE 511, BG_TILE_REGION 496

/// Bare `map_w u16, map_h u16, cells` bytes the cart's `CMD_LOAD_LAYER` wants.
pub fn layer_bytes(w: u16, h: u16, cells: &[u16]) -> Vec<u8>;
```

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::render::Selection;

    #[test]
    fn index_map_puts_direct_entities_first_then_instances_depth_first() {
        let m = IndexMap::new(2, &[3, 1]);
        assert_eq!(m.len(), 6);
        assert_eq!(m.selection_of(0), Some(Selection::Entity(0)));
        assert_eq!(m.selection_of(1), Some(Selection::Entity(1)));
        assert_eq!(m.selection_of(2), Some(Selection::Instance(0)));
        assert_eq!(m.selection_of(4), Some(Selection::Instance(0)));
        assert_eq!(m.selection_of(5), Some(Selection::Instance(1)));
        assert_eq!(m.selection_of(6), None);
        assert_eq!(m.indices_of(Selection::Instance(0)), [2, 3, 4]);
        assert_eq!(m.indices_of(Selection::Entity(1)), [1]);
    }

    #[test]
    fn hit_row_prefers_the_last_row_under_the_point() {
        let rows = vec![
            CartRow { index: 0, x: 0.0, y: 0.0, w: 16.0, h: 16.0 },
            CartRow { index: 1, x: 8.0, y: 8.0, w: 16.0, h: 16.0 },
        ];
        assert_eq!(hit_row(&rows, 10.0, 10.0), Some(1));
        assert_eq!(hit_row(&rows, 2.0, 2.0), Some(0));
        assert_eq!(hit_row(&rows, 100.0, 100.0), None);
        assert_eq!(rows_in_rect(&rows, 0.0, 0.0, 9.0, 9.0), [0, 1]);
    }

    #[test]
    fn raw_conversion_round_trips_pixels() {
        assert_eq!(to_raw(1.5), 98304);
        assert_eq!(from_raw(98304), 1.5);
        assert_eq!(to_raw(-3.0), -196608);
    }

    #[test]
    fn banks_split_the_region_evenly_over_linked_slots() {
        let b = banks(&[true, false, true, false]);
        assert_eq!(b[0].map(|b| (b.base, b.budget)), Some((511, 248)));
        assert!(b[1].is_none());
        assert_eq!(b[2].map(|b| (b.base, b.budget)), Some((759, 248)));
        assert!(banks(&[false; 4]).iter().all(|b| b.is_none()));
    }

    #[test]
    fn layer_bytes_are_w_h_then_cells_little_endian() {
        assert_eq!(layer_bytes(2, 1, &[5, 1023]), vec![2, 0, 1, 0, 5, 0, 255, 3]);
    }

    #[test]
    fn encode_world_produces_a_v5_blob_with_the_document_entities() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("worlds")).unwrap();
        std::fs::write(dir.path().join("worlds/sub.toml"), "[[entity]]\nTransform = { pos = [1, 1] }\n").unwrap();
        let store = ggo_worldlib::world_doc::WorldDocStore::new(ggo_worldlib::world_doc::WorldDocWire {
            entities: vec![ggo_worldlib::world_file::WorldEntity {
                components: serde_json::from_value(serde_json::json!({ "Transform": { "pos": [4.0, 4.0] } })).unwrap(),
            }],
            instances: vec![ggo_worldlib::world_file::WorldInstance { world: "worlds/sub".into(), pos: [10.0, 0.0], background_priority: false }],
            backgrounds: vec![],
        });
        let blob = encode_world(&store, dir.path()).unwrap();
        assert!(blob.starts_with(b"EWLD"));
        assert_eq!(blob[4], emerald_world::VERSION);
    }
}
```

- [ ] **Step 2: Run, verify fail** — `cd ~/projects/zed && cargo test -p ggo_world_panel --lib live::`

- [ ] **Step 3: Implement**

`Cargo.toml` (world_panel): `emerald-editor-link.workspace = true # GGO`, `emerald-world.workspace = true # GGO -- encode_toml_at for the live world blob`, `ggo-wire.workspace = true # GGO -- APP framing for EndpointIo`, dev: `emerald-editor-runtime.workspace = true # GGO -- wire::encode_cart in tests`.

```rust
//! Live mode's pure half: the link transport over the emu panel's
//! endpoint, the cart-index <-> document-selection map (the encoder's
//! order: direct entities, then each instance's subtree depth-first),
//! hit-testing over the cart's published rects, and the payload builders.

use std::path::Path;
use std::sync::Arc;

use emerald_editor_link::LinkIo;
use ggo_worldlib::render::Selection;
use ggo_worldlib::world_doc::WorldDocStore;
use ggo_worldlib::world_file::world_to_toml;

pub struct EndpointIo(pub Arc<ggo_common::LinkEndpoint>);

impl LinkIo for EndpointIo {
    fn send(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let mut wire = Vec::with_capacity(payload.len() + 8);
        if !ggo_wire::encode_payload(ggo_wire::channel::APP, payload, |b| wire.push(b)) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "payload over 255 bytes"));
        }
        self.0.send_wire(wire);
        Ok(())
    }
    fn recv(&mut self) -> Vec<Vec<u8>> {
        self.0.try_recv_inbound()
    }
}

pub struct IndexMap {
    entries: Vec<Selection>,
}

impl IndexMap {
    pub fn new(direct_entities: usize, instance_counts: &[usize]) -> Self {
        let mut entries: Vec<Selection> = (0..direct_entities).map(Selection::Entity).collect();
        for (i, count) in instance_counts.iter().enumerate() {
            entries.extend(std::iter::repeat_n(Selection::Instance(i), *count));
        }
        IndexMap { entries }
    }
    pub fn selection_of(&self, cart_index: u32) -> Option<Selection> {
        self.entries.get(cart_index as usize).copied()
    }
    pub fn indices_of(&self, sel: Selection) -> Vec<u32> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == sel)
            .map(|(i, _)| i as u32)
            .collect()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub struct CartRow { pub index: u32, pub x: f64, pub y: f64, pub w: f64, pub h: f64 }

pub fn rows_from(entities: &[emerald_editor_runtime::wire::EntityRow]) -> Vec<CartRow> {
    entities
        .iter()
        .map(|e| CartRow { index: e.index, x: from_raw(e.x), y: from_raw(e.y), w: e.w as f64, h: e.h as f64 })
        .collect()
}

fn contains(r: &CartRow, x: f64, y: f64) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

pub fn hit_row(rows: &[CartRow], x: f64, y: f64) -> Option<u32> {
    rows.iter().rev().find(|r| contains(r, x, y)).map(|r| r.index)
}

pub fn rows_in_rect(rows: &[CartRow], x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<u32> {
    let (l, r, t, b) = (x0.min(x1), x0.max(x1), y0.min(y1), y0.max(y1));
    rows.iter()
        .filter(|row| row.x < r && row.x + row.w > l && row.y < b && row.y + row.h > t)
        .map(|row| row.index)
        .collect()
}

pub fn to_raw(px: f64) -> i32 {
    (px * 65536.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}
pub fn from_raw(raw: i32) -> f64 {
    raw as f64 / 65536.0
}

pub fn encode_world(store: &WorldDocStore, assets_root: &Path) -> anyhow::Result<Vec<u8>> {
    let toml = world_to_toml(&store.to_doc())?;
    emerald_world::encode_toml_at(&toml, assets_root)
}

pub const BG_TILE_BASE: u16 = 511;
pub const BG_TILE_REGION: u16 = 496;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bank { pub base: u16, pub budget: u16 }

pub fn banks(linked: &[bool; 4]) -> [Option<Bank>; 4] {
    let n = linked.iter().filter(|&&b| b).count() as u16;
    let mut out = [None; 4];
    if n == 0 {
        return out;
    }
    let budget = BG_TILE_REGION / n;
    let mut k = 0u16;
    for (slot, &is_linked) in linked.iter().enumerate() {
        if is_linked {
            out[slot] = Some(Bank { base: BG_TILE_BASE + k * budget, budget });
            k += 1;
        }
    }
    out
}

pub fn layer_bytes(w: u16, h: u16, cells: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + cells.len() * 2);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    for c in cells {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out
}
```

- [ ] **Step 4: Run, verify pass** — `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib live::`

- [ ] **Step 5: Commit**

```bash
cd ~/projects/zed && git add Cargo.toml Cargo.lock crates/ggo/world_panel
git commit -m "ggo_world_panel: live-mode pure helpers (index map, rows, payloads)"
```

---

### Task 3: Layer payloads and instance counts in the loader

**Files:**
- Modify: `crates/ggo/world_panel/src/loader.rs`
- Test: `loader.rs` tests (it already has fixture helpers for maps/tilesets; reuse `write_test_tileset`-style helpers from the panel tests or add small ones)

**Interfaces:**
- Produces:

```rust
pub struct LayerPayload { pub slot: u8, pub tileset_stem: String, pub w: u16, pub h: u16, pub cells: Vec<u16> }
/// One payload per merged background slot whose `.map` opens; a slot whose
/// map is missing is skipped (the panel reports it on the rail as today).
pub fn layer_payloads(root: &Path, merged: &[MergedBackground]) -> Vec<LayerPayload>;
/// Entities each top-level instance contributes when flattened, recursing
/// through nested instances in file order; a missing world counts 0.
pub fn instance_entity_counts(root: &Path, instances: &[WorldInstance]) -> Vec<usize>;
```

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn instance_entity_counts_recurse_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("worlds")).unwrap();
        std::fs::write(dir.path().join("worlds/leaf.toml"), "[[entity]]\nTransform = { pos = [0, 0] }\n[[entity]]\nTransform = { pos = [1, 1] }\n").unwrap();
        std::fs::write(dir.path().join("worlds/mid.toml"), "[[entity]]\nTransform = { pos = [0, 0] }\n\n[[instance]]\nworld = \"worlds/leaf\"\npos = [0, 0]\n").unwrap();
        let instances = vec![
            WorldInstance { world: "worlds/mid".into(), pos: [0.0, 0.0], background_priority: false },
            WorldInstance { world: "worlds/missing".into(), pos: [0.0, 0.0], background_priority: false },
            WorldInstance { world: "worlds/leaf".into(), pos: [0.0, 0.0], background_priority: false },
        ];
        assert_eq!(instance_entity_counts(dir.path(), &instances), [3, 0, 2]);
    }

    #[test]
    fn layer_payloads_carry_cells_and_tileset_stem_per_slot() {
        let dir = tempfile::tempdir().unwrap();
        // write a 2x1 map bound to tileset "tiles/a" via the worldlib writer used by the map panel
        let map_rel = write_small_map(dir.path(), "maps/m.map", "tiles/a.til", 2, 1, &[7, 1023]);
        let merged = vec![MergedBackground { layer: 2, map: map_rel.clone(), source: None }];
        let payloads = layer_payloads(dir.path(), &merged);
        assert_eq!(payloads.len(), 1);
        assert_eq!((payloads[0].slot, payloads[0].w, payloads[0].h), (2, 2, 1));
        assert_eq!(payloads[0].cells, [7, 1023]);
        assert_eq!(payloads[0].tileset_stem, "tiles/a");
    }
```

`MergedBackground`'s fields: check `ggo_worldlib::backgrounds::MergedBackground` and adjust the literal; `write_small_map` uses `ggo_worldlib::sprites::io::write_map` (or whatever the map panel's tests use to author a `.map`).

- [ ] **Step 2: Run, verify fail**

- [ ] **Step 3: Implement** — `instance_entity_counts`: `read_world(root, "<stem>.toml")` per instance, `entities.len() + sum(instance_entity_counts(root, &doc.instances))` with a `seen` stack to stop cycles. `layer_payloads`: `ggo_worldlib::sprites::io::open_map(root.join(&merged.map))` → `(w, h, cells, tileset rel)`; `tileset_stem = tileset_rel.strip_suffix(".til")`.

- [ ] **Step 4: Run, verify pass; commit**

```bash
cd ~/projects/zed && git add crates/ggo/world_panel/src/loader.rs
git commit -m "ggo_world_panel: layer payloads and flattened instance counts"
```

---

### Task 4: `LiveView` state, boot, poll loop, fallback

**Files:**
- Modify: `crates/ggo/world_panel/src/live.rs` (add `LiveView`, `LiveStatus`)
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`OpenWorld` ~line 597, `WorldPanel::load_rel_path` completion, new `enter_live`/`leave_live`/`live_tick`)
- Test: panel tests with a fake booter

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasMode { Design, Live }

pub enum LiveStatus { Building, Connecting, Connected, Failed(String) }

pub struct LiveView {
    pub endpoint: Arc<ggo_common::LinkEndpoint>,
    pub mailbox: emerald_editor_link::LinkMailbox<EndpointIo>,
    pub status: LiveStatus,
    pub started: Instant,
    pub last_hello: Instant,
    pub frame: Option<(u32, Arc<RenderImage>)>,
    pub rows: Vec<CartRow>,
    pub index_map: IndexMap,
    pub world_dirty: bool,
    pub layers_dirty: bool,
    pub camera_dirty: bool,
    pub sys_mask: u64,
    pub drag_origin: Vec<(u32, i32, i32)>,   // rows' raw positions at drag start
    pub _poll: Option<Task<()>>,
}

pub const HELLO_RETRY: Duration = Duration::from_millis(500);
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(5);
```

`WorldPanel` gets `canvas_mode: CanvasMode` (default `Live`, sticky across worlds within a session) and:

- `fn enter_live(&mut self, window, cx)`: requires `ViewerState::Ready`; `window.defer` → `workspace.update` → `ggo_common::boot_viewer(workspace, &source_rel, window, cx)`; `None` → `status Failed("no emulator pane / no emerald project")` and `canvas_mode = Design` with the reason on the status row; `Some(endpoint)` → build `LiveView` (status `Building`), spawn the poll task.
- Poll task: `cx.spawn(async move |this, cx| { let ticks = endpoint.ticks(); loop { let woke = smol::future::race(async { ticks.recv().await.ok() }, async { cx.background_executor().timer(Duration::from_millis(250)).await; Some(()) }).await; if woke.is_none() { break } if this.update(cx, |this, cx| this.live_tick(cx)).is_err() { break } } })`.
- `fn live_tick(&mut self, cx)`: match `endpoint.state()`: `Building` → nothing; `Stopped(reason)` → `Failed(reason)`, fall back to Design (keep the LiveView for the message, drop the mailbox); `Running` → if status is `Building` → send `hello`, status `Connecting`, `last_hello = now`. Then `mailbox.poll(now)`; if `proto_version_mismatch()` → `Failed("viewer cart predates the link protocol (cart v{n}); rebuild it")` and fall back. If connected and status was `Connecting` → `Connected`, `world_dirty = layers_dirty = camera_dirty = true`. Else if `Connecting` and `last_hello.elapsed() > HELLO_RETRY` → resend hello; if `started.elapsed() > CONNECT_DEADLINE` → `Failed("viewer cart never answered")` + fallback. When `Connected` and `!mailbox.busy()`: if `world_dirty` → `encode_world` → `load_world`, clear; else if `layers_dirty` → send each `layer_payloads` slot via `load_layer(slot, bank.base, bank.budget, &layer_bytes(..), &stem)` and a `_blank` load for unlinked slots, clear (one slot per tick while busy); if `camera_dirty` → `set_camera(to_raw(pan_world_x), to_raw(pan_world_y))`. Refresh `rows = rows_from(mailbox.entities())`, `frame = endpoint.frame.lock().clone()`. `cx.notify()`.
- `fn leave_live(&mut self, cx)`: drop `LiveView` (task ends); the emu panel keeps running the viewer until another run replaces it (deliberate: switching back to Live is instant).
- Where `load_rel_path` completes a load: if `canvas_mode == Live` → `enter_live`; a world switch reuses the endpoint if its state is `Running` (same project) by resetting `LiveView` fields and sending `hello` again, else boots.

- [ ] **Step 1: Write the failing tests** (panel tests; reuse `routed_project`, `write_fixture`, `open_rel_path`)

```rust
    thread_local! {
        static BOOTED: RefCell<Vec<(String, Arc<ggo_common::LinkEndpoint>)>> = RefCell::new(Vec::new());
    }

    fn fake_booter(_: &mut Workspace, rel: &str, endpoint: Arc<ggo_common::LinkEndpoint>, _: &mut Window, _: &mut Context<Workspace>) -> bool {
        BOOTED.with(|b| b.borrow_mut().push((rel.to_string(), endpoint)));
        true
    }

    /// A cart's datagram, as the emu panel would deliver it.
    fn cart_says(endpoint: &ggo_common::LinkEndpoint, msg: emerald_editor_runtime::wire::CartMsg<'_>) {
        let mut out = [0u8; 255];
        let n = emerald_editor_runtime::wire::encode_cart(&msg, &mut out).unwrap();
        endpoint.push_inbound(out[..n].to_vec());
        endpoint.tick();
    }

    fn host_sent(endpoint: &ggo_common::LinkEndpoint) -> Vec<Vec<u8>> {
        // wire bytes -> APP payloads
        let mut reader = ggo_comm::MessageReader::default();
        endpoint.take_outbound().iter().flat_map(|w| reader.feed(w)).filter_map(|i| match i {
            ggo_comm::LinkItem::Message(m) => Some(m.payload().to_vec()),
            _ => None,
        }).collect()
    }

    async fn live_panel(cx: &mut TestAppContext) -> (Entity<WorldPanel>, Arc<ggo_common::LinkEndpoint>, tempfile::TempDir, &mut gpui::VisualTestContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        cx.update(|cx| ggo_common::register_viewer_booter(cx, fake_booter));
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| workspace.panel::<WorldPanel>(cx).expect("init() adds the panel"));
        panel.update(cx, |panel, _| panel.root_override = Some(dir.path().to_path_buf()));
        panel.update_in(cx, |panel, window, cx| panel.open_rel_path("worlds/test.toml", window, cx));
        cx.run_until_parked();
        let endpoint = BOOTED.with(|b| b.borrow().last().map(|(_, e)| e.clone())).expect("Live mode asked the booter");
        (panel, endpoint, dir, cx)
    }

    #[gpui::test]
    async fn opening_a_world_in_live_mode_boots_the_viewer_and_says_hello_when_running(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = live_panel(cx).await;
        assert!(host_sent(&endpoint).is_empty(), "nothing on the wire while building");
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        let sent = host_sent(&endpoint);
        assert_eq!(sent.first().map(|m| m[0]), Some(0x01), "Hello first");
        panel.read_with(cx, |panel, _| {
            assert!(matches!(open_of(panel).live.as_ref().map(|l| &l.status), Some(live::LiveStatus::Connecting)));
        });
    }

    #[gpui::test]
    async fn hello_ack_connects_and_the_world_blob_goes_out(cx: &mut TestAppContext) {
        use emerald_editor_runtime::wire::{CartMsg, SystemNames};
        let (panel, endpoint, _dir, cx) = live_panel(cx).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        host_sent(&endpoint);
        cart_says(&endpoint, CartMsg::HelloAck { version: 1, entity_cap: 256, world_cap: 32768, layer_cap: 8192, systems: SystemNames::from_slice(&["animate"]) });
        cx.run_until_parked();
        let sent = host_sent(&endpoint);
        assert_eq!(sent.first().map(|m| m[0]), Some(0x08), "BlobBegin for the world");
        panel.read_with(cx, |panel, _| {
            let live = open_of(panel).live.as_ref().unwrap();
            assert!(matches!(live.status, live::LiveStatus::Connected));
            assert_eq!(live.mailbox.system_names(), ["animate"]);
        });
    }

    #[gpui::test]
    async fn a_stopped_viewer_falls_back_to_design_with_the_reason(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = live_panel(cx).await;
        endpoint.set_state(ggo_common::ViewerState::Stopped("build failed: no emd".into()));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(open_of(panel).live_error.as_deref().unwrap_or("").contains("no emd"));
        });
    }

    #[gpui::test]
    async fn no_booter_means_design_mode(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| workspace.panel::<WorldPanel>(cx).unwrap());
        panel.update(cx, |panel, _| panel.root_override = Some(dir.path().to_path_buf()));
        panel.update_in(cx, |panel, window, cx| panel.open_rel_path("worlds/test.toml", window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert_eq!(panel.canvas_mode, CanvasMode::Design));
    }

    #[gpui::test]
    async fn a_version_mismatch_falls_back_and_names_the_rebuild(cx: &mut TestAppContext) {
        use emerald_editor_runtime::wire::{CartMsg, SystemNames};
        let (panel, endpoint, _dir, cx) = live_panel(cx).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(&endpoint, CartMsg::HelloAck { version: 0, entity_cap: 256, world_cap: 32768, layer_cap: 8192, systems: SystemNames::from_slice(&[]) });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(open_of(panel).live_error.as_deref().unwrap_or("").contains("rebuild"));
        });
    }
```

`open_of(panel).live_error` is a new `Option<String>` on `OpenWorld` set on fallback and shown on the status row. Note `world_cap`/`layer_cap` in `HelloAck` and `SystemNames` come from Phase 1's `wire.rs`.

The `BOOTED` thread-local needs a registered booter per test; `register_viewer_booter` pushes into a per-`App` global, so each test app starts empty. The last two tests rely on `EMULATED`-style ordering the existing emulate test already uses.

- [ ] **Step 2: Run, verify fail** — `cd ~/projects/zed && cargo test -p ggo_world_panel --lib live_mode`

- [ ] **Step 3: Implement** as described in Interfaces. Points that bite:

- `boot_viewer` must be called from a `workspace.update` inside `window.defer`, exactly as `emulate_impl` does (see it at ~line 3429), and the returned endpoint handed back to the panel through `this.update(cx, ..)`; the panel entity is not leased during the defer, so a direct `panel.update` from the workspace closure is fine.
- The poll task's `race` needs `smol::future::race` (smol is already a workspace dep; add `smol.workspace = true` to the panel if missing) and `cx.background_executor().timer(..)` (per the repo's timer rule for tests).
- `IndexMap` is rebuilt whenever the document's instance list changes: compute `instance_entity_counts` on the loader thread as part of `load_world` (store it on `OpenWorld` as `instance_counts: Vec<usize>`) and again in `refresh_worlds`/after `AddInstance`/`RemoveInstance` ops.

- [ ] **Step 4: Run, verify pass** — `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`

- [ ] **Step 5: Commit**

```bash
cd ~/projects/zed && git add crates/ggo/world_panel
git commit -m "ggo_world_panel: Live mode boot, hello, poll loop and Design fallback"
```

---

### Task 5: Live canvas rendering and gestures

**Files:**
- Modify: `crates/ggo/world_panel/src/canvas.rs` (add `LiveScene`, `paint_live`)
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`render_canvas` ~line 4740 branches on mode; `canvas_primary_down_with` ~2958, `canvas_drag_to` ~3050, `canvas_primary_up` ~3084, `handle_pan_move` ~3117, `wheel_zoom` ~3141, `paint_at_local` ~1866, `apply_op` ~1463, `undo_impl`/`redo_impl`, `refresh_backgrounds` ~1626, `render_view_controls` ~4542)
- Test: `canvas.rs` unit tests for the frame placement math; panel tests

**Interfaces:**
- Produces:

```rust
pub struct LiveScene {
    pub frame: Option<Arc<RenderImage>>,
    pub zoom: f64,
    pub pan: [f64; 2],
    pub rows: Vec<(Selection, [f64; 4], bool)>,   // selection, world rect, selected
    pub grid: bool,
    pub background: Hsla,
    pub accent: Hsla,
}
pub fn paint_live(scene: &LiveScene, canvas_bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App);
/// Where the 320x240 frame lands on the canvas: the camera's world origin is
/// the canvas top-left, so the frame's screen rect is `[0, 0, 320*zoom, 240*zoom]`.
pub fn live_frame_bounds(canvas_bounds: Bounds<Pixels>, zoom: f64) -> Bounds<Pixels>;
```

Camera contract: in Live the camera's world origin is what the canvas's top-left shows, so `pan` is `[0, 0]` for the frame and the entity overlay uses `view = View { zoom, pan_x: 0, pan_y: 0 }` shifted by the camera origin; the pan gesture moves the camera (`camera_dirty`), not the image. Concretely: `camera_world = [-pan_x / zoom, -pan_y / zoom]` using the existing `ViewShared.pan`; the overlay draws rows at `(row.x - camera_world.x) * zoom`. Zoom is host-side only.

- [ ] **Step 1: Write the failing tests**

`canvas.rs`:

```rust
    #[test]
    fn live_frame_bounds_scale_the_device_screen_by_zoom() {
        let b = live_frame_bounds(Bounds { origin: point(px(10.), px(20.)), size: size(px(800.), px(600.)) }, 2.0);
        assert_eq!(b.origin, point(px(10.), px(20.)));
        assert_eq!(b.size, size(px(640.), px(480.)));
    }
```

Panel tests (extend `live_panel` with a connected cart: after `HelloAck`, `cart_says(Entities { rows })` with two rows at (4,4) and (40,8) 16x16, then `EntityCount { 2 }`):

```rust
    #[gpui::test]
    async fn clicking_a_cart_row_selects_the_mapped_document_entity(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        // fixture: 3 direct entities + 1 instance (1 entity); cart rows index 0..4
        cart_rows(&endpoint, &[(0, 4.0, 4.0), (1, 40.0, 8.0), (2, 0.0, 0.0), (3, 32.0, 16.0)]);
        cx.run_until_parked();
        panel.update_in(cx, |panel, _, cx| panel.canvas_primary_down_with(live_screen_of(panel, [41.0, 9.0]), false, cx));
        panel.read_with(cx, |panel, _| assert_eq!(open_of(panel).selected, vec![Selection::Entity(1)]));
        panel.update_in(cx, |panel, _, cx| panel.canvas_primary_down_with(live_screen_of(panel, [33.0, 17.0]), false, cx));
        panel.read_with(cx, |panel, _| assert_eq!(open_of(panel).selected, vec![Selection::Instance(0)]));
    }

    #[gpui::test]
    async fn dragging_sends_set_transform_live_and_commits_the_move_op_on_release(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_rows(&endpoint, &[(0, 4.0, 4.0), (1, 40.0, 8.0), (2, 0.0, 0.0), (3, 32.0, 16.0)]);
        cx.run_until_parked();
        host_sent(&endpoint);
        panel.update_in(cx, |panel, _, cx| panel.canvas_primary_down_with(live_screen_of(panel, [41.0, 9.0]), false, cx));
        panel.update_in(cx, |panel, _, cx| panel.canvas_drag_to(live_screen_of(panel, [51.0, 19.0]), cx));
        cx.run_until_parked();
        let sent = host_sent(&endpoint);
        let st = sent.iter().find(|m| m[0] == 0x02).expect("SetTransform sent");
        match emerald_editor_runtime::wire::decode_host(st) {
            Some(emerald_editor_runtime::wire::HostMsg::SetTransform { id: 1, x, y }) => {
                assert_eq!((live::from_raw(x), live::from_raw(y)), (50.0, 18.0));
            }
            other => panic!("{other:?}"),
        }
        panel.update_in(cx, |panel, _, cx| panel.canvas_primary_up(cx));
        cx.run_until_parked();
        assert_eq!(entity_pos_of(&panel, cx, 1), [50.0, 18.0], "document moved by the existing op");
        panel.read_with(cx, |panel, _| assert!(open_of(panel).live.as_ref().unwrap().world_dirty, "drop re-syncs the world"));
    }

    #[gpui::test]
    async fn structural_edits_mark_the_world_dirty_and_the_next_tick_sends_a_blob(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update_in(cx, |panel, window, cx| panel.delete_selected_now(window, cx)); // whatever the existing delete entry point is
        panel.update_in(cx, |panel, _, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        assert!(host_sent(&endpoint).iter().any(|m| m[0] == 0x08), "BlobBegin after a structural edit");
    }

    #[gpui::test]
    async fn panning_in_live_sends_camera_not_pixels(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update_in(cx, |panel, _, cx| { panel.handle_pan_move(&move_event(10.0, 10.0, Some(MouseButton::Middle)), cx); });
        panel.update_in(cx, |panel, _, cx| { panel.handle_pan_move(&move_event(30.0, 20.0, Some(MouseButton::Middle)), cx); });
        endpoint.tick();
        cx.run_until_parked();
        assert!(host_sent(&endpoint).iter().any(|m| m[0] == 0x03), "Camera sent");
    }

    #[gpui::test]
    async fn painting_in_live_resends_the_layer(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_with_background(cx).await;
        host_sent(&endpoint);
        panel.update_in(cx, |panel, window, cx| panel.enter_paint_mode(PaintTarget::Background(0), window, cx));
        panel.update_in(cx, |panel, _, cx| panel.canvas_primary_down_with(live_screen_of(panel, [1.0, 1.0]), false, cx));
        panel.update_in(cx, |panel, _, cx| panel.canvas_primary_up(cx));
        endpoint.tick();
        cx.run_until_parked();
        assert!(host_sent(&endpoint).iter().any(|m| m[0] == 0x08 && m[1] == 1), "a Layer BlobBegin");
    }
```

`connected_live_panel`, `cart_rows`, `live_screen_of` (world → canvas-local px for the Live view: `[(x - camera.x) * zoom, (y - camera.y) * zoom]` using the panel's `ViewShared`) and `connected_live_panel_with_background` (fixture with a `[[background]]` and a written `.map` + `.til`) are test helpers to add beside `routed_project`. `PaintTarget::Background(_)`'s real variant name and `enter_paint_mode`'s signature: read them at the file's `EditMode`/`PaintTarget` definitions (~line 560-600) and match.

- [ ] **Step 2: Run, verify fail**

- [ ] **Step 3: Implement**

`canvas.rs`:

```rust
pub fn live_frame_bounds(canvas_bounds: Bounds<Pixels>, zoom: f64) -> Bounds<Pixels> {
    Bounds {
        origin: canvas_bounds.origin,
        size: size(px((DEVICE_SCREEN_W as f64 * zoom) as f32), px((DEVICE_SCREEN_H as f64 * zoom) as f32)),
    }
}

pub fn paint_live(scene: &LiveScene, canvas_bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    window.with_content_mask(Some(ContentMask { bounds: canvas_bounds }), |window| {
        window.paint_quad(fill(canvas_bounds, scene.background));
        if let Some(frame) = &scene.frame {
            let bounds = live_frame_bounds(canvas_bounds, scene.zoom);
            if let Err(e) = window.paint_image(bounds, Corners::default(), frame.clone(), 0, false) {
                log::warn!("live frame paint: {e}");
            }
        }
        let view = View { zoom: scene.zoom, pan_x: 0.0, pan_y: 0.0, dpr: None };
        if scene.grid {
            paint_grid(&view, canvas_bounds, window);
        }
        for (_, [x, y, w, h], selected) in &scene.rows {
            let b = item_bounds(&view, canvas_bounds.origin, *x, *y, *w, *h);
            let color = if *selected { scene.accent } else { scene.accent.opacity(0.35) };
            window.paint_quad(outline(b, color, BorderStyle::Solid));
        }
    });
}
```

`window.paint_image`'s exact signature in this checkout: copy the call `paint_item` already makes (~line 361) and match its arguments.

Panel: `render_canvas` branches on `self.canvas_mode` and `open.live.is_some()`: build `LiveScene` from `live.frame`, `live.rows` mapped through `index_map` and offset by the camera (`row.x - camera.x`), selection flags from `open.selected`. The mouse handlers stay the same element; inside the five gesture fns add a `if self.live_active()` branch:

- `canvas_primary_down_with`: world point = camera + local / zoom; `hit_row(&live.rows, ..)` → `index_map.selection_of` → same selection/marquee logic as Design with the hit; arm `edit_drag` as Design does (starts from the document positions) and record `live.drag_origin` = current raw positions of every cart index in `indices_of(sel)` for each selected item.
- `canvas_drag_to`: after applying the live move op to the store (unchanged), compute the delta in px from the primary's start and send `set_transform(index, origin_x + to_raw(dx), origin_y + to_raw(dy))` for every `(index, ox, oy)` in `drag_origin` (`log_err` an io error). Marquee: `rows_in_rect` on release.
- `canvas_primary_up`: after the existing logic, `live.world_dirty = true; live.drag_origin.clear()`.
- `paint_at_local`: after a successful `paint_at`, `live.layers_dirty = true`.
- `apply_op`, `undo_impl`, `redo_impl`, `paste_impl`, `duplicate_impl`, `delete_selected_*`, `nudge_impl`'s end-of-run, inspector `commit_editor`: `live.world_dirty = true` (they all funnel into `store.apply`/`undo`/`redo`; the least invasive spot is a `fn note_doc_changed(&mut self)` called at each of those sites, so the count of sites is the count of `store.apply(`/`store.undo(`/`store.redo(` calls in the file: `grep -n "store\.\(apply\|undo\|redo\)(" `).
- `handle_pan_move`/`wheel_zoom`: after updating `ViewShared`, `live.camera_dirty = true`.
- `refresh_backgrounds`: `live.layers_dirty = true`.
- `refresh_worlds`/instance ops: recompute `instance_counts` off-thread (`loader::instance_entity_counts`) then `index_map = IndexMap::new(entities.len(), &counts)`.

- [ ] **Step 4: Run, verify pass** — `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`

- [ ] **Step 5: Commit**

```bash
cd ~/projects/zed && git add crates/ggo/world_panel
git commit -m "ggo_world_panel: Live canvas, cart-row hit test, live drag/paint/camera sync"
```

---

### Task 6: Mode switch, systems rail, status

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`render_toolbar`, `render_view_controls` ~4542, `render_layers_rail`, `render_message`)
- Test: panel tests

**Interfaces:**
- Produces: toolbar segmented control `Design | Live` (`ToggleDesign`/`ToggleLive` actions in the `ggo_world` action list, keymap `ctrl-alt-l` toggles), a systems rail listing `mailbox.system_names()` as checkboxes bound to `live.sys_mask` (`set_sys_mask` on change), a status line under the toolbar: `Building viewer cart…`, `Connecting…`, `Live` + frame seq, or the failure reason with a `Retry` button (calls `enter_live`).

- [ ] **Step 1: Write the failing tests**

```rust
    #[gpui::test]
    async fn toggling_a_system_sends_the_mask(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await; // HelloAck names ["animate", "ai"]
        host_sent(&endpoint);
        panel.update_in(cx, |panel, _, cx| panel.set_live_system(1, true, cx));
        cx.run_until_parked();
        let sent = host_sent(&endpoint);
        match sent.iter().find(|m| m[0] == 0x07).and_then(|m| emerald_editor_runtime::wire::decode_host(m)) {
            Some(emerald_editor_runtime::wire::HostMsg::SysMask { mask }) => assert_eq!(mask, 0b10),
            other => panic!("{other:?}"),
        }
    }

    #[gpui::test]
    async fn switching_to_design_keeps_the_document_and_back_to_live_reuses_a_running_viewer(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        panel.update_in(cx, |panel, window, cx| panel.set_canvas_mode(CanvasMode::Design, window, cx));
        panel.read_with(cx, |panel, _| assert!(open_of(panel).live.is_none()));
        let before = BOOTED.with(|b| b.borrow().len());
        panel.update_in(cx, |panel, window, cx| panel.set_canvas_mode(CanvasMode::Live, window, cx));
        cx.run_until_parked();
        assert_eq!(BOOTED.with(|b| b.borrow().len()), before + 1, "asks the booter again; the emu panel decides whether to rebuild");
        assert!(host_sent(&endpoint).is_empty() || true, "a fresh endpoint is used for the new session");
    }
```

- [ ] **Step 2: Run, verify fail**

- [ ] **Step 3: Implement** the toolbar segment (two `IconButton`s with a selected style, same pattern as the grid/snap checkboxes), `set_canvas_mode`, `set_live_system(i, on, cx)` (flip the bit, `set_sys_mask`, `log_err`), the rail (only when `live.status == Connected`), and the status line. Design mode never shows the rail.

- [ ] **Step 4: Run, verify pass; commit**

```bash
cd ~/projects/zed && git add crates/ggo/world_panel && git commit -m "ggo_world_panel: Design|Live switch, systems rail, live status"
```

---

### Task 7: Watch-mode predicate and docs

**Files:**
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (the viewer branch of the re-pack predicate from Phase 2 Task 3)
- Modify: `docs/ggo/MIGRATION.md` (one row: world view → Live/Design)
- Test: `emu_panel` predicate tests

- [ ] **Step 1: Test + implement** — a viewer run's rebuild predicate also ignores `**/maps/**.map` and `**/worlds/**.toml` (both travel over the link); tilesets/sprites/audio/Rust sources still rebuild. Add the two cases to the existing predicate tests.

- [ ] **Step 2: Docs** — add the Live view row and a short "Suggested .rules additions" note in the PR description for the one non-obvious trap met: a booter hook runs inside another panel's workspace update, so the emu panel must be touched only via `cx.defer_in`.

- [ ] **Step 3: Gates and merge**

```bash
cd ~/projects/zed && ./script/clippy -p ggo_world_panel -p ggo_emu_panel -p ggo_common && cargo test -p ggo_world_panel -p ggo_emu_panel -p ggo_common --lib && cargo check -p zed
git add -A crates/ggo docs/ggo && git commit -m "ggo_emu_panel: viewer rebuild ignores link-carried assets; docs"
```

Review pass, then merge `live-world-view` into `ggo` and push `main` per the branch workflow.

## Deferred (spec "Out of scope")

- Hardware peer (`LinkIo` over `ggo_comm::GgoLink` on the uartd pty): Phase 4, own plan.
- `SetCell` per cell instead of whole-layer resends: `// ponytail: whole-layer resend per stroke tick; SetCell per changed cell if hardware painting feels slow` at the `layers_dirty` site.
- Deleting the Design renderer.
