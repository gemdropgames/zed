# Cart-Owned Editing — Phase 3 (emerald: cart as source of truth for persistence) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Any component round-trips out of a live cart (binary field writer on the derive), the host can set/spawn/despawn entities, the cart reports component changes it made, and the host can pull a full world snapshot and a layer's cells to save.

**Architecture:** `SceneComponent::write_fields` (derive-emitted) + `FieldWriter` produce the world blob's own bag layout; `SceneRegistry` gains `WriteFn`; new host→cart commands `SetComponent`, `Spawn` (entity blob), `Despawn`, `Snapshot`, `ReadLayer`; a `Dirty` resource + `mark_dirty` drive `ComponentChanged` reports (diffed against a bounded per-slot cache); cart→host blob transfers mirror the host→cart chunk protocol (host acks, ≤ 4 in flight); layers keep a source-cell shadow.

**Tech Stack:** Rust `no_std` (`emerald-world`, `emerald-world-derive`, `emerald-editor-runtime`, `emerald-core`), std (`emerald-editor-link`).

**Spec:** `docs/superpowers/specs/2026-09-06-cart-owned-editing-design.md` — "Persistence (B)", "Testing".

## Global Constraints

- Branch `cart-owned-editing` (emerald), on top of phase 1. Commit per task, no AI trailers. Gate as in phase 1 (fmt, clippy on touched crates minus pre-existing lints, `cargo test --workspace`).
- `LINK_PROTO_VERSION = 4`. Host→cart kinds: `SetComponent 0x10 { index u32, bag… }` (bag ≤ 240 B inline; larger via blob kind `Entity` with a one-bag body), `Despawn 0x11 { index u32 }`, `Snapshot 0x12`, `ReadLayer 0x13 { layer u8 }`, `BlobAck 0x14 { seq u16 }` (host acks a cart chunk). `BlobBegin.kind` gains `2 Entity` (host→cart: spawn payload = `comp_count u16` + bags). Cart→host: `ComponentChanged 0x8E { index u32, bag… }` (inline ≤ 240 B else blob kind `Component`), `EntityAdded 0x8F { index u32 }` (the bags follow via `Snapshot`-style blob kind `Entity` for that index, or the host re-reads), `CartBlobBegin 0x90 { kind u8 (0 Snapshot, 1 Layer, 2 Entity), len u32, index u32 /* entity or layer */ }`, `CartBlobChunk 0x91 { seq u16, off u32, bytes }`, `CartBlobEnd 0x92 { seq u16 }`.
- Bag layout is exactly the world blob's: `name_hash u64 | field_count u16 | section_len u32 | (field_hash u64 | tagged value)…`, tags Int 1, Bool 2, Str 3, List 4, Fixed 5, Struct 6 (`crates/world/src/encode.rs`, `lib.rs:117-125`).
- Snapshot blob body: `entity_count u32` then per live tracked slot `index u32 | comp_count u16 | bags`. Layer blob body: `w u16 | h u16 | cells [u16; w*h]` (source tile indices).
- Dirty cache: per tracked slot up to 8 `(name_hash, [u8; 64] prefix, len)`; a bag longer than 64 B is always considered changed.
- Cart→host blob window: ≤ 4 chunks in flight, retransmit from the oldest unacked after 100 ms (cart frames × 16.7 ms) up to 20 times, then abandon and report `CartBlobEnd` with `seq = u16::MAX` (the host treats it as a failed transfer).
- Comments why only; no `unwrap()` outside tests.

---

### Task 1: `FieldWriter`, `write_fields`, public bag decoding

**Files:** `crates/world/src/lib.rs` (trait method, `FieldWriter`, `FieldReader::from_bag`, `SceneRegistry::{write_entity, apply_bag}`, `WriteFn`, builtin impls ×8 + resources), `crates/world/src/encode.rs` (share the value encoder), `crates/world-derive/src/lib.rs` (emit `write_fields`), tests in `crates/world/tests`.

**Interfaces:**
```rust
pub struct FieldWriter { .. }  // no_std, writes into a Vec<u8>
impl FieldWriter { pub fn int(&mut self, name: &str, v: i32); pub fn fixed(&mut self, name: &str, v: Fixed); pub fn bool_(..); pub fn str_(..); pub fn vec2(..); pub fn list_int(..); pub fn finish(self, component_name: &str) -> Vec<u8> /* whole bag */ }
trait SceneComponent { …; fn write_fields(&self, w: &mut FieldWriter) { /* default: nothing */ } }
impl<'a> FieldReader<'a> { pub fn from_bag(bag: &'a [u8]) -> Option<(u64 /*name hash*/, FieldReader<'a>, &'a [u8] /*rest*/)>; }
impl SceneRegistry {
    pub fn write_entity(&self, world: &World, entity: Entity, out: &mut Vec<u8>) -> u16 /* comp_count written */;
    pub fn apply_bag(&self, world: &mut World, entity: Entity, bag: &[u8]) -> bool;  // BuildFn by name hash
}
```
- [ ] Tests: every builtin round-trips (`write_fields` → `from_bag` → `from_reader` equals the original: Transform, Sprite, MetaSprite (fields only, no asset load), Text, Tilemap, Music, Sfx, Camera); a `#[derive(SceneComponent)]` struct with Int/Fixed/Bool/Str/Vec2/List fields round-trips; `write_entity` of a spawned entity with two components produces two bags the world loader would accept (feed through `load_impl`'s bag path via a tiny world blob assembled around it); `apply_bag` replaces a component.
- [ ] Implement; gate; commit `world: components write their fields back as bags`.

---

### Task 2: Wire v4 kinds + cart→host blob protocol

**Files:** `crates/editor-runtime/src/wire.rs`, `crates/editor-runtime/src/link.rs` (`LinkState`: outbound blob queue `CartTransfer { kind, index, bytes, next_seq, acked, in_flight: [.. ; 4], retries }`, `queue_blob(kind, index, bytes)`, acks from `BlobAck`), `crates/editor-link/src/lib.rs` (receiver: reassembly per kind keyed by `(kind, index)`, `take_blob() -> Option<(CartBlobKind, u32, Vec<u8>)>`, acking every chunk), tests both sides incl. an injected-drop test in `editor-link/tests/protocol.rs`.
- [ ] Tests: round trips; a 3 KiB cart blob crosses a lossy `Wire` (drop every 5th datagram) and reassembles; abandonment after 20 retries surfaces as a failed transfer on the host.
- [ ] Implement; gate; commit `editor-runtime, editor-link: link v4 with cart-to-host blobs`.

---

### Task 3: `SetComponent`, `Spawn`, `Despawn` on the cart

**Files:** `crates/editor-runtime/src/{link.rs,sync.rs,mailbox.rs}`.
- `SetComponent` (inline or via `Entity` blob with one bag) → `reg.apply_bag(world, tracked[index], bag)`; marks dirty. `Spawn` (blob kind Entity, `comp_count` + bags) → `world.spawn()`, apply each bag, append `(next_index, e)` to `tracked` where `next_index = tracked.iter().max()+1`; report `EntityAdded { index }`. `Despawn { index }` → despawn, remove from tracked, report `EntityRemoved`. Blob-carried commands are committed through the existing command slot (`cmd_kind` new consts `CMD_SET_COMPONENT 10`, `CMD_SPAWN 11`, `CMD_DESPAWN 12`), payload in `world_buf` for blobs.
- [ ] Tests (`tests/sync.rs`, `tests/link.rs`): set a Transform bag moves the entity; spawn adds a tracked slot with the next index and reports `EntityAdded`; despawn removes and reports.
- [ ] Implement; gate; commit `editor-runtime: set-component, spawn, despawn commands`.

---

### Task 4: Dirty tracking and `ComponentChanged`

**Files:** `crates/editor-runtime/src/edit.rs` (`Dirty(Vec<Entity>)`, `pub fn mark_dirty(world, entity)`), `edit_systems.rs` (built-ins mark dirty on drag/nudge), `link.rs` (per-slot bag cache, publish `ComponentChanged` inline or via `Component` blob), `sync.rs` (`editor_report` drains `Dirty`).
- [ ] Tests: a drag publishes `ComponentChanged` for Transform once per changed frame with the new bag; an unchanged dirty mark publishes nothing; a 300-byte bag goes as a blob; a user edit system that mutates a component and calls `mark_dirty` publishes it.
- [ ] Implement; gate; commit `editor-runtime: dirty tracking publishes component changes`.

---

### Task 5: Layer cell shadow, `Snapshot`, `ReadLayer`

**Files:** `crates/editor-runtime/src/layer.rs` (static `CELLS: [[u16; 1024]; 4]` source indices, written by `load_layer`/`set_cell`), `link.rs`/`sync.rs` (`Snapshot` → build the body with `reg.write_entity` per tracked slot, `queue_blob(Snapshot)`; `ReadLayer` → `queue_blob(Layer, layer)`).
- [ ] Tests: load a 4×3 map, set a cell, `ReadLayer` returns the 4×3 cells with the change; `Snapshot` of a two-entity world round-trips through `FieldReader::from_bag` to the same Transform values; a snapshot larger than one datagram streams as a blob.
- [ ] Implement; gate; commit `editor-runtime: snapshot and layer readback`.

---

### Task 6: Host mirror (`editor-link`) and journeys

**Files:** `crates/editor-link/src/lib.rs` (`set_component(index, bag)`, `spawn(bags)`, `despawn(index)`, `request_snapshot()`, `read_layer(layer)`, `take_component_changes() -> Vec<(u32, Vec<u8>)>`, `take_added() -> Vec<u32>`, `take_blob()`), `tests/protocol.rs`.
- [ ] Journeys: inspector-style `set_component` of Transform moves the row; `spawn` a Transform+Camera entity → `EntityAdded` with the next index and a row; `despawn` → removed; cart-side drag → `ComponentChanged` for the moved entity's Transform; `request_snapshot` after edits → bags reflect the edits; `read_layer` after `set_cell` → the cell; drops injected on the cart→host path still complete.
- [ ] Implement; gate; commit `editor-link: set-component, spawn, despawn, snapshot, layer readback`.

---

### Task 7: Review; leave unmerged until Zed phase 4

- [ ] Gate; opus review of the phase diff against spec "Persistence (B)"; fix. Do not merge (Zed phase 4 merges both).
