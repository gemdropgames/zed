# Live World View Design

Date: 2026-09-04
Status: Approved design, pre-implementation

## Goal

Replace the ZedGG world panel's composed-pixel rendering of the open world
with a real emulation of that world: the game's own viewer cart runs in the
in-IDE emulator, renders through the real PPU, and the panel edits it live.
The panel talks to the cart only over the UART link cable protocol the
Gemdrop Go already supports (ggo-wire framed messages on `CHANNEL_APP`), with
ZedGG acting as the peer at the other end of the cable. The same bytes drive
a cart on a real board later.

The viewer cart runs an editor schedule: the editor sync system plus any of
the game's registered systems the user switches on at runtime.

Every existing editor feature survives: the current renderer stays as a
`Design` mode fallback, and the document side (ops, undo/redo, save,
inspector, lists, clipboard, painting, audio budget, remote reads) is not
touched.

Decisions taken during design:

- The unmerged ggo branch `worktree-uart-comm-link` is merged as phase 0 of
  this work, not assumed.
- Systems are toggled at runtime over the link, not by rebuilding.
- `Design` mode is kept as a fallback next to `Live`.
- Emerald adopts the editor's world format (`[[background]]` slots 0..3);
  the editor does not go back to `[layers]`.

## Repos and starting points

Three repos, all path-linked from `~/projects`:

| Repo | What exists today | What this design adds |
|------|-------------------|-----------------------|
| `ggo` (`main`) | `ggo-wire`, `ggo-comm`, firmware RX + `COMM_*` syscalls, emulator full-system `uart_inject`: all on branch `worktree-uart-comm-link` (12 commits, ~1200 behind `main`, 10 conflicting files). Sandbox (cart-XIP) emulator has no `COMM_*` arms. | Merge the branch. Sandbox-mode comm model in `ggo-emu-core`. |
| `emerald` (`main`) | `crates/editor-runtime`: `Mailbox` + `process()` + `editor_system`; `emd editor-cart` scaffolds `register_scene` and builds a viewer `.cart`; `crates/editor` (Iced) reaches the mailbox by scanning guest RAM. World encoder flattens `[[instance]]` (direct entities first, then instances depth-first) and encodes a `[layers]` bg/fg header. | UART transport for the mailbox, runtime-toggled user systems, `editor_systems()` game contract, `.ggo` viewer artifact, `[[background]]` format, a host `emerald-editor-link` crate. |
| `zed` (`ggo`) | `ggo_emu_panel`: sandbox emulator on its own thread, `Session` (input, pause/step, speed, RAM inspect tap, frame channel, uart log), `emulate_world` (`emd pack-ggo --world`), watch-mode re-pack, agent socket. `ggo_world_panel`: 11.7k lines, document side + `ggo-worldlib::render` pixel side. | Viewer run kind + link endpoint on `Session`; `Live` canvas mode in the world panel. |

## Architecture

```
ZedGG world panel (Live mode)
  LinkMailbox  (host mirror of the emerald Mailbox: typed commands,
                chunked blobs, seq/ack)
        │  APP datagrams, ggo-wire framed bytes
        ▼
EmuPanel Session ── uart_inject(bytes) / MessageReader over take_uart() ──► viewer cart
                                                                              editor_system:  link pump → Mailbox → process()   (process unchanged)
                                                                              user_systems:   mask-gated table of game systems
                                                                              engine render → PPU → framebuffer ──► Session frame channel ──► canvas
```

The cart never learns which peer it has. On hardware the same `LinkMailbox`
runs over `ggo_comm::GgoLink` on the uartd pty (phase 4).

## Link protocol

Every message is one ggo-wire `Message` on `CHANNEL_APP` (payload ≤ 255 B).
Payload byte 0 is the kind; integers are little-endian. Datagram semantics
are the wire's (no ordering guarantee, drops counted), so reliability is
applied only where it matters: blob transfers.

Host → cart:

| Kind | Fields | Notes |
|------|--------|-------|
| `Hello` | proto version u8 | Cart answers `HelloAck`, then streams `Schema`, then a full `Entities` set. Host resends until `HelloAck` arrives. |
| `SetTransform` | id u32, x i32, y i32 | Q16.16 raw, same as `CMD_SET_TRANSFORM`. |
| `Camera` | x i32, y i32 | `CMD_CAMERA`. |
| `SetCell` | layer u8, x u16, y u16, tile u16 | `CMD_SET_CELL`. |
| `PreviewMetasprite` | stem (len-prefixed), anim (len-prefixed) | `CMD_PREVIEW_METASPRITE`. |
| `PreviewClear` | | `CMD_PREVIEW_CLEAR`. |
| `SysMask` | mask u64 | Bit i enables entry i of the cart's system table. |
| `BlobBegin` | kind u8 (World / Layer), total len u32, layer u8, tileset stem (len-prefixed) | Resets reassembly. |
| `BlobChunk` | seq u16, off u32, bytes | Written into `world_buf` / `layer_buf` at `off`. |
| `BlobEnd` | seq u16 | Sets `world_len` / `layer_len`, fills the cmd fields, bumps `cmd_seq`. |

Cart → host:

| Kind | Fields | Notes |
|------|--------|-------|
| `HelloAck` | proto version u8, entity cap u16, world buf cap u32, layer buf cap u32, system names (count u8, each len-prefixed) | Also sent unsolicited once at boot so a host already listening learns of a reboot. |
| `Ack` | seq u16 | One per received `BlobChunk` / `BlobEnd`. |
| `Schema` | off u16, bytes | Chunks of the existing `schema_buf` encoding. |
| `Entities` | first index u32, rows (index u32, x i32, y i32, w u16, h u16)... | Only rows whose bytes changed since the last publish; the full table after `Hello`, after a world load, and when `entity_count` shrinks (then a final `EntityCount`). |
| `EntityCount` | count u32 | |
| `LayerStatus` | 4 × u8 | After each layer load. |
| `PreviewStatus` | u8 | After each preview command. |
| `FrameSeq` | seq u32 | Once per frame while a host is connected; the host uses it as the liveness signal. |

Rules:

- Single-datagram commands are fire-and-forget, exactly as today's
  busy-coalesced mailbox sends. A dropped `SetTransform` during a drag is
  corrected by the next one; the drop commit re-sends the final position.
- Blob chunks use stop-and-wait: the host sends one chunk, waits for its
  `Ack` (timeout 100 ms, 20 retries), then the next. The cart applies
  chunks idempotently by offset, so a duplicate after a lost `Ack` is
  harmless. In the emulator the link is lossless and no retry ever fires;
  on hardware a 32 KiB world takes about 3 s at 115200 baud, which is
  accepted for a world switch.
- The cart validates every field before touching the `Mailbox` (ids below
  `MAX_ENTITIES`, offsets inside the buffer, stems ≤ the stem caps). A
  malformed datagram is dropped and counted; it never panics the cart.
- Protocol version mismatch: `HelloAck` carries the version; the host shows
  "viewer cart predates the link protocol, rebuild" and stays in `Design`.

## Emerald changes

### `crates/editor-runtime`

- New `link.rs`. Inbound: drains `gemdrop_sdk::comm::recv` each frame,
  decodes datagrams, reassembles blobs into the static `Mailbox` and bumps
  `cmd_seq` exactly as the RAM host does. Outbound: acks, entity row diffs
  (a per-row copy of the last published bytes decides "changed"), statuses,
  `FrameSeq`, all through `comm::send`. Runs inside `editor_system` before
  and after `process()`.
- `Mailbox`, `process()`, `MailboxClient`'s RAM-scan path and the magic are
  unchanged, so `emerald-editor` keeps working without modification.
- User systems: `install` grows a `systems: &'static [(&'static str,
  System)]` parameter and inserts an `EditorSystems { table, mask: u64 }`
  resource. One schedule slot `user_systems` runs after `editor_system` and
  calls each table entry whose mask bit is set. `SysMask` writes the mask.
  Entries past bit 63 are reported in `HelloAck` but cannot be enabled; the
  host greys them out.
- Constants: `MAX_ENTITIES`, `WORLD_BUF_BYTES`, `LAYER_BUF_BYTES` unchanged.

### `crates/cli`

- `emd editor-cart` scaffolds, beside `register_scene`, the stub
  `pub fn editor_systems() -> &'static [(&'static str, emerald_core::System)] { &[] }`
  with an `// emerald:editor-systems` marker line, and repairs older
  projects idempotently. `emd generate system` splices
  `("name", crate::systems::name::run),` (module-scoped path for module
  systems) at the marker, and `emd rm system` removes the line.
- The editor-cart template passes `<game>_core::editor_systems()` to
  `install`.
- `emd editor-cart --ggo` writes `<name>-editor.ggo`: the editor ELF plus
  the GGO2 asset section built by the same card writer `pack-ggo` uses.
  Its JSON trailer reports the path. The plain `.cart` output stays.

### `crates/world`

- World format: `[[background]] { layer = 0..3, map = "<stem>" }` replaces
  `[layers]`. Header version bumps to 4 with four fixed layer slots; a
  world blob with `[layers]` fails with `BadVersion` and `emd` errors name
  the file. `apply_layers` spawns one `Tilemap` per present slot on the
  matching `TileLayer`. The encoder merges an instanced sub-world's
  backgrounds using the same rule `ggo_worldlib::backgrounds::merge_backgrounds`
  applies (deepest-first, `background_priority` lets an instance override
  the host's slot), reimplemented with a test that pins the same cases.
- Instances: no change. Direct entities encode first, then instances
  depth-first in file order, which is what the index-keyed protocol relies
  on. The host derives entity index → instance chain from the document.

### New crate `crates/editor-link` (`emerald-editor-link`, std, no UI)

- `trait LinkIo { fn send(&mut self, payload: &[u8]) -> io::Result<()>; fn recv(&mut self) -> Vec<Vec<u8>>; }`
- `LinkMailbox<L: LinkIo>`: mirrors the parts of `Mailbox` a host reads
  (entity rows, counts, statuses, schema, system names), encodes every
  host→cart kind, chunks blobs with the stop-and-wait rule, and exposes the
  same read API `MailboxClient` has today (`entities()`, `schemas()`,
  `layer_status()`, `preview_status()`, `busy()`), so the world panel's
  Live mode and any future host share one implementation.
- Tests: an in-memory `LinkIo` pair wired to `link::pump` + `process()`
  (editor-runtime already compiles and tests on the host) proves the whole
  protocol without an emulator: hello handshake, world load in chunks with
  injected drops, transform round trip, entity diff publishing, layer load
  and cell pokes, system mask.

## ggo changes (phase 0)

- Rebase `worktree-uart-comm-link` onto `main`, resolving `docs/abi.md`,
  `firmware/ggo-hal/{perf,uart}.rs`, `firmware/ggo-wire/src/lib.rs`,
  `firmware/system/{Cargo.toml,src/cart.rs,src/main.rs}`,
  `sdk/gemdrop-sdk/src/lib.rs`, `tools/{Cargo.toml,Cargo.lock}`. Gates:
  `cargo test` in `firmware/`, `sdk/`, `tools/`; firmware `cargo build
  --release`; a user-run flash + `ggo-diag` full run for TX parity and a
  host-send → `comm-echo` cart round trip for RX.
- `ggo-emu-core` sandbox mode: `Peripherals` gains the OS-equivalent RX
  path by reusing `ggo_hal::comm::CommState` (injected bytes → ggo-wire
  decoder → APP queue of depth 4, drop counters mirroring `CommStatus`)
  and a separate TX byte buffer of encoded frames (`take_comm`), kept apart
  from the raw-text `log_sink` the sandbox console already reads.
  `runtime.rs` gains `COMM_SEND`, `COMM_RECV`, `COMM_STATUS` arms with the
  same pointer and length validation the `LOG` and `SAVE_READ` arms do.
  Host API: `Peripherals::uart_inject(&[u8])`, `Peripherals::take_comm()`.
  Test: sandbox echo cart round trip, mirroring the full-system echo test.
- `ggo-emu-core` depends on `ggo-wire` (already a `no_std` crate in
  `firmware/`) for the decoder; `ggo-comm`'s `MessageReader` stays the host
  side.

## ZedGG changes

### `ggo_emu_panel`

- `RunKind::Viewer { world_rel }`: runs `emd editor-cart --ggo` through the
  existing proc runner, then boots the reported `.ggo` through the ordinary
  run path. The world to show is loaded over the link by the world panel,
  not baked in, so switching worlds does not rebuild. Watch mode re-runs
  the editor-cart build when Rust sources or assets change, and the panel
  re-sends `Hello` after every boot.
- `Session` gains a link endpoint: `send_app(&[u8])` pushes wire bytes into
  an inject queue the emulator thread drains into `uart_inject` at each
  frame boundary; the thread runs a `MessageReader` over the existing uart
  drain and forwards `CHANNEL_APP` payloads into a bounded channel read by
  `recv_app()`. Other channels keep going to the uart console as today.
- `ggo_common` registry: `viewer_link(workspace, world_rel) -> Option<LinkHandle>`,
  the same shape as the existing `emulate_world` hook, so the world panel
  never depends on the emu panel crate.

### `ggo_world_panel`

- Toolbar switch `Design | Live`. `Live` boots (or reuses) the viewer run
  and drives a `LinkMailbox` over the `LinkHandle`. `Design` is the code
  that exists today, untouched.
- Live canvas: the emulator frame in the existing `WorldCanvasItem`, with
  the host drawing grid, selection outlines and drag ghosts from the cart's
  entity rows. Zoom stays host-side; pan sends `Camera`.
- Hit-test and drag: entity rows from the cart; index → document entity or
  instance chain computed from the open document with the encoder's order
  (direct entities, then instances depth-first). Drag sends `SetTransform`
  live and applies the existing `MoveEntity` / `MoveInstance` op on drop,
  so undo/redo, coalescing and snap are unchanged.
- Painting: `SetCell` live plus the existing `PaintSession` document write;
  backgrounds rail changes send a layer blob.
- Everything structural (add/remove entity, component or instance,
  inspector commits, paste, duplicate, undo/redo of those) re-encodes the
  document with `emerald_world::encode_toml_at` (std path dependency on
  `../emerald/crates/world`, matching the existing `ggo-*` path deps) and
  sends a world blob. Live never writes TOML itself; save is the existing
  save.
- Systems rail: checkboxes from `HelloAck`'s names, sending `SysMask`.
  Session state only (all off after every boot); the panel has no
  settings persistence yet and this design does not add one.
- Fallback: any of "no emerald project", editor-cart build failure, cart
  boot failure, no `HelloAck` within 5 s, protocol version mismatch, or a
  run ending shows the reason on the status row and drops to `Design`.
- `remote_read` (agent `world_read`) is unaffected: it reads the document.
- Tests: a fake `LinkIo` in gpui panel tests asserting the bytes sent for
  each gesture and the overlay drawn from injected entity rows; the Design
  test suite is unchanged.

## Hardware peer (phase 4)

- `ggo-uartd`'s pty relay becomes bidirectional (host writes on the pty
  reach the device), guarded so `ggo-diag` reads are unaffected.
- A `LinkIo` over `ggo_comm::GgoLink` on that pty; the world panel's
  "Flash" path can then target the board with the same `LinkMailbox`.
- Out of scope for the first four phases; listed so nothing in phases 0..3
  assumes an in-process peer.

## Phases

0. ggo: merge the comm branch; sandbox comm model.
1. emerald: `link.rs`, user systems, `editor_systems` scaffolding, `.ggo`
   viewer artifact, `[[background]]` format, `emerald-editor-link` crate.
2. zed: viewer run kind, `Session` link endpoint, registry hook.
3. zed: world panel `Live` mode with fallback.
4. hardware peer.

Each phase leaves every repo green on its own gates
(`./script/clippy -p <crate> && cargo test -p <crate> --lib` in zed;
`cargo clippy --workspace --all-targets -- -D warnings && cargo test
--workspace` in emerald; the three workspace test runs in ggo).

## Out of scope

- Acked delivery for single-datagram commands.
- Editing over the link from anything other than ZedGG (emerald-editor
  keeps its RAM mailbox).
- Runtime world switching by name; the host always sends the world blob.
- More than 64 toggleable systems.
- Deleting the `Design` renderer.
