# Cart-Owned Editing Design

Date: 2026-09-06
Status: Approved design, pre-implementation
Builds on `2026-09-04-live-world-view-design.md` (link protocol, viewer
cart) and `2026-09-06-live-world-view-v2-design.md` (headless viewer runs,
`WorldDock`, centered Live frame). Supersedes the host-side interaction
model of both: in Live, the cart is the source of truth for the world and
for every editing interaction.

## Problem

Live mode today keeps two half-authorities. The editor hit-tests and drags
against rects the cart published, mirrors the document into the cart with
`SetTransform`, and re-sends the whole world after each gesture. The
first real drag showed the seam: the box did not move. Every interaction
is implemented twice (Design and Live), a game whose systems move
entities fights the editor, and a game author cannot add editing
behaviour for their own components.

The user's direction:

- The cart is the source of truth while editing. `editor_system` on the
  cart owns all interactions.
- The editor pipes input through the link and updates component data in
  its sidebar from what the cart reports.
- Only editor systems run while the cart is in edit mode: a prebuilt
  `editor_schedule`. Play mode runs the game as it is.
- Game authors append their own edit systems to that schedule, so they
  can edit worlds that use their custom components. Pointer and command
  input must reach those systems as well as the built-in ones.
- Tilemap painting and every other edit also treat the cart as truth.

Decisions taken:

- Pointer and keyboard both move to the cart. Keyboard travels as
  semantic commands (Nudge, Delete, …), so Zed's keymap stays in Zed.
- The editor still paints the selection overlay, from cart-reported
  state. No PPU budget is spent on editor chrome.
- The editor's document mirrors the cart continuously; undo replays
  inverse commands to the cart.
- Save takes a snapshot from the cart. `#[derive(SceneComponent)]` gains
  a binary field writer so any component round-trips.
- Structural edits (add or remove an entity) are cart commands too; the
  editor keeps `[[instance]]` structure and tells the cart which
  flattened indices form a group.
- Two modes, Edit and Play. The per-system on/off mask and its rail go
  away.

Two sub-projects, one spec: **A. Interaction on the cart** (fixes
dragging, adds user edit systems) and **B. Cart as source of truth for
persistence**. A ships first.

## Architecture

```
Zed world tab (Live)                         viewer cart (editor-cart template)
  pointer / wheel / keymap                     editor_system (every frame, both modes)
    │ Pointer, Command, SetMode, SetTool,        link pump → mailbox → EditorInput
    │ Groups, SetComponent, Spawn, Despawn,      ┌─ Edit: edit_schedule
    │ Blob(World|Layer|Entity), Snapshot,        │    built-in: select, marquee, drag,
    │ ReadLayer, SetTransform (undo only)        │      nudge/delete/duplicate, camera pan
    ▼                                            │    user: edit_systems() table (all, every frame)
  LinkEndpoint ── uart ──►                       └─ Play: game_systems() table
    ▲                                            report: rows, Selection, Marquee, Gesture,
    │ Entities, Selection, Marquee, Gesture,             ComponentChanged, EntityAdded/Removed,
    │ ComponentChanged, EntityAdded/Removed,             Camera, FrameSeq, blobs (Snapshot, Layer)
    │ Camera, FrameSeq, HelloAck(tools),
    │ Blob(Snapshot|Layer|Entity)
  document mirror (ops, undo) → sidebar, save
```

The editor never decides what a click means. It converts canvas pixels to
device pixels, forwards them, and draws what the cart says.

## Cart runtime (emerald `editor-runtime`)

### Modes and schedule

`install(world, schedule, reg, game_systems, edit_systems)`. Both tables
are `&'static [(&'static str, System)]`.

- `EditorMode { Edit, Play }` resource, default Edit. `SetMode` switches
  it; a switch to Edit clears `Selection` and `EditorInput`.
- Schedule order every frame: `editor_system` (link pump, mailbox →
  `EditorInput`, `process` for commands) → `edit_schedule` gated on Edit
  (built-in edit systems in the order below, then every `edit_systems()`
  entry in table order) → `game_systems` gated on Play (every entry, in
  table order) → `editor_report` (publish). Gating uses
  `Schedule::add_if` with a requirement that reads `EditorMode`.
- `edit_systems()` replaces nothing: `editor_systems()` is renamed
  `game_systems()` by `emd editor-cart` (idempotent repair, marker
  `// emerald:game-systems`), and a second stub `edit_systems()` with
  marker `// emerald:edit-systems` is scaffolded beside it. `emd generate
  system --edit <name>` splices into the edit table; `emd rm system`
  removes from whichever table holds the line.

### Input resources

```rust
pub struct EditorInput {
    pub pointer: Vec2,          // world px, camera already applied
    pub device: (i16, i16),     // device px as received, for HUD-style tools
    pub buttons: u8, pub prev_buttons: u8,   // bit0 left, bit1 middle, bit2 right
    pub modifiers: u8,          // bit0 shift, bit1 ctrl/cmd, bit2 alt
    pub snap: bool,             // host's snap toggle
    pub consumed: bool,         // a user system took this frame's pointer
    pub commands: Vec<EditCommand>,   // this frame's semantic commands
}
impl EditorInput { pub fn just_pressed(&self, b: Button) -> bool; pub fn just_released(..); pub fn held(..); }

pub enum EditCommand { Nudge { dx: i32, dy: i32 }, Delete, Duplicate, SelectAll, ClearSelection, SetTool(u8) }

pub struct Selection { pub entities: Vec<Entity>, pub primary: Option<Entity>, pub marquee: Option<Rect> }
pub struct EditorTool(pub u8);   // 0 = Select (built-in); n = edit_systems()[n-1]
pub struct Groups(pub Vec<(u32, u32)>);   // (first flattened index, count), from the host
pub struct GestureState { pub id: u32, pub active: bool }
```

Pointer and commands are written by `LinkState::pump_inbound` into
non-committing mailbox fields (`pointer_*`, `pointer_seq`, a ring of 8
`EditCommand`s + count) so they never occupy the one-command-per-frame
slot; `editor_system` copies them into `EditorInput` each frame,
computes `pointer` from `device` and `effective_camera`, and clears
`consumed`. The RAM host (Iced) can drive the same fields.

### Built-in edit systems (run only when `EditorTool == 0`)

1. `edit_select`: on left press, hit-test the drawn rects (topmost by
   `Transform.z`, ties by spawn order) at `pointer`; hit → select (shift
   toggles); a hit inside a `Groups` range selects the whole group; miss
   → clear (unless shift) and start a marquee.
2. `edit_marquee`: while the marquee is active, `Selection.marquee` =
   press point → pointer; on release, select every entity whose drawn
   rect overlaps (shift adds).
3. `edit_drag`: a press on a selected entity starts a gesture (`Gesture
   Begin`); each frame moves every selected entity by the pointer delta
   since press, snapped to 16 px when `snap` and the primary's start
   position is on the grid; release ends the gesture (`Gesture End`).
4. `edit_commands`: `Nudge` moves the selection (one gesture per
   command); `Delete` despawns the selection and reports `EntityRemoved
   { index }` per entity (A; the host mirrors it as a `RemoveEntity` op
   and undo re-sends the entity as a world reload until B's `Spawn`
   exists); `Duplicate` spawns copies 16 px offset (B);
   `SelectAll`/`ClearSelection`; `SetTool`.
5. `edit_camera_pan`: middle drag moves the `Camera` resource by the
   pointer delta (in device px, so it does not fight its own movement).

Drawn rect per drawable: `MetaSprite::size()` + offset (exists);
`Sprite::size()` is added (handle: `size.px()` square; solid: `(w, h)`;
grid: max `dx + cell px`); `Text` and `Tilemap` keep the 16×16 box at the
transform in v1 and the spec records it as a limitation.

A user edit system runs every Edit frame after the built-ins. It reads
`EditorInput` and `Selection`, may set `consumed` to keep later systems
off this frame's pointer, and may mark entities dirty (B). A system that
wants to be a tool checks `EditorTool` against its own index.

### Reports

Every frame while a host is connected, after the schedule ran:
`Entities` rows as today (transform, size, draw offset), `Camera`,
`FrameSeq`; `Selection { count u8, indices u32… }` (≤ 60 per datagram,
several datagrams, a trailing `SelectionEnd` when it spans more than one)
and `Marquee { x0 y0 x1 y1 i32 }` when they changed; `Gesture { kind u8
(0 Begin, 1 End), id u32 }` on change; `EntityRemoved { index u32 }`. `HelloAck` gains the tool names
(`"Select"` first, then `edit_systems()` names) in place of the system
names.

## Persistence (B)

### Field writer

`SceneComponent::write_fields(&self, w: &mut FieldWriter)` mirrors
`write_json`; the derive emits it, the eight builtins implement it by
hand. `FieldWriter` produces the same bag layout the world blob uses
(`name_hash u64 | field_count u16 | section_len u32 | fields`, tags Int 1,
Bool 2, Str 3, List 4, Fixed 5, Struct 6). `FieldReader::from_bag(&[u8])
-> Option<(FieldReader, rest)>` becomes public so a single bag can be
decoded outside `load`. `SceneRegistry` gains `WriteFn` next to `BuildFn`
and `write_entity(world, entity, out)` / `read_bag(world, entity, hash,
bag)`.

### Commands

- `SetComponent { index u32, bag }` (bag ≤ 240 B inline, else a blob of
  kind `Entity` holding one bag). The cart rebuilds the component from
  the bag (`BuildFn`, i.e. replace-whole-component) and marks it dirty.
- `Spawn { bag… }` via blob kind `Entity` (one entity: `comp_count u16`
  then bags); the cart appends it to `tracked` with the next flattened
  index and reports `EntityAdded`.
- `Despawn { index }`; the cart despawns, drops the tracked entry, and
  reports `EntityRemoved { index }`. Indices above it do not shift: a
  slot stays empty until the next `LoadWorld`. The host keeps the same
  sparse mapping.
- `Snapshot`: the cart streams, as a cart→host blob of kind `Snapshot`,
  `entity_count u32` then per tracked slot `index u32 | comp_count u16 |
  bags` (empty slots omitted). `ReadLayer { layer }` streams the layer's
  source cells (`w u16 | h u16 | cells`), which the cart now keeps in a
  per-layer shadow (`[u16; 32*32]` × 4, updated by `load_layer` and
  `set_cell`).
- Cart→host blobs reuse the host→cart chunk protocol mirrored: cart
  sends `BlobBegin/Chunk/End`, host acks per chunk, cart keeps ≤ 4 in
  flight and retransmits on the same 100 ms / 20-retry rule.

### Dirty tracking

`editor::mark_dirty(world, entity)` pushes onto a `Dirty` resource; the
built-in systems call it; user systems must. `editor_report` serializes
each dirty entity's changed components (bag compared to the last
published bag, kept per tracked slot in a bounded cache: 256 slots × up
to 8 components × 64 B; larger bags are always resent) and publishes
`ComponentChanged { index, bag }` inline or as an `Entity` blob.

## Editor (Zed `ggo_world_panel`, `ggo_emu_panel`)

### Live becomes a terminal

- Mouse move/down/up over the Live canvas → `Pointer` with device px from
  the inverse of `live::geometry` (frame origin, scale) and modifiers;
  coalesced to the last event per tick; wheel keeps stepping the scale
  host-side. Keymap actions in Live (nudge, delete, duplicate, select
  all, clear) → `Command`. `Snap` toggle → `Pointer.snap`.
- Every host-side Live hit-test, marquee, drag, `pending_transforms`,
  `drag_origin`, `input_camera`/`pan_drag` code is deleted. Design mode
  keeps its own.
- After each world load: `Groups` from the document's instance spans.
- Rail: `Edit | Play` (sends `SetMode`; Play greys the inspector and
  ignores pointer) and a tool radio from `HelloAck` (`SetTool`). The
  systems checkbox rail is deleted.

### Mirror and undo

- Rows → `MoveEntity`/`MoveInstance` ops in the document, coalesced per
  cart gesture id (one undo entry per gesture); a row for a group member
  moves the instance by the primary member's delta. Selection from the
  cart replaces the document selection; the inspector shows the primary.
- `ComponentChanged` → a new `WorldOp::SetComponent { entity, comp, fields }`
  applied to the document; inspector field edit → the same op locally +
  `SetComponent` to the cart. `EntityAdded/Removed` → `AddEntity` /
  `RemoveEntity` ops with the cart's index.
- Undo/redo apply the inverse op to the document and send the matching
  command(s) to the cart (`SetTransform` for moves, `SetComponent`,
  `Spawn`/`Despawn`).
- Instance members' non-transform components are read-only in the
  inspector ("edit in `<stem>`").

### Save

Save requests `Snapshot` and `ReadLayer` for each loaded slot, waits for
the blobs (status row "Saving…"), rebuilds the document's direct entities
from the snapshot (instance members are validated against the instance's
own world and skipped), writes the world TOML and the `.map` files, and
clears dirty. A snapshot that fails or times out (5 s) keeps the file
untouched and shows the reason. Design mode saves the document as today.

### Painting

Paint strokes send `SetCell` as today; the paint session's document copy
stays for Design and for the map editor, and save reads the cart's cells
back so a user edit system that painted is kept.

### Loading and failure

Unchanged from v2 (loading screen, deadlines, fallback to Design). A
protocol version below 4 (B) or 3 (A) is refused with the rebuild
message.

## Testing — the UX must be proven before hand-off

- **Journey tests in Zed** (`ggo_world_panel`, new `tests/cart_journeys`
  module): an in-process `CartHarness` runs the real
  `emerald-editor-runtime` (`LinkState` + `process` + the edit schedule on
  a host `World`, as `editor-link/tests/protocol.rs` does) wired to a
  `LinkEndpoint`, so the panel's real mouse handlers drive the real cart.
  Required journeys, each asserting through the panel's public surface
  (overlay rects via `debug_bounds`, inspector text, document state):
  click selects and the outline appears on the sprite; drag moves the
  entity, the outline follows every frame, the inspector position
  updates, release leaves one undo entry, undo moves it back on the cart;
  shift-click toggles; marquee selects two; middle-drag pans and the
  outline stays on the sprite; nudge/delete/duplicate; group (instance)
  drag moves all members and undoes as one; Play mode ignores clicks and
  runs a game system; a user edit system receives the click and
  `consumed` keeps the built-in select off; inspector edit round-trips
  through `SetComponent`; paint a cell, save, reload from disk, the cell
  is there; save after a user system moved an entity writes the moved
  position; tab close stops the cart.
- **Emerald tests**: unit tests per built-in system over a `World`;
  `protocol.rs` journeys for every new message; bag round trip for every
  builtin and a derived component; snapshot/layer readback; blob
  cart→host with injected drops.
- **Real-artifact smoke** (`ggo_smoke`, runs when `emd` and a riscv
  toolchain are on `PATH`, otherwise skips with a message): builds the
  fixture project's editor cart with the real `emd editor-cart --ggo`,
  boots it in the emulator through `ViewerRun`, greets, loads a world,
  and drags an entity by sending `Pointer` datagrams, asserting rows move
  and no rebuild is triggered by the build's own outputs.
- Gates: `./script/clippy -p <crate> && cargo test -p <crate> --lib` per
  crate; emerald: clippy on the touched crates + `cargo test --workspace`.

## Phases

1. Emerald A: modes, `EditorInput`/`Selection`/`EditorTool`/`Groups`,
   built-in edit systems, `Sprite::size`, `edit_systems()` scaffold and
   `game_systems()` rename, Pointer/Command/SetMode/SetTool/Groups,
   Selection/Marquee/Gesture reports, HelloAck tools. Protocol v3.
2. Zed A: input forwarding, overlay from cart state, Edit/Play + tool
   rail, gesture-grouped mirror ops and undo, Groups, deletion of the
   host interaction code, journey tests.
3. Emerald B: `write_fields`, public bag decoding, SetComponent / Spawn /
   Despawn, dirty tracking and change reports, layer cell shadow,
   cart→host blobs, Snapshot / ReadLayer. Protocol v4.
4. Zed B: SetComponent ops and inspector round trip, structural mirror,
   snapshot save, paint readback, journey tests; real-artifact smoke.

Each phase: branch, subagent implementation, review, merge to its repo's
main, reinstall (`emd`, `zedgg`).

## Out of scope

- Text and Tilemap hit rects (16×16 fallback).
- Maps wider than the 32×32 hardware layer in the cell shadow.
- Cart-side undo history.
- Hardware peer over the uartd pty (unchanged plan: phase 4 of v1).
