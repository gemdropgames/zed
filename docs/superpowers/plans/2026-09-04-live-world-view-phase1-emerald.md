# Live World View, Phase 1 (emerald) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the emerald viewer cart drivable over the UART link (ggo-wire `CHANNEL_APP` datagrams) instead of a RAM-scanned mailbox, let a host toggle the game's own systems at runtime, ship the viewer as a `.ggo`, and teach the engine the editor's `[[background]]` world format.

**Architecture:** `emerald-editor-runtime` keeps `Mailbox` + `process()` untouched and gains `wire.rs` (the datagram codec, `no_std`, shared by cart and host) and `link.rs` (the cart-side pump: inbound datagrams → mailbox commands, outbound acks/diffs/statuses). A new std crate `emerald-editor-link` wraps the same codec in a host `LinkMailbox` with the read API today's `MailboxClient` has. `emd editor-cart` scaffolds an `editor_systems()` table in the game lib, and the template feeds it to `install`, which runs enabled entries after the editor system.

**Tech Stack:** Rust, `no_std + alloc` cart crates, `gemdrop_sdk::comm` (`send`/`recv`, from ggo `main` after Phase 0), `minijinja` templates in `emerald-cli`, `cargo test --workspace` in `~/projects/emerald`.

**Spec:** `docs/superpowers/specs/2026-09-04-live-world-view-design.md` (zed repo), sections "Link protocol" and "Emerald changes". Prerequisite: Phase 0 plan merged (`docs/superpowers/plans/2026-09-04-live-world-view-phase0-ggo.md`).

## Global Constraints

- All work in `~/projects/emerald`, branch `live-world-view`. Gates before every commit: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. Cart-target build check for `no_std` crates: `cargo build -p emerald-editor-runtime --target riscv32imc-unknown-none-elf`.
- Path dep `gemdrop-sdk` resolves to `../ggo/sdk/gemdrop-sdk` on ggo `main`; `comm::send`/`comm::recv` exist there after Phase 0.
- Wire payloads ≤ 255 bytes (`ggo_wire::MAX_PAYLOAD`). Kind byte first, integers little-endian, strings length-prefixed with one `u8`.
- Protocol version constant `LINK_PROTO_VERSION: u8 = 1`.
- `Mailbox` layout, `MAILBOX_MAGIC`, `MAX_ENTITIES`, `WORLD_BUF_BYTES`, `LAYER_BUF_BYTES`, `process()` and the RAM-scan host path stay byte-for-byte as they are; `emerald-editor` keeps working without edits except the `install` signature change in the editor-cart template.
- No `unwrap()` outside tests; the cart never panics on a malformed datagram.
- Commit messages: short imperative subject, no AI trailers.
- Line numbers below are as of 2026-09-04; locate by symbol if they drifted.

---

## File structure

| Path | Responsibility |
|------|----------------|
| `crates/core/src/gfx/tilemap.rs` | `TileLayer::Slot(u32)` so a `Tilemap` can sit on any of the four hardware layers. |
| `crates/world/src/lib.rs` | `Layers` becomes four slots; `VERSION = 5`; `apply_layers` spawns per slot. |
| `crates/world/src/encode.rs` | `[[background]]` header encoding + instance background merge (worldlib precedence). |
| `crates/editor-runtime/src/wire.rs` | New: datagram kinds, encode/decode for both directions. Pure, `no_std`. |
| `crates/editor-runtime/src/link.rs` | New: `CartLink` trait (`SdkLink` on riscv, test doubles elsewhere), `LinkState`, `pump_inbound`, `pump_outbound`. |
| `crates/editor-runtime/src/sync.rs` | `install(world, schedule, reg, systems)`, `EditorSystems` resource, `user_systems`, `editor_system` calls the pumps. |
| `crates/editor-link/` | New std crate `emerald-editor-link`: `LinkIo` trait, `LinkMailbox`, protocol e2e tests against the runtime. |
| `crates/cli/src/commands/editor_cart.rs` | `editor_systems()` stub + marker, `--ggo` flag. |
| `crates/cli/src/commands/generate.rs`, `rm.rs` | Splice/strip `("name", crate::systems::name::run),` at the systems marker. |
| `crates/cli/templates/editor-cart/src/main.rs.jinja` | Pass `editor_systems()` to `install`. |
| `crates/cli/src/commands/pack.rs` | `pack_ggo_elf(project, elf, out)` shared by `pack-ggo` and `editor-cart --ggo`. |

---

### Task 1: Four background slots in the engine and world format

**Files:**
- Modify: `crates/core/src/gfx/tilemap.rs:24-38`
- Modify: `crates/world/src/lib.rs` (`VERSION` ~line 107, `LayerRef`/`Layers`/`parse_layers`/`layers` ~lines 995-1050)
- Modify: `crates/world/src/boot.rs:33-50` (`apply_layers`)
- Modify: `crates/world/src/encode.rs` (`encode_toml`, `encode_toml_at`, `collect_world`, `encode_entities`, `write_layers_header` ~line 259)
- Test: `crates/world/src/encode.rs` tests module, `crates/world/src/lib.rs` tests

**Interfaces:**
- Produces: `TileLayer::Slot(u32)` (`hw()` returns the number); `pub struct Layers<'a> { pub slots: [Option<LayerRef<'a>>; 4] }`; `emerald_world::VERSION == 5`; `encode_toml`/`encode_toml_at` read `[[background]] { layer = 0..3, map = "<path>.map" }`; `apply_layers(world, &Layers)` spawns one `Tilemap` per present slot on `TileLayer::Slot(i)`.

- [ ] **Step 1: Write the failing encoder tests** (in `crates/world/src/encode.rs`'s `tests` module)

```rust
    #[test]
    fn backgrounds_encode_into_four_header_slots() {
        let src = "[[background]]\nlayer = 0\nmap = \"maps/main.bg0.map\"\n\n[[background]]\nlayer = 3\nmap = \"maps/main.bg3.map\"\n\n[[entity]]\nTransform = { pos = [0, 0] }\n";
        let blob = encode_toml(src).unwrap();
        let layers = crate::layers(&blob).unwrap();
        assert_eq!(layers.slots[0].map(|l| l.stem), Some("maps/main.bg0"));
        assert!(layers.slots[1].is_none());
        assert!(layers.slots[2].is_none());
        assert_eq!(layers.slots[3].map(|l| l.stem), Some("maps/main.bg3"));
    }

    #[test]
    fn a_layers_table_is_rejected_by_name() {
        let src = "[layers]\nbg = \"maps/old\"\n\n[[entity]]\nTransform = { pos = [0, 0] }\n";
        let err = encode_toml(src).unwrap_err().to_string();
        assert!(err.contains("[layers]"), "names the retired table: {err}");
        assert!(err.contains("[[background]]"), "names the replacement: {err}");
    }

    #[test]
    fn background_slot_out_of_range_is_an_error() {
        let src = "[[background]]\nlayer = 4\nmap = \"maps/x.map\"\n";
        assert!(encode_toml(src).is_err());
    }

    #[test]
    fn instance_backgrounds_merge_with_worldlib_precedence() {
        // priority instances (in order) > root > non-priority instances (in order);
        // first claimant per slot wins.
        let dir = std::env::temp_dir().join(format!("emerald-bg-merge-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("worlds")).unwrap();
        std::fs::write(
            dir.join("worlds/prio.toml"),
            "[[background]]\nlayer = 0\nmap = \"maps/prio0.map\"\n[[background]]\nlayer = 1\nmap = \"maps/prio1.map\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("worlds/plain.toml"),
            "[[background]]\nlayer = 1\nmap = \"maps/plain1.map\"\n[[background]]\nlayer = 2\nmap = \"maps/plain2.map\"\n",
        )
        .unwrap();
        let root = "[[background]]\nlayer = 0\nmap = \"maps/root0.map\"\n[[background]]\nlayer = 1\nmap = \"maps/root1.map\"\n\n[[instance]]\nworld = \"worlds/plain\"\npos = [0, 0]\n\n[[instance]]\nworld = \"worlds/prio\"\npos = [0, 0]\nbackground_priority = true\n";
        let blob = encode_toml_at(root, &dir).unwrap();
        let layers = crate::layers(&blob).unwrap();
        assert_eq!(layers.slots[0].map(|l| l.stem), Some("maps/prio0"));
        assert_eq!(layers.slots[1].map(|l| l.stem), Some("maps/prio1"));
        assert_eq!(layers.slots[2].map(|l| l.stem), Some("maps/plain2"));
        assert!(layers.slots[3].is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
```

And in `crates/world/src/lib.rs` tests (find the existing `layers` tests and add):

```rust
    #[test]
    fn version_four_blob_is_rejected() {
        let mut blob = MAGIC.to_vec();
        blob.push(4);
        assert_eq!(layers(&blob).unwrap_err(), LoadError::BadVersion(4));
    }
```

- [ ] **Step 2: Run, verify they fail**

Run: `cd ~/projects/emerald && cargo test -p emerald-world backgrounds_ layers_table background_slot instance_backgrounds version_four`
Expected: compile errors (`slots` field missing) or failures.

- [ ] **Step 3: Implement**

`crates/core/src/gfx/tilemap.rs`:

```rust
pub enum TileLayer {
    /// Under sprites (opaque base).
    Bg,
    /// Over sprites (nibble 0 = transparent — overlays, occluders, text).
    Fg,
    /// Any of the four hardware layers by index — what a world's
    /// `[[background]]` slot maps to.
    Slot(u32),
}

impl TileLayer {
    pub const fn hw(self) -> u32 {
        match self {
            TileLayer::Bg => ppu::LAYER_BG,
            TileLayer::Fg => ppu::LAYER_FG,
            TileLayer::Slot(n) => n % ppu::LAYER_COUNT,
        }
    }
}
```

`crates/world/src/lib.rs`: `pub const VERSION: u8 = 5;` and

```rust
/// Number of hardware background layer slots a world header carries.
pub const BACKGROUND_SLOTS: usize = 4;

/// The `[[background]]` set of a world blob, one entry per hardware layer
/// slot. An absent slot is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layers<'a> {
    pub slots: [Option<LayerRef<'a>>; BACKGROUND_SLOTS],
}

fn parse_layers(blob: &[u8]) -> Result<(Layers<'_>, usize), LoadError> {
    if blob.get(0..4) != Some(&MAGIC) {
        return Err(LoadError::BadMagic);
    }
    let version = *blob.get(4).ok_or(LoadError::Truncated)?;
    if version != VERSION {
        return Err(LoadError::BadVersion(version));
    }
    let mut off = 5;
    let mut slots = [None; BACKGROUND_SLOTS];
    for slot in &mut slots {
        *slot = take_layer_ref(blob, &mut off)?;
    }
    Ok((Layers { slots }, off))
}
```

`crates/world/src/boot.rs`:

```rust
pub fn apply_layers(world: &mut emerald_core::World, l: &Layers<'_>) {
    use emerald_core::gfx::{TileLayer, Tilemap};
    for (slot, layer) in l.slots.iter().enumerate() {
        let Some(layer) = layer else { continue };
        let e = world.spawn();
        let tag = crate::map_tag(layer.stem);
        world.insert(
            e,
            Tilemap::load_map_at(&tag, TileLayer::Slot(slot as u32), layer.col as u32, layer.row as u32),
        );
    }
}
```

`crates/world/src/encode.rs`: replace `write_layers_header` and thread backgrounds through:

```rust
/// One resolved `[[background]]` row: hardware slot + map stem.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BackgroundRow {
    layer: usize,
    stem: String,
}

/// Read a doc's `[[background]]` rows. `map` is the editor's convention, a
/// project-relative `.map` path (`maps/main.bg0.map`); the header stores
/// the stem, which `map_tag` turns back into the `.map` asset tag on the
/// cart. A `[layers]` table is the retired format and is named as such.
fn read_backgrounds(doc: &toml::Value) -> Result<Vec<BackgroundRow>> {
    if doc.get("layers").is_some() {
        bail!("`[layers]` is no longer a world header; use `[[background]]` rows (layer = 0..3, map = \"maps/<stem>.map\")");
    }
    let Some(rows) = doc.get("background") else {
        return Ok(Vec::new());
    };
    let rows = rows
        .as_array()
        .context("`background` must be an array of tables ([[background]])")?;
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let layer = row
            .get("layer")
            .and_then(|v| v.as_integer())
            .with_context(|| format!("background {i}: `layer` must be an integer"))?;
        let layer = usize::try_from(layer)
            .ok()
            .filter(|l| *l < crate::BACKGROUND_SLOTS)
            .with_context(|| format!("background {i}: `layer` must be 0..{}", crate::BACKGROUND_SLOTS))?;
        let map = row
            .get("map")
            .and_then(|v| v.as_str())
            .with_context(|| format!("background {i}: `map` must be a string path"))?;
        let stem = map.strip_suffix(".map").unwrap_or(map).to_string();
        out.push(BackgroundRow { layer, stem });
    }
    Ok(out)
}

/// `ggo_worldlib::backgrounds::merge_backgrounds`'s rule, reimplemented so
/// the cart renders exactly the slots the editor shows: priority instances
/// (in `[[instance]]` order) claim first, then the root, then non-priority
/// instances (in order); the first claimant of a slot wins. Nested
/// instances inherit the priority of their top-level instance.
fn merge_backgrounds(
    root: &[BackgroundRow],
    instances: &[(bool, Vec<BackgroundRow>)],
) -> [Option<BackgroundRow>; crate::BACKGROUND_SLOTS] {
    let mut slots: [Option<BackgroundRow>; crate::BACKGROUND_SLOTS] = Default::default();
    let mut claim = |rows: &[BackgroundRow]| {
        for row in rows {
            if slots[row.layer].is_none() {
                slots[row.layer] = Some(row.clone());
            }
        }
    };
    for (priority, rows) in instances {
        if *priority {
            claim(rows);
        }
    }
    claim(root);
    for (priority, rows) in instances {
        if !*priority {
            claim(rows);
        }
    }
    slots
}

fn write_layers_header(
    out: &mut Vec<u8>,
    slots: &[Option<BackgroundRow>; crate::BACKGROUND_SLOTS],
) -> Result<()> {
    for slot in slots {
        let stem = slot.as_ref().map(|r| r.stem.as_str()).unwrap_or("");
        let len = u16::try_from(stem.len()).context("background stem longer than 65535 bytes")?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(stem.as_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    Ok(())
}
```

Change `encode_entities`'s first parameter from `layers: Option<&toml::Value>` to `slots: &[Option<BackgroundRow>; BACKGROUND_SLOTS]`. In `encode_toml`: `let slots = merge_backgrounds(&read_backgrounds(&doc)?, &[]);`. In `encode_toml_at`: extend `collect_world` with an `out_backgrounds: &mut Vec<(bool, Vec<BackgroundRow>)>` parameter and an `inherited_priority: Option<bool>` parameter; at each `[[instance]]` read `background_priority` (`table.get("background_priority").and_then(|v| v.as_bool()).unwrap_or(false)`), and when recursing push `(inherited_priority.unwrap_or(priority), read_backgrounds(&sub_doc)?)` for the sub-world. The root call passes `None`. Then `merge_backgrounds(&read_backgrounds(&doc)?, &collected)`.

Fix every other `Layers { bg, fg }` / `.bg` / `.fg` user: `grep -rn "\.bg\b\|\.fg\b\|Layers {" crates/ --include=*.rs` (expect `boot.rs`, world tests, possibly `crates/cli/src/worldlint.rs` and `crates/editor/src/worldfile.rs`). The editor's `worldfile.rs` reads `[layers]` for its BG/FG panel: replace its reads with `[[background]]` slots 0 and 1 (the editor's BG|Sprite|FG switch maps BG → slot 0, FG → slot 1); its writes likewise. Keep that edit minimal and covered by the editor's existing worldfile tests updated to the new shape.

- [ ] **Step 4: Run, verify pass**

Run: `cd ~/projects/emerald && cargo test --workspace`
Expected: all pass, including the updated editor worldfile tests and the cli e2e that scaffolds a world (`crates/cli/templates/new/assets/worlds/main.toml` has no `[layers]`, so it needs no change; confirm with `grep -rn "\[layers\]" crates/cli/templates`).

- [ ] **Step 5: Commit**

```bash
cd ~/projects/emerald && git add -A crates/core crates/world crates/cli crates/editor
git commit -m "world: [[background]] slots replace the [layers] header (v5)"
```

---

### Task 2: Datagram codec (`wire.rs`)

**Files:**
- Create: `crates/editor-runtime/src/wire.rs`
- Modify: `crates/editor-runtime/src/lib.rs` (add `pub mod wire;`)
- Test: inline `#[cfg(test)]` in `wire.rs`

**Interfaces:**
- Produces (all `no_std`, no alloc needed except `Vec` under `alloc`):

```rust
pub const LINK_PROTO_VERSION: u8 = 1;
pub const MAX_PAYLOAD: usize = 255;
pub const CHUNK_BYTES: usize = 240; // BlobChunk data per datagram

pub enum BlobKind { World, Layer }

pub enum HostMsg<'a> {
    Hello { version: u8 },
    SetTransform { id: u32, x: i32, y: i32 },
    Camera { x: i32, y: i32 },
    SetCell { layer: u8, x: u16, y: u16, tile: u16 },
    PreviewMetasprite { stem: &'a str, anim: &'a str },
    PreviewClear,
    SysMask { mask: u64 },
    BlobBegin { kind: BlobKind, len: u32, layer: u8, base: u16, budget: u16, stem: &'a str },
    BlobChunk { seq: u16, off: u32, data: &'a [u8] },
    BlobEnd { seq: u16 },
}

pub struct EntityRow { pub index: u32, pub x: i32, pub y: i32, pub w: u16, pub h: u16 }

pub enum CartMsg<'a> {
    HelloAck { version: u8, entity_cap: u16, world_cap: u32, layer_cap: u32, systems: SystemNames<'a> },
    Ack { seq: u16 },
    Schema { off: u16, data: &'a [u8] },
    Entities { rows: EntityRows<'a> },   // iterator over EntityRow, ≤ 15 per datagram
    EntityCount { count: u32 },
    LayerStatus { status: [u8; 4] },
    PreviewStatus { status: u8 },
    FrameSeq { seq: u32 },
}

pub fn encode_host(msg: &HostMsg, out: &mut [u8; MAX_PAYLOAD]) -> Option<usize>;
pub fn decode_host(buf: &[u8]) -> Option<HostMsg<'_>>;
pub fn encode_cart(msg: &CartMsg, out: &mut [u8; MAX_PAYLOAD]) -> Option<usize>;
pub fn decode_cart(buf: &[u8]) -> Option<CartMsg<'_>>;
pub const ENTITY_ROWS_PER_MSG: usize = 15;
```

`SystemNames<'a>` and `EntityRows<'a>` are small borrowed iterators over the encoded bytes (count-prefixed), so decoding never allocates.

- [ ] **Step 1: Write the failing round-trip tests** (bottom of `wire.rs`)

```rust
#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn host_rt(msg: HostMsg<'_>) -> std::vec::Vec<u8> {
        let mut out = [0u8; MAX_PAYLOAD];
        let n = encode_host(&msg, &mut out).expect("encodes");
        out[..n].to_vec()
    }

    #[test]
    fn host_messages_round_trip() {
        let bytes = host_rt(HostMsg::SetTransform { id: 7, x: -65536, y: 131072 });
        assert!(matches!(decode_host(&bytes), Some(HostMsg::SetTransform { id: 7, x: -65536, y: 131072 })));
        let bytes = host_rt(HostMsg::SetCell { layer: 2, x: 5, y: 9, tile: 1023 });
        assert!(matches!(decode_host(&bytes), Some(HostMsg::SetCell { layer: 2, x: 5, y: 9, tile: 1023 })));
        let bytes = host_rt(HostMsg::PreviewMetasprite { stem: "sprites/hero", anim: "walk" });
        match decode_host(&bytes) {
            Some(HostMsg::PreviewMetasprite { stem, anim }) => {
                assert_eq!(stem, "sprites/hero");
                assert_eq!(anim, "walk");
            }
            other => panic!("{other:?}"),
        }
        let bytes = host_rt(HostMsg::SysMask { mask: 0x8000_0000_0000_0005 });
        assert!(matches!(decode_host(&bytes), Some(HostMsg::SysMask { mask: 0x8000_0000_0000_0005 })));
    }

    #[test]
    fn blob_chunk_carries_up_to_chunk_bytes_and_no_more() {
        let data = [0xABu8; CHUNK_BYTES];
        let bytes = host_rt(HostMsg::BlobChunk { seq: 3, off: 480, data: &data });
        assert!(bytes.len() <= MAX_PAYLOAD);
        match decode_host(&bytes) {
            Some(HostMsg::BlobChunk { seq: 3, off: 480, data }) => assert_eq!(data, &data[..]),
            other => panic!("{other:?}"),
        }
        let too_big = [0u8; CHUNK_BYTES + 1];
        let mut out = [0u8; MAX_PAYLOAD];
        assert!(encode_host(&HostMsg::BlobChunk { seq: 0, off: 0, data: &too_big }, &mut out).is_none());
    }

    #[test]
    fn cart_entities_round_trip_fifteen_rows() {
        let rows: std::vec::Vec<EntityRow> = (0..15)
            .map(|i| EntityRow { index: i, x: i as i32 * 65536, y: -(i as i32), w: 16, h: 16 })
            .collect();
        let mut out = [0u8; MAX_PAYLOAD];
        let n = encode_cart(&CartMsg::Entities { rows: EntityRows::from_slice(&rows) }, &mut out).expect("15 rows fit");
        match decode_cart(&out[..n]) {
            Some(CartMsg::Entities { rows: decoded }) => {
                let decoded: std::vec::Vec<EntityRow> = decoded.collect();
                assert_eq!(decoded.len(), 15);
                assert_eq!(decoded[14].index, 14);
                assert_eq!(decoded[14].x, 14 * 65536);
            }
            other => panic!("{other:?}"),
        }
        let sixteen: std::vec::Vec<EntityRow> = (0..16).map(|i| EntityRow { index: i, x: 0, y: 0, w: 0, h: 0 }).collect();
        assert!(encode_cart(&CartMsg::Entities { rows: EntityRows::from_slice(&sixteen) }, &mut out).is_none());
    }

    #[test]
    fn hello_ack_carries_system_names() {
        let names = ["animate", "physics/step"];
        let mut out = [0u8; MAX_PAYLOAD];
        let n = encode_cart(
            &CartMsg::HelloAck { version: LINK_PROTO_VERSION, entity_cap: 256, world_cap: 32768, layer_cap: 8192, systems: SystemNames::from_slice(&names) },
            &mut out,
        )
        .unwrap();
        match decode_cart(&out[..n]) {
            Some(CartMsg::HelloAck { version: 1, entity_cap: 256, systems, .. }) => {
                let got: std::vec::Vec<&str> = systems.collect();
                assert_eq!(got, ["animate", "physics/step"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn garbage_decodes_to_none_never_panics() {
        for len in 0..40 {
            let junk: std::vec::Vec<u8> = (0..len).map(|i| (i * 37 % 251) as u8).collect();
            let _ = decode_host(&junk);
            let _ = decode_cart(&junk);
        }
        assert!(decode_host(&[0x02, 1, 2]).is_none(), "truncated SetTransform");
        assert!(decode_cart(&[0x84, 200]).is_none(), "row count past the buffer");
    }
}
```

`EntityRow`, `HostMsg`, `CartMsg` derive `Debug` (and `Copy`/`PartialEq` for `EntityRow`).

- [ ] **Step 2: Run, verify fail**

Run: `cd ~/projects/emerald && cargo test -p emerald-editor-runtime wire::`
Expected: compile error, module missing.

- [ ] **Step 3: Implement the codec**

Kind bytes: host `0x01 Hello, 0x02 SetTransform, 0x03 Camera, 0x04 SetCell, 0x05 PreviewMetasprite, 0x06 PreviewClear, 0x07 SysMask, 0x08 BlobBegin, 0x09 BlobChunk, 0x0A BlobEnd`; cart `0x81 HelloAck, 0x82 Ack, 0x83 Schema, 0x84 Entities, 0x85 EntityCount, 0x86 LayerStatus, 0x87 PreviewStatus, 0x88 FrameSeq`. Use a tiny cursor:

```rust
struct Writer<'a> { buf: &'a mut [u8; MAX_PAYLOAD], at: usize }
impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.at.checked_add(bytes.len())?;
        self.buf.get_mut(self.at..end)?.copy_from_slice(bytes);
        self.at = end;
        Some(())
    }
    fn u8(&mut self, v: u8) -> Option<()> { self.put(&[v]) }
    fn u16(&mut self, v: u16) -> Option<()> { self.put(&v.to_le_bytes()) }
    fn u32(&mut self, v: u32) -> Option<()> { self.put(&v.to_le_bytes()) }
    fn i32(&mut self, v: i32) -> Option<()> { self.put(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) -> Option<()> { self.put(&v.to_le_bytes()) }
    fn str(&mut self, s: &str) -> Option<()> {
        let len = u8::try_from(s.len()).ok()?;
        self.u8(len)?;
        self.put(s.as_bytes())
    }
}

struct Reader<'a> { buf: &'a [u8], at: usize }
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.buf.get(self.at..end)?;
        self.at = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> { self.take(1).map(|b| b[0]) }
    fn u16(&mut self) -> Option<u16> { self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]])) }
    fn u32(&mut self) -> Option<u32> { self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) }
    fn i32(&mut self) -> Option<i32> { self.u32().map(|v| v as i32) }
    fn u64(&mut self) -> Option<u64> { self.take(8).map(|b| u64::from_le_bytes(b.try_into().ok()?)) }
    fn str(&mut self) -> Option<&'a str> {
        let len = self.u8()? as usize;
        core::str::from_utf8(self.take(len)?).ok()
    }
    fn rest(&mut self) -> &'a [u8] { let r = &self.buf[self.at.min(self.buf.len())..]; self.at = self.buf.len(); r }
    fn done(&self) -> bool { self.at == self.buf.len() }
}
```

Each `encode_*` writes the kind then fields and returns `Some(w.at)`; each `decode_*` matches the kind byte, reads fields, and returns `None` unless `r.done()` (except `BlobChunk`/`Schema`, whose `data` is `r.rest()`). `EntityRows` encodes `count u8` then 16 bytes per row (`index u32, x i32, y i32, w u16, h u16`); `EntityRows::from_slice(&[EntityRow])` for encoding and an iterator over the borrowed bytes for decoding; `encode_cart` returns `None` when `count > ENTITY_ROWS_PER_MSG`. `SystemNames` encodes `count u8` then length-prefixed names; `from_slice(&[&str])` for encoding, iterator for decoding. `BlobChunk` refuses `data.len() > CHUNK_BYTES`.

- [ ] **Step 4: Run, verify pass; check the cart target still builds**

Run: `cd ~/projects/emerald && cargo test -p emerald-editor-runtime wire:: && cargo build -p emerald-editor-runtime --target riscv32imc-unknown-none-elf`
Expected: 5 passed; cart build exit 0.

- [ ] **Step 5: Commit**

```bash
cd ~/projects/emerald && git add crates/editor-runtime/src/wire.rs crates/editor-runtime/src/lib.rs
git commit -m "editor-runtime: link datagram codec"
```

---

### Task 3: Cart-side link pump and user systems

**Files:**
- Create: `crates/editor-runtime/src/link.rs`
- Modify: `crates/editor-runtime/src/sync.rs` (`EditorRuntime` ~line 306, `install` ~line 319, `editor_system` ~line 337)
- Modify: `crates/editor-runtime/src/lib.rs` (`pub mod link;`, re-export `SystemTable`)
- Modify: `crates/editor-runtime/Cargo.toml` (no new deps; `emerald_core::sys` already re-exports `gemdrop_sdk`)
- Test: `crates/editor-runtime/tests/link.rs` (new), existing `tests/sync.rs` updated for the new `install` signature

**Interfaces:**
- Consumes: `wire::*` (Task 2), `Mailbox`, `process`, `EditorState`, `encode_schemas`.
- Produces:

```rust
pub type SystemTable = &'static [(&'static str, emerald_core::System)];

pub trait CartLink {
    /// Pop one inbound datagram into `buf`; 0 when none is queued.
    fn recv(&mut self, buf: &mut [u8]) -> usize;
    /// Send one datagram; a failure is dropped (datagram semantics).
    fn send(&mut self, payload: &[u8]);
}

/// `gemdrop_sdk::comm` on the device; on host builds the SDK's stubs
/// make it a link that never receives and drops every send.
pub struct SdkLink;

pub struct LinkState { /* connected, pending acks, blob reassembly, last-sent entity rows, last statuses, hello_pending, sys names */ }

impl LinkState {
    pub const fn new(systems: SystemTable) -> Self;
    /// Drain inbound datagrams into the mailbox while it is idle
    /// (`cmd_seq == cmd_ack`). Returns how many were applied.
    pub fn pump_inbound(&mut self, mb: &mut Mailbox, link: &mut impl CartLink) -> usize;
    /// After `process()`: acks, hello reply, schema, entity diffs,
    /// statuses, frame seq.
    pub fn pump_outbound(&mut self, mb: &Mailbox, link: &mut impl CartLink);
    pub fn sys_mask(&self) -> u64;
}

pub fn install(world: &mut World, schedule: &mut Schedule, reg: SceneRegistry, systems: SystemTable);
pub struct EditorSystems { pub table: SystemTable, pub mask: u64 }   // Resource
pub fn user_systems(world: &mut World);                               // schedule slot after editor_system
```

- [ ] **Step 1: Write the failing tests** (`crates/editor-runtime/tests/link.rs`; `emerald-editor-runtime` compiles on the host, `tests/sync.rs` already builds `Mailbox`/`World` there)

```rust
use emerald_editor_runtime::link::{CartLink, LinkState};
use emerald_editor_runtime::wire::{self, CartMsg, HostMsg, MAX_PAYLOAD};
use emerald_editor_runtime::*;
use std::collections::VecDeque;

/// In-memory link: `inbox` is what the host sent, `outbox` what the cart sent.
#[derive(Default)]
struct VecLink {
    inbox: VecDeque<Vec<u8>>,
    outbox: Vec<Vec<u8>>,
}

impl CartLink for VecLink {
    fn recv(&mut self, buf: &mut [u8]) -> usize {
        let Some(msg) = self.inbox.pop_front() else { return 0 };
        let n = msg.len().min(buf.len());
        buf[..n].copy_from_slice(&msg[..n]);
        n
    }
    fn send(&mut self, payload: &[u8]) {
        self.outbox.push(payload.to_vec());
    }
}

impl VecLink {
    fn host_send(&mut self, msg: HostMsg<'_>) {
        let mut out = [0u8; MAX_PAYLOAD];
        let n = wire::encode_host(&msg, &mut out).expect("encodes");
        self.inbox.push_back(out[..n].to_vec());
    }
    fn drain_cart(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbox)
    }
}

fn noop(_: &mut emerald_core::World) {}
const SYSTEMS: link::SystemTable = &[("animate", noop), ("physics/step", noop)];

fn kinds(msgs: &[Vec<u8>]) -> Vec<u8> {
    msgs.iter().map(|m| m[0]).collect()
}

#[test]
fn hello_is_answered_with_ack_schema_and_full_entity_table() {
    let mut mb = Mailbox::new();
    mb.entity_count = 2;
    mb.entities[0] = EntityRect { index: 0, x: 0, y: 0, w: 16, h: 16 };
    mb.entities[1] = EntityRect { index: 1, x: 65536, y: 0, w: 16, h: 16 };
    mb.schema_len = 3;
    mb.schema_buf[..3].copy_from_slice(&[1, 0, 0]);
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();

    link.host_send(HostMsg::Hello { version: wire::LINK_PROTO_VERSION });
    assert_eq!(st.pump_inbound(&mut mb, &mut link), 1);
    st.pump_outbound(&mb, &mut link);

    let out = link.drain_cart();
    assert_eq!(kinds(&out), [0x81, 0x83, 0x84, 0x88], "HelloAck, Schema, Entities, FrameSeq");
    match wire::decode_cart(&out[0]) {
        Some(CartMsg::HelloAck { systems, .. }) => {
            assert_eq!(systems.collect::<Vec<_>>(), ["animate", "physics/step"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn nothing_is_published_before_hello() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    st.pump_outbound(&mb, &mut link);
    assert!(link.drain_cart().is_empty());
    mb.entity_count = 1;
    st.pump_outbound(&mb, &mut link);
    assert!(link.drain_cart().is_empty());
}

#[test]
fn set_transform_fills_the_mailbox_command_and_bumps_seq() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    link.host_send(HostMsg::SetTransform { id: 4, x: 100, y: -200 });
    st.pump_inbound(&mut mb, &mut link);
    assert_eq!(mb.cmd_kind, CMD_SET_TRANSFORM);
    assert_eq!((mb.cmd_id, mb.cmd_x, mb.cmd_y), (4, 100, -200));
    assert_eq!(mb.cmd_seq, mb.cmd_ack + 1);
}

#[test]
fn a_pending_command_blocks_further_inbound_until_acked() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    link.host_send(HostMsg::Camera { x: 1, y: 1 });
    link.host_send(HostMsg::Camera { x: 2, y: 2 });
    assert_eq!(st.pump_inbound(&mut mb, &mut link), 1);
    assert_eq!(mb.cmd_x, 1);
    mb.cmd_ack = mb.cmd_seq; // what process() does
    assert_eq!(st.pump_inbound(&mut mb, &mut link), 1);
    assert_eq!(mb.cmd_x, 2);
}

#[test]
fn world_blob_reassembles_in_order_and_acks_each_chunk() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    let blob: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8).collect();
    link.host_send(HostMsg::Hello { version: wire::LINK_PROTO_VERSION });
    link.host_send(HostMsg::BlobBegin { kind: wire::BlobKind::World, len: 600, layer: 0, base: 0, budget: 0, stem: "" });
    for (seq, chunk) in blob.chunks(wire::CHUNK_BYTES).enumerate() {
        link.host_send(HostMsg::BlobChunk { seq: seq as u16, off: (seq * wire::CHUNK_BYTES) as u32, data: chunk });
    }
    link.host_send(HostMsg::BlobEnd { seq: 3 });
    st.pump_inbound(&mut mb, &mut link);
    st.pump_outbound(&mb, &mut link);
    assert_eq!(mb.cmd_kind, CMD_LOAD_WORLD);
    assert_eq!(mb.world_len, 600);
    assert_eq!(&mb.world_buf[..600], &blob[..]);
    let acks: Vec<u16> = link
        .drain_cart()
        .iter()
        .filter_map(|m| match wire::decode_cart(m) { Some(CartMsg::Ack { seq }) => Some(seq), _ => None })
        .collect();
    assert_eq!(acks, [0, 1, 2, 3]);
}

#[test]
fn a_duplicate_chunk_is_reacked_and_a_gap_is_dropped() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    link.host_send(HostMsg::Hello { version: wire::LINK_PROTO_VERSION });
    link.host_send(HostMsg::BlobBegin { kind: wire::BlobKind::Layer, len: 10, layer: 2, base: 511, budget: 248, stem: "tiles/a" });
    link.host_send(HostMsg::BlobChunk { seq: 0, off: 0, data: &[1; 5] });
    link.host_send(HostMsg::BlobChunk { seq: 0, off: 0, data: &[1; 5] }); // duplicate after a lost ack
    link.host_send(HostMsg::BlobChunk { seq: 2, off: 5, data: &[2; 5] }); // gap: seq 1 missing
    st.pump_inbound(&mut mb, &mut link);
    st.pump_outbound(&mb, &mut link);
    let acks: Vec<u16> = link
        .drain_cart()
        .iter()
        .filter_map(|m| match wire::decode_cart(m) { Some(CartMsg::Ack { seq }) => Some(seq), _ => None })
        .collect();
    assert_eq!(acks, [0, 0], "duplicate re-acked, gap not acked");
    assert_eq!(mb.cmd_seq, mb.cmd_ack, "no command committed without BlobEnd");
}

#[test]
fn blob_end_for_a_layer_fills_the_layer_command() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    link.host_send(HostMsg::BlobBegin { kind: wire::BlobKind::Layer, len: 6, layer: 2, base: 511, budget: 248, stem: "tiles/a" });
    link.host_send(HostMsg::BlobChunk { seq: 0, off: 0, data: &[1, 0, 1, 0, 5, 0] });
    link.host_send(HostMsg::BlobEnd { seq: 1 });
    st.pump_inbound(&mut mb, &mut link);
    assert_eq!(mb.cmd_kind, CMD_LOAD_LAYER);
    assert_eq!((mb.cmd_id, mb.cmd_x, mb.cmd_y), (2, 511, 248));
    assert_eq!(mb.layer_len, 6);
    assert_eq!(decode_stem(&mb.layer_tileset), Some("tiles/a"));
}

#[test]
fn only_changed_entity_rows_are_republished() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    mb.entity_count = 3;
    for i in 0..3 {
        mb.entities[i] = EntityRect { index: i as u32, x: 0, y: 0, w: 16, h: 16 };
    }
    link.host_send(HostMsg::Hello { version: wire::LINK_PROTO_VERSION });
    st.pump_inbound(&mut mb, &mut link);
    st.pump_outbound(&mb, &mut link);
    link.drain_cart();

    st.pump_outbound(&mb, &mut link);
    assert_eq!(kinds(&link.drain_cart()), [0x88], "unchanged table: only FrameSeq");

    mb.entities[1].x = 65536;
    st.pump_outbound(&mb, &mut link);
    let out = link.drain_cart();
    assert_eq!(kinds(&out), [0x84, 0x88]);
    match wire::decode_cart(&out[0]) {
        Some(CartMsg::Entities { rows }) => {
            let rows: Vec<_> = rows.collect();
            assert_eq!(rows.len(), 1);
            assert_eq!((rows[0].index, rows[0].x), (1, 65536));
        }
        other => panic!("{other:?}"),
    }

    mb.entity_count = 1;
    st.pump_outbound(&mb, &mut link);
    assert!(kinds(&link.drain_cart()).contains(&0x85), "shrink publishes EntityCount");
}

#[test]
fn sys_mask_is_applied_without_touching_the_mailbox() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    link.host_send(HostMsg::SysMask { mask: 0b10 });
    st.pump_inbound(&mut mb, &mut link);
    assert_eq!(st.sys_mask(), 0b10);
    assert_eq!(mb.cmd_seq, mb.cmd_ack, "not a mailbox command");
}

#[test]
fn malformed_datagrams_are_ignored() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    link.inbox.push_back(vec![0x02, 1]); // truncated SetTransform
    link.inbox.push_back(vec![0xFF]);    // unknown kind
    link.inbox.push_back(vec![]);
    assert_eq!(st.pump_inbound(&mut mb, &mut link), 0);
    assert_eq!(mb.cmd_seq, 0);
}
```

Also in `tests/sync.rs`: every `install(world, schedule, reg)` call becomes `install(world, schedule, reg, &[])`.

- [ ] **Step 2: Run, verify fail**

Run: `cd ~/projects/emerald && cargo test -p emerald-editor-runtime --test link`
Expected: compile error, `link` module missing.

- [ ] **Step 3: Implement `link.rs`**

```rust
//! Cart side of the editor link: inbound datagrams become mailbox
//! commands, the mailbox's published regions go back out as datagrams.
//! `Mailbox`/`process()` never learn a link exists — this module is the
//! only thing that touches them besides the RAM-scan host path.

use alloc::vec::Vec;

use crate::mailbox::*;
use crate::wire::{self, BlobKind, CartMsg, EntityRow, EntityRows, HostMsg, SystemNames, MAX_PAYLOAD};

pub type SystemTable = &'static [(&'static str, emerald_core::System)];

pub trait CartLink {
    fn recv(&mut self, buf: &mut [u8]) -> usize;
    fn send(&mut self, payload: &[u8]);
}

/// The device link: `gemdrop_sdk::comm`. Host builds get the SDK's stubs
/// (never receives, sends succeed into nothing), which is what the
/// RAM-mailbox editor sees too.
pub struct SdkLink;

impl CartLink for SdkLink {
    fn recv(&mut self, buf: &mut [u8]) -> usize {
        emerald_core::sys::comm::recv(buf)
    }
    fn send(&mut self, payload: &[u8]) {
        // Datagram semantics: a stalled TX loses this datagram, the host
        // re-requests what it still needs (blob chunks are host-driven).
        let _ = emerald_core::sys::comm::send(payload);
    }
}

#[derive(Clone, Copy)]
struct Blob {
    kind: BlobKind,
    len: u32,
    layer: u8,
    base: u16,
    budget: u16,
    next_seq: u16,
    stem: [u8; LAYER_TILESET_BYTES],
}

pub struct LinkState {
    systems: SystemTable,
    connected: bool,
    hello_pending: bool,
    sys_mask: u64,
    blob: Option<Blob>,
    /// Acks owed this frame (chunk seqs, in arrival order).
    acks: Vec<u16>,
    /// What the host last saw: rows and count, layer/preview statuses.
    sent_rows: [EntityRect; MAX_ENTITIES],
    sent_count: u32,
    sent_layer_status: [u8; LAYER_COUNT],
    sent_preview_status: u8,
    /// Set when the host must be re-sent the whole table (hello, world load).
    resend_all: bool,
}

impl LinkState {
    pub const fn new(systems: SystemTable) -> Self {
        LinkState {
            systems,
            connected: false,
            hello_pending: false,
            sys_mask: 0,
            blob: None,
            acks: Vec::new(),
            sent_rows: [EntityRect { index: 0, x: 0, y: 0, w: 0, h: 0 }; MAX_ENTITIES],
            sent_count: 0,
            sent_layer_status: [0; LAYER_COUNT],
            sent_preview_status: 0,
            resend_all: false,
        }
    }

    pub fn sys_mask(&self) -> u64 {
        self.sys_mask
    }

    pub fn pump_inbound(&mut self, mb: &mut Mailbox, link: &mut impl CartLink) -> usize {
        let mut applied = 0;
        let mut buf = [0u8; MAX_PAYLOAD];
        while mb.cmd_seq == mb.cmd_ack {
            let n = link.recv(&mut buf);
            if n == 0 {
                break;
            }
            let Some(msg) = wire::decode_host(&buf[..n]) else {
                continue;
            };
            if self.apply(mb, msg) {
                applied += 1;
            }
        }
        applied
    }

    /// Returns whether the datagram was well-formed and acted on.
    fn apply(&mut self, mb: &mut Mailbox, msg: HostMsg<'_>) -> bool {
        match msg {
            HostMsg::Hello { version } => {
                self.connected = version == wire::LINK_PROTO_VERSION;
                self.hello_pending = self.connected;
                self.resend_all = true;
                self.blob = None;
            }
            HostMsg::SetTransform { id, x, y } => {
                if id as usize >= MAX_ENTITIES {
                    return false;
                }
                mb.cmd_kind = CMD_SET_TRANSFORM;
                mb.cmd_id = id;
                mb.cmd_x = x;
                mb.cmd_y = y;
                commit(mb);
            }
            HostMsg::Camera { x, y } => {
                mb.cmd_kind = CMD_CAMERA;
                mb.cmd_x = x;
                mb.cmd_y = y;
                commit(mb);
            }
            HostMsg::SetCell { layer, x, y, tile } => {
                if layer as usize >= LAYER_COUNT {
                    return false;
                }
                mb.cmd_kind = CMD_SET_CELL;
                mb.cmd_id = layer as u32;
                mb.cmd_x = x as i32;
                mb.cmd_y = y as i32;
                mb.cmd_tile = tile;
                commit(mb);
            }
            HostMsg::PreviewMetasprite { stem, anim } => {
                if !put_stem(&mut mb.preview_stem, stem) || !put_stem(&mut mb.preview_anim, anim) {
                    return false;
                }
                mb.cmd_kind = CMD_PREVIEW_METASPRITE;
                commit(mb);
            }
            HostMsg::PreviewClear => {
                mb.cmd_kind = CMD_PREVIEW_CLEAR;
                commit(mb);
            }
            HostMsg::SysMask { mask } => self.sys_mask = mask,
            HostMsg::BlobBegin { kind, len, layer, base, budget, stem } => {
                let cap = match kind {
                    BlobKind::World => WORLD_BUF_BYTES,
                    BlobKind::Layer => LAYER_BUF_BYTES,
                };
                if len as usize > cap || (matches!(kind, BlobKind::Layer) && layer as usize >= LAYER_COUNT) {
                    return false;
                }
                let mut stem_buf = [0u8; LAYER_TILESET_BYTES];
                if !put_stem(&mut stem_buf, stem) {
                    return false;
                }
                self.blob = Some(Blob { kind, len, layer, base, budget, next_seq: 0, stem: stem_buf });
            }
            HostMsg::BlobChunk { seq, off, data } => {
                let Some(blob) = self.blob.as_mut() else { return false };
                if seq.wrapping_add(1) == blob.next_seq {
                    self.acks.push(seq); // duplicate after a lost ack
                    return true;
                }
                if seq != blob.next_seq {
                    return false; // gap: the host resends from its own timeout
                }
                let end = (off as usize).checked_add(data.len());
                let Some(end) = end.filter(|e| *e <= blob.len as usize) else { return false };
                let target = match blob.kind {
                    BlobKind::World => &mut mb.world_buf[..],
                    BlobKind::Layer => &mut mb.layer_buf[..],
                };
                let Some(slot) = target.get_mut(off as usize..end) else { return false };
                slot.copy_from_slice(data);
                blob.next_seq = blob.next_seq.wrapping_add(1);
                self.acks.push(seq);
            }
            HostMsg::BlobEnd { seq } => {
                let Some(blob) = self.blob.take() else { return false };
                if seq != blob.next_seq {
                    self.blob = Some(blob);
                    return false;
                }
                match blob.kind {
                    BlobKind::World => {
                        mb.world_len = blob.len;
                        mb.cmd_kind = CMD_LOAD_WORLD;
                        self.resend_all = true;
                    }
                    BlobKind::Layer => {
                        mb.layer_len = blob.len;
                        mb.layer_tileset = blob.stem;
                        mb.cmd_kind = CMD_LOAD_LAYER;
                        mb.cmd_id = blob.layer as u32;
                        mb.cmd_x = blob.base as i32;
                        mb.cmd_y = blob.budget as i32;
                    }
                }
                self.acks.push(seq);
                commit(mb);
            }
        }
        true
    }

    pub fn pump_outbound(&mut self, mb: &Mailbox, link: &mut impl CartLink) {
        let mut out = [0u8; MAX_PAYLOAD];
        let mut send = |msg: &CartMsg<'_>, link: &mut dyn FnMut(&[u8])| {
            if let Some(n) = wire::encode_cart(msg, &mut out) {
                link(&out[..n]);
            }
        };
        let mut tx = |bytes: &[u8]| link.send(bytes);

        for seq in self.acks.drain(..) {
            send(&CartMsg::Ack { seq }, &mut tx);
        }
        if !self.connected {
            return;
        }
        if core::mem::take(&mut self.hello_pending) {
            let names: Vec<&str> = self.systems.iter().map(|(n, _)| *n).collect();
            send(
                &CartMsg::HelloAck {
                    version: wire::LINK_PROTO_VERSION,
                    entity_cap: MAX_ENTITIES as u16,
                    world_cap: WORLD_BUF_BYTES as u32,
                    layer_cap: LAYER_BUF_BYTES as u32,
                    systems: SystemNames::from_slice(&names),
                },
                &mut tx,
            );
            let schema = &mb.schema_buf[..(mb.schema_len as usize).min(SCHEMA_BUF_BYTES)];
            for (i, chunk) in schema.chunks(wire::CHUNK_BYTES).enumerate() {
                send(&CartMsg::Schema { off: (i * wire::CHUNK_BYTES) as u16, data: chunk }, &mut tx);
            }
        }
        let count = (mb.entity_count as usize).min(MAX_ENTITIES);
        let resend_all = core::mem::take(&mut self.resend_all);
        let mut batch: Vec<EntityRow> = Vec::with_capacity(wire::ENTITY_ROWS_PER_MSG);
        for i in 0..count {
            let row = mb.entities[i];
            if resend_all || !same_rect(&row, &self.sent_rows[i]) {
                batch.push(EntityRow { index: row.index, x: row.x, y: row.y, w: row.w, h: row.h });
                self.sent_rows[i] = row;
                if batch.len() == wire::ENTITY_ROWS_PER_MSG {
                    send(&CartMsg::Entities { rows: EntityRows::from_slice(&batch) }, &mut tx);
                    batch.clear();
                }
            }
        }
        if !batch.is_empty() {
            send(&CartMsg::Entities { rows: EntityRows::from_slice(&batch) }, &mut tx);
        }
        if resend_all || count as u32 != self.sent_count {
            send(&CartMsg::EntityCount { count: count as u32 }, &mut tx);
            self.sent_count = count as u32;
        }
        if resend_all || mb.layer_status != self.sent_layer_status {
            send(&CartMsg::LayerStatus { status: mb.layer_status }, &mut tx);
            self.sent_layer_status = mb.layer_status;
        }
        if resend_all || mb.preview_status != self.sent_preview_status {
            send(&CartMsg::PreviewStatus { status: mb.preview_status }, &mut tx);
            self.sent_preview_status = mb.preview_status;
        }
        send(&CartMsg::FrameSeq { seq: mb.frame_seq }, &mut tx);
    }
}

fn commit(mb: &mut Mailbox) {
    mb.cmd_seq = mb.cmd_seq.wrapping_add(1);
}

fn put_stem<const N: usize>(dst: &mut [u8; N], s: &str) -> bool {
    if s.len() >= N {
        return false;
    }
    dst.fill(0);
    dst[..s.len()].copy_from_slice(s.as_bytes());
    true
}

fn same_rect(a: &EntityRect, b: &EntityRect) -> bool {
    a.index == b.index && a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h
}
```

Adjust the test `hello_is_answered_with_ack_schema_and_full_entity_table`'s expected kinds to what this emits: `[0x81, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88]` (hello resends everything). The borrow dance around `send`/`tx` can be simplified by making `send` a method taking `&mut impl CartLink`; do whatever compiles cleanly, the wire output is what the tests pin.

`sync.rs`:

```rust
/// The game's systems the host may switch on while editing, and which
/// are currently on (bit i = table entry i; entries past 63 never run).
pub struct EditorSystems {
    pub table: crate::link::SystemTable,
    pub mask: u64,
}
impl emerald_core::Resource for EditorSystems {}

#[derive(Default)]
struct EditorRuntime {
    reg: SceneRegistry,
    state: EditorState,
    link: Option<crate::link::LinkState>,
}

pub fn install(world: &mut World, schedule: &mut Schedule, reg: SceneRegistry, systems: crate::link::SystemTable) {
    let mb = unsafe { &mut *mailbox_ptr() };
    let (len, truncated) = encode_schemas(reg.schemas(), &mut mb.schema_buf);
    mb.schema_len = len as u32;
    mb.schema_truncated = truncated as u8;

    world.insert_resource(EditorRuntime {
        reg,
        state: EditorState::default(),
        link: Some(crate::link::LinkState::new(systems)),
    });
    world.insert_resource(EditorSystems { table: systems, mask: 0 });
    schedule.add(editor_system);
    schedule.add(user_systems);
}

pub fn editor_system(world: &mut World) {
    let mut rt = core::mem::take(world.resource_mut::<EditorRuntime>());
    let mb = unsafe { &mut *mailbox_ptr() };
    let mut link = crate::link::SdkLink;
    if let Some(st) = rt.link.as_mut() {
        st.pump_inbound(mb, &mut link);
    }
    process(mb, world, &rt.reg, &mut rt.state);
    if let Some(st) = rt.link.as_mut() {
        st.pump_outbound(mb, &mut link);
        world.resource_mut::<EditorSystems>().mask = st.sys_mask();
    }
    *world.resource_mut::<EditorRuntime>() = rt;
}

/// Runs every table entry whose mask bit is set, after the editor system
/// applied this frame's command, so a host-toggled animation or physics
/// system sees the edited world.
pub fn user_systems(world: &mut World) {
    let (table, mask) = {
        let s = world.resource::<EditorSystems>();
        (s.table, s.mask)
    };
    for (i, (_, run)) in table.iter().enumerate().take(64) {
        if mask & (1u64 << i) != 0 {
            run(world);
        }
    }
}
```

`lib.rs`: `pub mod link; pub mod wire; pub use link::SystemTable;`.

- [ ] **Step 4: Run, verify pass; cart target**

Run: `cd ~/projects/emerald && cargo test -p emerald-editor-runtime && cargo build -p emerald-editor-runtime --target riscv32imc-unknown-none-elf`
Expected: link tests + updated sync tests pass; cart build exit 0.

- [ ] **Step 5: Commit**

```bash
cd ~/projects/emerald && git add crates/editor-runtime
git commit -m "editor-runtime: UART link pump and host-toggled user systems"
```

---

### Task 4: Host crate `emerald-editor-link`

**Files:**
- Create: `crates/editor-link/Cargo.toml`, `crates/editor-link/src/lib.rs`, `crates/editor-link/tests/protocol.rs`
- Modify: root `Cargo.toml` workspace `members` (add `crates/editor-link`)

**Interfaces:**
- Consumes: `emerald_editor_runtime::{wire, link, Mailbox, process, EditorState, ...}`; `emerald_editor_runtime::wire::{encode_host, decode_cart}`.
- Produces:

```rust
pub trait LinkIo {
    fn send(&mut self, payload: &[u8]) -> std::io::Result<()>;
    /// Every datagram received since the last call.
    fn recv(&mut self) -> Vec<Vec<u8>>;
}

pub struct SchemaEntry { pub name: String, pub fields: Vec<SchemaField> }
pub struct SchemaField { pub name: String, pub kind: FieldKind }
pub enum FieldKind { Int, Fixed, Bool, Str, Vec2, AssetRef { ext: String } }
pub enum LayerStatus { None, Loaded, Error, Clamped }
pub enum PreviewStatus { None, Active, Error }

pub struct LinkMailbox<L: LinkIo> { .. }

impl<L: LinkIo> LinkMailbox<L> {
    pub fn new(io: L) -> Self;
    pub fn io_mut(&mut self) -> &mut L;
    /// Send Hello; call `poll` until `is_connected`.
    pub fn hello(&mut self) -> io::Result<()>;
    /// Drain inbound datagrams, update the mirror, retry any blob chunk
    /// whose ack is overdue. Returns true when something changed.
    pub fn poll(&mut self, now: Instant) -> io::Result<bool>;
    pub fn is_connected(&self) -> bool;
    pub fn proto_version_mismatch(&self) -> Option<u8>;
    pub fn system_names(&self) -> &[String];
    pub fn entities(&self) -> &[EntityRow];
    pub fn schemas(&self) -> &[SchemaEntry];
    pub fn layer_status(&self) -> [LayerStatus; 4];
    pub fn preview_status(&self) -> PreviewStatus;
    pub fn frame_seq(&self) -> u32;
    pub fn busy(&self) -> bool;   // a blob transfer is in flight
    pub fn set_transform(&mut self, index: u32, x: i32, y: i32) -> io::Result<()>;
    pub fn set_camera(&mut self, x: i32, y: i32) -> io::Result<()>;
    pub fn set_cell(&mut self, layer: u32, x: u16, y: u16, tile: u16) -> io::Result<()>;
    pub fn preview_metasprite(&mut self, stem: &str, anim: &str) -> io::Result<()>;
    pub fn preview_clear(&mut self) -> io::Result<()>;
    pub fn set_sys_mask(&mut self, mask: u64) -> io::Result<()>;
    /// Queue a world blob; chunks go out one at a time as acks return.
    pub fn load_world(&mut self, blob: &[u8]) -> io::Result<()>;
    pub fn load_layer(&mut self, layer: u32, base: u16, budget: u16, map_bytes: &[u8], tileset_stem: &str) -> io::Result<()>;
}

pub const ACK_TIMEOUT: Duration = Duration::from_millis(100);
pub const MAX_RETRIES: u32 = 20;
```

- [ ] **Step 1: Create the crate skeleton**

`crates/editor-link/Cargo.toml`:

```toml
[package]
name              = "emerald-editor-link"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description       = "Host side of the emerald editor link: drive a viewer cart over ggo-wire APP datagrams"

[dependencies]
emerald-editor-runtime = { path = "../editor-runtime" }

[dev-dependencies]
emerald-core  = { path = "../core" }
emerald-world = { path = "../world", features = ["std"] }

[lints]
workspace = true
```

Add `"crates/editor-link"` to the root `Cargo.toml` `members`.

- [ ] **Step 2: Write the failing protocol tests** (`crates/editor-link/tests/protocol.rs`; this is the spec's "no emulator needed" proof)

```rust
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use emerald_editor_link::{LayerStatus, LinkIo, LinkMailbox, ACK_TIMEOUT};
use emerald_editor_runtime::link::{CartLink, LinkState};
use emerald_editor_runtime::*;

/// Two queues shared by both ends; `drop_next_host_send` simulates a lost
/// datagram on the wire.
#[derive(Default)]
struct Wire {
    to_cart: VecDeque<Vec<u8>>,
    to_host: VecDeque<Vec<u8>>,
    drop_next_host_send: bool,
}

struct HostEnd(Rc<RefCell<Wire>>);
impl LinkIo for HostEnd {
    fn send(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let mut w = self.0.borrow_mut();
        if std::mem::take(&mut w.drop_next_host_send) {
            return Ok(());
        }
        w.to_cart.push_back(payload.to_vec());
        Ok(())
    }
    fn recv(&mut self) -> Vec<Vec<u8>> {
        self.0.borrow_mut().to_host.drain(..).collect()
    }
}

struct CartEnd(Rc<RefCell<Wire>>);
impl CartLink for CartEnd {
    fn recv(&mut self, buf: &mut [u8]) -> usize {
        let Some(m) = self.0.borrow_mut().to_cart.pop_front() else { return 0 };
        let n = m.len().min(buf.len());
        buf[..n].copy_from_slice(&m[..n]);
        n
    }
    fn send(&mut self, payload: &[u8]) {
        self.0.borrow_mut().to_host.push_back(payload.to_vec());
    }
}

fn noop(_: &mut emerald_core::World) {}

/// One cart frame: pump in, process, pump out — what `editor_system` does.
struct Cart {
    world: emerald_core::World,
    reg: emerald_world::SceneRegistry,
    st: EditorState,
    link: LinkState,
    mb: Box<Mailbox>,
    end: CartEnd,
}

impl Cart {
    fn new(wire: &Rc<RefCell<Wire>>) -> Self {
        let reg = emerald_world::SceneRegistry::with_builtins();
        let mut mb = Box::new(Mailbox::new());
        let (len, _) = encode_schemas(reg.schemas(), &mut mb.schema_buf);
        mb.schema_len = len as u32;
        Cart {
            world: emerald_core::World::new(),
            reg,
            st: EditorState::default(),
            link: LinkState::new(&[("animate", noop), ("ai", noop)]),
            mb,
            end: CartEnd(wire.clone()),
        }
    }
    fn frame(&mut self) {
        self.link.pump_inbound(&mut self.mb, &mut self.end);
        process(&mut self.mb, &mut self.world, &self.reg, &mut self.st);
        self.link.pump_outbound(&self.mb, &mut self.end);
    }
}

fn pair() -> (LinkMailbox<HostEnd>, Cart) {
    let wire = Rc::new(RefCell::new(Wire::default()));
    let cart = Cart::new(&wire);
    (LinkMailbox::new(HostEnd(wire)), cart)
}

fn connect(host: &mut LinkMailbox<HostEnd>, cart: &mut Cart) {
    host.hello().unwrap();
    cart.frame();
    host.poll(Instant::now()).unwrap();
    assert!(host.is_connected());
}

#[test]
fn handshake_publishes_schema_and_system_names() {
    let (mut host, mut cart) = pair();
    connect(&mut host, &mut cart);
    assert_eq!(host.system_names(), ["animate", "ai"]);
    assert!(host.schemas().iter().any(|s| s.name == "Transform"));
}

#[test]
fn a_world_loads_over_the_link_and_its_entities_publish() {
    let (mut host, mut cart) = pair();
    connect(&mut host, &mut cart);
    let blob = emerald_world::encode_toml(
        "[[entity]]\nTransform = { pos = [3, 4] }\n\n[[entity]]\nTransform = { pos = [10, 20] }\n",
    )
    .unwrap();
    host.load_world(&blob).unwrap();
    let now = Instant::now();
    // stop-and-wait: one chunk per frame until the cart acks BlobEnd
    for _ in 0..8 {
        cart.frame();
        host.poll(now).unwrap();
        if !host.busy() {
            break;
        }
    }
    assert!(!host.busy(), "blob transfer completed");
    cart.frame(); // the LOAD_WORLD command is processed on the next frame
    host.poll(now).unwrap();
    let rows = host.entities();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].index, rows[0].x, rows[0].y), (0, 3 << 16, 4 << 16));
    assert_eq!((rows[1].index, rows[1].x, rows[1].y), (1, 10 << 16, 20 << 16));
}

#[test]
fn a_lost_chunk_is_retried_after_the_ack_timeout() {
    let (mut host, mut cart) = pair();
    connect(&mut host, &mut cart);
    let blob: Vec<u8> = {
        let mut src = String::new();
        for i in 0..40 {
            src.push_str(&format!("[[entity]]\nTransform = {{ pos = [{i}, 0] }}\n\n"));
        }
        emerald_world::encode_toml(&src).unwrap()
    };
    assert!(blob.len() > emerald_editor_runtime::wire::CHUNK_BYTES * 2, "needs several chunks");
    host.load_world(&blob).unwrap();
    let t0 = Instant::now();
    cart.frame();
    host.poll(t0).unwrap();
    host.io_mut().0.borrow_mut().drop_next_host_send = true; // the next chunk vanishes
    cart.frame();
    host.poll(t0).unwrap();
    cart.frame();
    host.poll(t0 + Duration::from_millis(10)).unwrap();
    assert!(host.busy(), "still waiting on the lost chunk");
    host.poll(t0 + ACK_TIMEOUT + Duration::from_millis(1)).unwrap(); // retry fires
    for k in 0..64 {
        cart.frame();
        host.poll(t0 + ACK_TIMEOUT * 2 + Duration::from_millis(k)).unwrap();
        if !host.busy() {
            break;
        }
    }
    assert!(!host.busy());
    cart.frame();
    host.poll(Instant::now()).unwrap();
    assert_eq!(host.entities().len(), 40);
}

#[test]
fn set_transform_moves_the_entity_and_the_diff_comes_back() {
    let (mut host, mut cart) = pair();
    connect(&mut host, &mut cart);
    let blob = emerald_world::encode_toml("[[entity]]\nTransform = { pos = [0, 0] }\n").unwrap();
    host.load_world(&blob).unwrap();
    for _ in 0..6 {
        cart.frame();
        host.poll(Instant::now()).unwrap();
    }
    host.set_transform(0, 5 << 16, 6 << 16).unwrap();
    cart.frame();
    host.poll(Instant::now()).unwrap();
    assert_eq!((host.entities()[0].x, host.entities()[0].y), (5 << 16, 6 << 16));
}

#[test]
fn layer_load_reports_status_and_cells_poke_after() {
    let (mut host, mut cart) = pair();
    connect(&mut host, &mut cart);
    let map = {
        let mut m = Vec::new();
        m.extend_from_slice(&2u16.to_le_bytes());
        m.extend_from_slice(&2u16.to_le_bytes());
        for _ in 0..4 {
            m.extend_from_slice(&BLANK_TILE.to_le_bytes());
        }
        m
    };
    host.load_layer(1, 511, 248, &map, BLANK_LAYER_STEM).unwrap();
    for _ in 0..6 {
        cart.frame();
        host.poll(Instant::now()).unwrap();
    }
    cart.frame();
    host.poll(Instant::now()).unwrap();
    assert!(matches!(host.layer_status()[1], LayerStatus::Loaded | LayerStatus::Clamped));
    host.set_cell(1, 0, 0, 3).unwrap();
    cart.frame(); // no observable status change; just must not error
    host.poll(Instant::now()).unwrap();
}

#[test]
fn sys_mask_reaches_the_cart() {
    let (mut host, mut cart) = pair();
    connect(&mut host, &mut cart);
    host.set_sys_mask(0b01).unwrap();
    cart.frame();
    assert_eq!(cart.link.sys_mask(), 0b01);
}

#[test]
fn a_version_mismatch_is_reported_not_connected() {
    let (mut host, mut cart) = pair();
    // Pretend the host speaks a newer protocol: send Hello with version 2 by hand.
    host.io_mut().send(&[0x01, 2]).unwrap();
    cart.frame();
    host.poll(Instant::now()).unwrap();
    assert!(!host.is_connected());
}
```

The last test relies on the cart ignoring a wrong-version Hello (it sets `connected = false` and answers nothing). To make the mismatch *reported*, the cart answers `HelloAck` with its own version even on mismatch and does not mark itself connected; `LinkMailbox::proto_version_mismatch()` then returns `Some(cart_version)`. Adjust `LinkState::apply`'s `Hello` arm: `self.hello_pending = true; self.connected = version == LINK_PROTO_VERSION;` and make `pump_outbound` send `HelloAck` when `hello_pending` even if not connected, then return.

- [ ] **Step 3: Run, verify fail**

Run: `cd ~/projects/emerald && cargo test -p emerald-editor-link`
Expected: compile errors (crate empty).

- [ ] **Step 4: Implement `LinkMailbox`**

`crates/editor-link/src/lib.rs` outline (fill each method against the wire enums; the tests pin behaviour):

```rust
//! Host side of the emerald editor link. Mirrors the parts of the cart's
//! `Mailbox` a host reads, and turns the `MailboxClient`-shaped calls the
//! editors make into `HostMsg` datagrams. Transport-agnostic through
//! [`LinkIo`]: an in-process emulator queue, a serial port, or a test pipe.

use std::io;
use std::time::{Duration, Instant};

use emerald_editor_runtime::wire::{self, BlobKind, CartMsg, EntityRow, HostMsg, MAX_PAYLOAD};

pub use emerald_editor_runtime::wire::LINK_PROTO_VERSION;

pub const ACK_TIMEOUT: Duration = Duration::from_millis(100);
pub const MAX_RETRIES: u32 = 20;

pub trait LinkIo {
    fn send(&mut self, payload: &[u8]) -> io::Result<()>;
    fn recv(&mut self) -> Vec<Vec<u8>>;
}

struct Transfer {
    kind: BlobKind,
    layer: u8,
    base: u16,
    budget: u16,
    stem: String,
    data: Vec<u8>,
    next_seq: u16,
    /// `None` until BlobBegin went out; then the seq in flight and when it went.
    in_flight: Option<(u16, Instant, u32)>,
    begun: bool,
    ended: bool,
}

pub struct LinkMailbox<L: LinkIo> {
    io: L,
    connected: bool,
    cart_version: Option<u8>,
    system_names: Vec<String>,
    entities: Vec<EntityRow>,
    entity_count: usize,
    schema_bytes: Vec<u8>,
    schemas: Vec<SchemaEntry>,
    layer_status: [LayerStatus; 4],
    preview_status: PreviewStatus,
    frame_seq: u32,
    transfer: Option<Transfer>,
    pending: std::collections::VecDeque<Transfer>,
}
```

Behaviour:

- `hello`: send `Hello { version: LINK_PROTO_VERSION }`; reset `connected`, clear entities/schemas.
- `poll(now)`: for each inbound datagram `decode_cart`: `HelloAck` → record version, names, caps; `connected = version == LINK_PROTO_VERSION`; `Schema` → splice bytes at `off`, re-decode with `decode_schemas` (port of `emerald-editor`'s `mailbox.rs::decode_schemas`, same packed format: `component_count u16`, then per component `name_len u8 + name`, `field_count u8`, per field `name_len u8 + name`, `kind_tag u8` (0 Int, 1 Fixed, 2 Bool, 3 Str, 4 Vec2, 5 AssetRef + `ext_len u8 + ext`)); `Entities` → upsert rows by `index` into `entities` (Vec sized to `entity_count`, sorted by index); `EntityCount` → truncate/extend; `LayerStatus`/`PreviewStatus`/`FrameSeq` → store; `Ack { seq }` → if `transfer.in_flight` matches `seq`, advance (`next_seq += 1`; if all chunks sent and `ended` not yet → send `BlobEnd`, else next chunk); after `BlobEnd`'s ack → `transfer = None`, pop `pending` and start it. Then retry: if `in_flight` is older than `ACK_TIMEOUT`, resend that chunk (or `BlobEnd`) and bump the retry count; past `MAX_RETRIES` → abort the transfer with `io::Error::new(TimedOut, "viewer cart stopped acking")`.
- `load_world`/`load_layer`: build a `Transfer`; if none active, start it: send `BlobBegin`, then the first `BlobChunk` (chunk `k` = `data[k*CHUNK_BYTES..]`, `off = k*CHUNK_BYTES`); else push to `pending`. A new `load_world` while one is pending replaces the pending world transfer (only the latest world matters).
- Single-datagram commands encode with `encode_host` and `io.send` immediately; `busy()` is `transfer.is_some()`.

- [ ] **Step 5: Run, verify pass**

Run: `cd ~/projects/emerald && cargo test -p emerald-editor-link && cargo clippy -p emerald-editor-link --all-targets -- -D warnings`
Expected: 7 passed, clippy clean.

- [ ] **Step 6: Commit**

```bash
cd ~/projects/emerald && git add Cargo.toml Cargo.lock crates/editor-link crates/editor-runtime
git commit -m "editor-link: host LinkMailbox over the UART datagram protocol"
```

---

### Task 5: `editor_systems()` scaffolding, generate/rm splice, template, `--ggo`

**Files:**
- Modify: `crates/cli/src/commands/editor_cart.rs` (`REGISTER_SCENE_STUB` ~line 11, `run` ~line 25, tests)
- Modify: `crates/cli/src/commands/generate.rs` (`edit_parent_mod` ~line 474, `register_in_scene` ~line 498)
- Modify: `crates/cli/src/commands/rm.rs` (`rm_system` ~line 93)
- Modify: `crates/cli/src/commands/pack.rs` (`pack_ggo` ~line 30)
- Modify: `crates/cli/src/main.rs` (`Cmd::EditorCart` ~line 79 and its dispatch ~line 144)
- Modify: `crates/cli/templates/editor-cart/src/main.rs.jinja`
- Test: `crates/cli/src/commands/editor_cart.rs` tests, `crates/cli/tests/editor_cart_scaffold.rs`, `crates/cli/tests/e2e_manifest.rs`

**Interfaces:**
- Produces: game lib fn `pub fn editor_systems() -> &'static [(&'static str, emerald_core::System)]` with marker `// emerald:editor-systems`; `emd editor-cart [--ggo]`; JSON trailer `{ "cart": <path>, "elf": <path>, "ggo": <path or null> }`; `pack::pack_ggo_elf(project, elf, out) -> Result<PathBuf>`.

- [ ] **Step 1: Write the failing tests**

In `editor_cart.rs` tests:

```rust
    #[test]
    fn editor_systems_stub_is_scaffolded_with_its_marker() {
        let lib = tmp_file("systems-fresh", "#![no_std]\n");
        ensure_editor_systems(&lib).unwrap();
        let out = std::fs::read_to_string(&lib).unwrap();
        assert!(out.contains("pub fn editor_systems() -> &'static [(&'static str, emerald_core::System)]"));
        assert_eq!(out.matches(EDITOR_SYSTEMS_MARKER).count(), 1);
        ensure_editor_systems(&lib).unwrap();
        assert_eq!(std::fs::read_to_string(&lib).unwrap().matches(EDITOR_SYSTEMS_MARKER).count(), 1, "idempotent");
        std::fs::remove_file(&lib).ok();
    }
```

In `crates/cli/tests/editor_cart_scaffold.rs` (follow the file's existing helpers):

```rust
#[test]
fn template_passes_editor_systems_to_install() {
    let main_rs = emerald_cli::templates::render("editor-cart/src/main.rs.jinja", &vars_for("demo")).unwrap();
    assert!(main_rs.contains("emerald_editor_runtime::install(world, schedule, reg, demo_core::editor_systems())"));
}
```

In `crates/cli/tests/e2e_manifest.rs`, after the existing `generate system` step (~line 160), add assertions that `crates/e2etest-core/src/lib.rs` now contains `("spawn", crate::systems::spawn::run),` and, after `rm system spawn`, no longer does. (The e2e runs `emd editor-cart` first? If not, add `emd(&dst, &["--json", "editor-cart"])` is too heavy — instead call `emd(&dst, &["--json", "generate", "system", "spawn"])` after the scaffold step that already exists, and have `generate system` scaffold the stub itself via `ensure_editor_systems` when missing, exactly as `register_in_scene` requires the scene marker.)

- [ ] **Step 2: Run, verify fail**

Run: `cd ~/projects/emerald && cargo test -p emerald-cli editor_systems template_passes`
Expected: compile errors on `ensure_editor_systems`/`EDITOR_SYSTEMS_MARKER`; template assertion fails.

- [ ] **Step 3: Implement**

`editor_cart.rs`:

```rust
pub const EDITOR_SYSTEMS_MARKER: &str = "// emerald:editor-systems";

/// Appended beside `register_scene`: the systems a host may switch on in
/// the viewer cart. Empty by default; `emd generate system` splices one
/// `(name, path)` row per generated system.
pub const EDITOR_SYSTEMS_STUB: &str = concat!(
    "\n/// Editor hook: this game's systems, by name, that the world editor may\n",
    "/// run live while editing (all off until the editor switches them on).\n",
    "pub fn editor_systems() -> &'static [(&'static str, emerald_core::System)] {\n",
    "    &[\n",
    "        // emerald:editor-systems\n",
    "    ]\n",
    "}\n",
);

pub(crate) fn ensure_editor_systems(lib_rs: &Path) -> Result<()> {
    let src = std::fs::read_to_string(lib_rs).with_context(|| format!("read {}", lib_rs.display()))?;
    if src.contains("pub fn editor_systems") {
        if !src.contains(EDITOR_SYSTEMS_MARKER) {
            bail!("{} has `editor_systems` without `{EDITOR_SYSTEMS_MARKER}` — add the marker inside its `&[ ]`", lib_rs.display());
        }
        return Ok(());
    }
    std::fs::write(lib_rs, format!("{src}{EDITOR_SYSTEMS_STUB}"))
        .with_context(|| format!("write {}", lib_rs.display()))
}

pub fn run(ggo: bool) -> Result<()> {
    let project = Project::discover(None)?;
    let lib_rs = project.src().join("lib.rs");
    ensure_register_scene(&lib_rs)?;
    ensure_editor_systems(&lib_rs)?;
    crate::util::ensure_dep(&project, "emerald-world")?;

    let elf = super::build::cargo_editor_cart_build(&project)?;
    let name_kebab = project.config.project.name.to_kebab_case();
    let out = project.root.join(format!("{name_kebab}-editor.cart"));
    let cart = super::pack::pack_with_save(&project, &elf, Some(&out), Some(0))?;
    let ggo_path = if ggo {
        let out = project.root.join(format!("{name_kebab}-editor.ggo"));
        Some(super::pack::pack_ggo_elf(&project, &elf, &out)?)
    } else {
        None
    };
    println!("{}", serde_json::json!({ "cart": cart, "elf": elf, "ggo": ggo_path }));
    Ok(())
}
```

`generate.rs`, in `edit_parent_mod`'s `Kind::System` arm's caller path (where `register_in_scene` is called for components), add for systems:

```rust
/// Splices `("name", crate::systems::name::run),` (module-scoped path for a
/// module system) at `editor_cart::EDITOR_SYSTEMS_MARKER`, scaffolding the
/// `editor_systems` stub first when the project predates it.
fn register_in_editor(project: &Project, scope: &Scope, name: &str) -> Result<()> {
    let lib_rs = project.src().join("lib.rs");
    crate::commands::editor_cart::ensure_editor_systems(&lib_rs)?;
    let path = match &scope.module_name {
        Some(m) => format!("crate::modules::{m}::systems::{name}::run"),
        None => format!("crate::systems::{name}::run"),
    };
    let label = match &scope.module_name {
        Some(m) => format!("{m}/{name}"),
        None => name.to_string(),
    };
    insert_at_marker(&lib_rs, crate::commands::editor_cart::EDITOR_SYSTEMS_MARKER, &format!("        ({label:?}, {path}),\n"))
}
```

`rm.rs` `rm_system`: after the two `strip_line` calls add a third for the lib row:

```rust
    let lib_rs = project.src().join("lib.rs");
    let label = if entry.module.is_empty() { entry.name.clone() } else { format!("{}/{}", entry.module, entry.name) };
    let path = if entry.module.is_empty() {
        format!("crate::systems::{}::run", entry.name)
    } else {
        format!("crate::modules::{}::systems::{}::run", entry.module, entry.name)
    };
    strip_line(project, &lib_rs, &format!("        ({label:?}, {path}),"), &mut stripped_files, &mut missing_lines)?;
```

`pack.rs`: extract the body of `pack_ggo` after `cargo_build` into

```rust
/// Pack an already-built ELF plus the project's baked assets into `out`
/// (a `.ggo`). Shared by `pack-ggo` and `editor-cart --ggo`.
pub fn pack_ggo_elf(project: &Project, elf: &Path, out: &Path) -> Result<PathBuf> { /* card dir, builder, toc, pack_with_full(..., Some(out), None, Some(&toc_path)) */ }
```

and have `pack_ggo` call it. `main.rs`: `EditorCart { #[arg(long)] ggo: bool }` → `commands::editor_cart::run(ggo)`.

Template `editor-cart/src/main.rs.jinja`, in `editor_main`:

```rust
        emerald_editor_runtime::install(world, schedule, reg, {{ name_snake }}_core::editor_systems());
```

- [ ] **Step 4: Run, verify pass**

Run: `cd ~/projects/emerald && cargo test -p emerald-cli && cargo test -p emerald-cli --test e2e_manifest -- --ignored` (the e2e is env-gated like the others; run it if the riscv toolchain is present).
Expected: pass.

- [ ] **Step 5: Commit**

```bash
cd ~/projects/emerald && git add crates/cli
git commit -m "emd: editor_systems scaffold, generate/rm splice, editor-cart --ggo"
```

---

### Task 6: Live check against the emulator (env-gated)

**Files:**
- Create: `crates/editor-link/tests/e2e_cart.rs`

**Interfaces:**
- Consumes: `ggo_emu_core::peripherals::Peripherals::{uart_inject, take_comm}` (Phase 0), `ggo_comm::MessageReader`, `emd editor-cart --ggo` (Task 5).

- [ ] **Step 1: Write the test**

Same gating as `crates/editor/tests/e2e_protocol.rs` (`--ignored`, env var naming a scaffolded project or scaffolding one with `emd new`). Boot the `.ggo` in a sandbox `ggo_emu_core` session (recipe: `Cart::parse`, `sandbox::plan`, `Mmu::with_plan`, `load_cart_body`, `Cpu::new(XIP_BASE + entry_offset)`, `enter_sandbox`, `Peripherals::new(0, save_bytes)` + `set_toc`), and implement `LinkIo` over it:

```rust
struct EmuIo<'a> { p: &'a mut Peripherals, reader: ggo_comm::MessageReader }
impl LinkIo for EmuIo<'_> {
    fn send(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let mut wire = Vec::new();
        ggo_wire::encode_payload(ggo_wire::channel::APP, payload, |b| wire.push(b));
        self.p.uart_inject(&wire);
        Ok(())
    }
    fn recv(&mut self) -> Vec<Vec<u8>> {
        self.reader
            .feed(&self.p.take_comm())
            .into_iter()
            .filter_map(|i| match i { ggo_comm::LinkItem::Message(m) if m.channel == ggo_wire::channel::APP => Some(m.payload().to_vec()), _ => None })
            .collect()
    }
}
```

Drive frames with `run_until_event` between `poll`s; assert: `HelloAck` arrives within 60 frames, `load_world` of the project's `assets/worlds/main.toml` (encoded with `encode_toml_at`) completes, and the published entity table has the file's entity count. Dev-deps: `ggo-emu-core`, `ggo-comm`, `ggo-wire` by path into `../../../ggo/...` exactly as `crates/editor` depends on `ggo-emu`.

- [ ] **Step 2: Run it**

Run: `cd ~/projects/emerald && cargo test -p emerald-editor-link --test e2e_cart -- --ignored`
Expected: pass with the toolchain present; skips cleanly otherwise.

- [ ] **Step 3: Commit and merge**

```bash
cd ~/projects/emerald && git add crates/editor-link && git commit -m "editor-link: sandbox emulator e2e"
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
git checkout main && git merge --ff-only live-world-view
```

Phase 2 (zed emu panel viewer run + link endpoint) depends on `emerald-editor-link`, `emerald-editor-runtime::wire` and `emd editor-cart --ggo` from this branch.
