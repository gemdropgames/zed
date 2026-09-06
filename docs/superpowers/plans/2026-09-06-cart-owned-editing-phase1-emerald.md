# Cart-Owned Editing — Phase 1 (emerald: interaction on the cart) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The viewer cart owns selection, marquee, drag, nudge, delete and camera pan; the host only forwards pointer/commands and reads back selection state. Game authors append their own edit systems.

**Architecture:** New link messages carry pointer/commands/mode/tool/groups into non-committing mailbox fields; `editor_system` turns them into an `EditorInput` resource; an Edit-gated set of built-in systems plus the user's `edit_systems()` table act on the `World`; `editor_report` publishes rows, `Selection`, `Marquee`, `Gesture`, `EntityRemoved`. Play gates the game's `game_systems()` table instead.

**Tech Stack:** Rust `no_std` (`emerald-editor-runtime`, `emerald-core`), std (`emerald-editor-link`, `emerald-cli`), cargo workspace `/home/clay/projects/emerald`.

**Spec:** `/home/clay/projects/zed/docs/superpowers/specs/2026-09-06-cart-owned-editing-design.md` — sections "Cart runtime", "Reports", "Testing", phase 1.

## Global Constraints

- Branch `cart-owned-editing` off emerald `main` (a07377f). Commit per task, no AI trailers.
- Gate before every commit: `cd /home/clay/projects/emerald && cargo fmt --all -- --check && cargo clippy -p emerald-editor-runtime -p emerald-editor-link -p emerald-core -p emerald-world -p emerald-cli --all-targets -- -D warnings && cargo test --workspace`. Workspace clippy is red on `main` for pre-existing lints in `crates/core`/`crates/world`; a run whose only diagnostics are pre-existing (verify once with `git stash`) passes. Do not fix unrelated lints.
- `LINK_PROTO_VERSION = 3`. Wire kinds (host→cart): `Pointer 0x0B`, `Command 0x0C`, `SetMode 0x0D`, `SetTool 0x0E`, `Groups 0x0F`. Cart→host: `Selection 0x8A`, `Marquee 0x8B`, `Gesture 0x8C`, `EntityRemoved 0x8D`. All integers little-endian. `Pointer { x i16, y i16, buttons u8, modifiers u8, snap u8 }` (device px, buttons bit0 left / bit1 middle / bit2 right, modifiers bit0 shift / bit1 ctrl / bit2 alt). `Command { kind u8, a i32, b i32 }` kinds: 0 Nudge(dx,dy) 1 Delete 2 Duplicate 3 SelectAll 4 ClearSelection 5 SetTool(a). `SetMode { mode u8 }` 0 Edit 1 Play. `SetTool { tool u8 }`. `Groups { count u8, (first u32, count u32)… }` ≤ 30 per datagram, `more u8` flag at the end (1 = another `Groups` datagram follows; the cart replaces its table when a datagram with `more == 0` closes a sequence). `Selection { more u8, count u8, indices u32… }` ≤ 60 per datagram, same `more` rule; `Marquee { active u8, x0 i32, y0 i32, x1 i32, y1 i32 }` world px integer; `Gesture { kind u8 (0 Begin, 1 End), id u32 }`; `EntityRemoved { index u32 }`.
- `HelloAck` `systems` field now carries the TOOL names: `"Select"` first, then every `edit_systems()` entry name, in order. The name count cap stays.
- `Mailbox` new fields are APPENDED after `entity_offsets` (layout test pins every prior offset): `pointer_x i16, pointer_y i16, pointer_buttons u8, pointer_modifiers u8, pointer_snap u8, _pad u8, pointer_seq u32, edit_cmd_count u8, _pad2 [u8;3], edit_cmds [EditCmdRaw { kind u8, _pad [u8;3], a i32, b i32 }; 8], mode u8, tool u8, _pad3 [u8;2], group_count u16, _pad4 [u8;2], groups [[u32; 2]; 64]`. Non-committing: `pump_inbound` writes them without touching `cmd_seq`.
- Mailbox size check in `tests/mailbox.rs` updated; every existing offset unchanged.
- Built-in edit systems act only when `EditorTool.0 == 0`; every user edit system runs every Edit frame.
- Comments explain why only; no `unwrap()` outside tests; full-word names.

---

### Task 1: Wire v3 — new kinds both ways

**Files:**
- Modify: `crates/editor-runtime/src/wire.rs` (kind table doc, consts, `HostMsg`, `CartMsg`, `encode_host`/`decode_host`, `encode_cart`/`decode_cart`, tests)

**Interfaces:**
- Produces:

```rust
pub const LINK_PROTO_VERSION: u8 = 3;
pub enum HostMsg<'a> { /* existing */,
    Pointer { x: i16, y: i16, buttons: u8, modifiers: u8, snap: bool },
    Command { kind: u8, a: i32, b: i32 },
    SetMode { mode: u8 },
    SetTool { tool: u8 },
    Groups { more: bool, groups: GroupRows<'a> },   // like EntityRows: slice or bytes; iterates (u32,u32)
}
pub const GROUPS_PER_MSG: usize = 30;   // 1 + 1 + 30*8 + 1 = 243
pub const SELECTION_PER_MSG: usize = 60; // 1 + 1 + 1 + 60*4 = 243
pub enum CartMsg<'a> { /* existing */,
    Selection { more: bool, indices: IndexRows<'a> },   // iterates u32
    Marquee { active: bool, x0: i32, y0: i32, x1: i32, y1: i32 },
    Gesture { kind: u8, id: u32 },
    EntityRemoved { index: u32 },
}
pub mod command { pub const NUDGE: u8 = 0; DELETE 1; DUPLICATE 2; SELECT_ALL 3; CLEAR_SELECTION 4; SET_TOOL 5; }
pub mod mode { pub const EDIT: u8 = 0; PLAY 1; }
```

`GroupRows`/`IndexRows` follow the existing `EntityRows` pattern (`from_slice` for encoding, borrowed bytes for decoding, `len()`, iterator).

- [ ] **Step 1: Tests first.** In `wire.rs` tests add round-trip cases to the host and cart tables: `Pointer { x: -3, y: 240, buttons: 0b101, modifiers: 0b011, snap: true }` (8 bytes), `Command { kind: command::NUDGE, a: -16, b: 0 }` (10), `SetMode { mode: mode::PLAY }` (2), `SetTool { tool: 2 }` (2), `Groups { more: true, groups: from_slice(&[(0,3),(3,1)]) }` (1+1+16+1 = 19 with `more` last), `Selection { more: false, indices: from_slice(&[0, 7, 255]) }` (1+1+1+12 = 15), `Marquee { active: true, x0: -1, y0: 2, x1: 300, y1: 400 }` (18), `Gesture { kind: 1, id: 9 }` (6), `EntityRemoved { index: 42 }` (5). Add golden-byte tests for `Pointer` and `Selection` (field order). Assert `encode_host` refuses `Groups` with > 30 entries and `encode_cart` refuses `Selection` with > 60 (returns `None`), and that a truncated `Groups` datagram decodes to `None`. Version test → 3.
- [ ] **Step 2: Run** `cargo test -p emerald-editor-runtime wire` → compile errors.
- [ ] **Step 3: Implement** per the constraints (put `more` as the LAST byte of `Groups` so the table can be scanned with a known count; for `Selection` put `more` FIRST as listed).
- [ ] **Step 4: Run** the crate tests; the `editor-link` crate will fail to compile until Task 6 adds arms — add temporary `_ => Ok(false)`-style arms there now so the workspace builds (they are replaced in Task 6).
- [ ] **Step 5: Commit** `editor-runtime: link v3 kinds for cart-owned editing`.

---

### Task 2: Mailbox input fields, non-committing inbound

**Files:**
- Modify: `crates/editor-runtime/src/mailbox.rs` (struct + `new`), `crates/editor-runtime/src/link.rs` (`pump_inbound` arms; `LinkState` gains `mode: u8`, `tool: u8`, `groups_pending: Vec<(u32,u32)>` for multi-datagram assembly), `crates/editor-runtime/tests/mailbox.rs`, `crates/editor-runtime/tests/link.rs`

**Interfaces:**
- Produces: the `Mailbox` fields listed in Global Constraints; `pub struct EditCmdRaw { pub kind: u8, pub _pad: [u8; 3], pub a: i32, pub b: i32 }` `#[repr(C)]`; `pub const EDIT_CMD_RING: usize = 8; pub const MAX_GROUPS: usize = 64;`.
- Behaviour: `Pointer` overwrites `pointer_*` and increments `pointer_seq`; `Command` appends to `edit_cmds` (drops when full, counted in a `dropped_commands` LinkState counter); `SetMode`/`SetTool` write `mode`/`tool`; `Groups` accumulates in `groups_pending` and copies into `mb.groups`/`group_count` when `more == false` (truncated to `MAX_GROUPS`). None of these commit (`cmd_seq` untouched), so `pump_inbound`'s drain loop keeps draining past them.

- [ ] **Step 1: Tests.** `tests/mailbox.rs`: new offsets pinned (`pointer_x` = old size 62708, and so on — compute by hand, keep 4-byte alignment; final `size_of`). `tests/link.rs`: a `Pointer` then a `SetTransform` in one drain → pointer fields set AND the transform committed in the same `pump_inbound` (pointer did not stop the loop); three `Command`s in one frame all land in the ring in order; a 9th command in one frame is dropped and `st.dropped_commands() == 1`; `Groups` split across two datagrams (`more` true then false) lands as one table; `SetMode`/`SetTool` land.
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Run** crate tests. **Step 5: Commit** `editor-runtime: pointer, commands, mode, tool, groups reach the mailbox without committing`.

---

### Task 3: Resources, modes, schedule split

**Files:**
- Create: `crates/editor-runtime/src/edit.rs` (resources + `EditCommand` + helpers)
- Modify: `crates/editor-runtime/src/sync.rs` (`install`, `editor_system`, new `editor_report`, `EditorRuntime`), `crates/editor-runtime/src/lib.rs` (`pub mod edit; pub use edit::*;`)
- Modify: `crates/cli/templates/editor-cart/src/main.rs.jinja` (call site, see Task 7 for the game-side stubs), `crates/editor/…` if it calls `install` (grep; fix the call)
- Test: `crates/editor-runtime/tests/sync.rs`, `tests/link.rs`

**Interfaces:**
- Produces in `edit.rs` (all `pub`, `impl Resource`):

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)] pub enum EditorMode { #[default] Edit, Play }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Button { Left, Middle, Right }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditCommand { Nudge { dx: i32, dy: i32 }, Delete, Duplicate, SelectAll, ClearSelection, SetTool(u8) }
#[derive(Default)] pub struct EditorInput { pub pointer: Vec2, pub device: (i16, i16), pub buttons: u8, pub prev_buttons: u8, pub modifiers: u8, pub snap: bool, pub consumed: bool, pub commands: Vec<EditCommand> }
impl EditorInput { pub fn held(&self, b: Button) -> bool; pub fn just_pressed(&self, b: Button) -> bool; pub fn just_released(&self, b: Button) -> bool; pub fn shift(&self) -> bool; }
#[derive(Default)] pub struct Selection { pub entities: Vec<Entity>, pub primary: Option<Entity>, pub marquee: Option<[i32; 4]> }
#[derive(Default)] pub struct EditorTool(pub u8);
#[derive(Default)] pub struct Groups(pub Vec<(u32, u32)>);
#[derive(Default)] pub struct GestureState { pub id: u32, pub active: bool }
/// Rect in world px an entity is drawn at: `(x, y, w, h)`.
pub fn drawn_rect(world: &World, entity: Entity) -> Option<(i32, i32, u16, u16)>;
```
- `install(world, schedule, reg, game_systems: SystemTable, edit_systems: SystemTable)`; inserts the resources above with defaults; schedule: `add(editor_system)`, `add_if(in_edit, edit_builtin)` (one system that runs the five built-ins in order — Task 4 fills it; here a no-op), for each `edit_systems` entry `add_if(in_edit, entry)`, for each `game_systems` entry `add_if(in_play, entry)`, then `add(editor_report)`. `in_edit`/`in_play` read `EditorMode`.
- `editor_system`: pump inbound, `process` (commands), then build `EditorInput` from the mailbox: `device = (pointer_x, pointer_y)`, `pointer = device + effective_camera(world)` (world px as `Vec2` from ints), `prev_buttons = last frame's buttons`, `buttons = pointer_buttons`, `modifiers`, `snap`, `consumed = false`, `commands` = drain the ring into `EditCommand`s (unknown kinds dropped); apply `mode` (a change to Edit clears `Selection` and `EditorInput.buttons/prev_buttons`), `tool`, `groups` → resources. `EditorTool` also changes on `EditCommand::SetTool`.
- `editor_report`: what `pump_outbound` did, moved to the END of the frame (rows reflect the systems that ran this frame). `EditorRuntime` keeps `link` boxed; both systems `mem::take` it as today.
- `EditorSystems`/`user_systems`/`sys_mask` are deleted; `SysMask` handling in `link.rs` is removed from the decoder's arm list (kind 0x07 becomes unknown → dropped).

- [ ] **Step 1: Tests.** `tests/sync.rs`: `install` with two one-entry tables and a `Schedule`; run one frame in Edit → the edit entry ran (flag in a resource), the game entry did not; `SetMode Play` via the mailbox → next frame the reverse; switching back to Edit clears `Selection`. `EditorInput` from a mailbox `Pointer (10, 20)` with `Camera` resource at (100, 50) → `pointer == (110, 70)`, `just_pressed(Left)` true on the first frame the bit is set and false on the next; a ring of two commands drains in order and is empty next frame. `tests/link.rs`: existing `pump_outbound` tests keep passing through `editor_report`'s helper (keep `LinkState::pump_outbound` as the callable; `editor_report` is the system wrapper).
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** Fix the Iced host / template call sites so the workspace builds (template: `install(world, schedule, reg, {{ name_snake }}_core::game_systems(), {{ name_snake }}_core::edit_systems())`). **Step 4: Run** workspace tests. **Step 5: Commit** `editor-runtime: edit/play modes, EditorInput, edit and game system tables`.

---

### Task 4: Built-in edit systems

**Files:**
- Create: `crates/editor-runtime/src/edit_systems.rs` (`edit_builtin` + the five systems + hit-testing)
- Modify: `crates/core/src/gfx/sprite.rs` (`pub fn size(&self) -> (u16, u16)`), `crates/editor-runtime/src/edit.rs` (`drawn_rect` uses it), `crates/editor-runtime/src/sync.rs` (`EditorState` gains `drag: Option<DragState>`, `gesture_seq: u32`, `removed: Vec<u32>` for reporting)
- Test: unit tests inside `edit_systems.rs` over a `World` with the resources inserted; `crates/core` test for `Sprite::size` in each mode.

**Interfaces:**
- `pub fn edit_builtin(world: &mut World)` runs, when `EditorTool.0 == 0` and `!EditorInput.consumed`: `select`, `marquee`, `drag`, `commands`, `camera_pan` in that order (commands always run even for other tools — `SetTool`, `SelectAll`, `ClearSelection` are tool-independent; `Nudge`/`Delete` act on the current selection whatever the tool).
- Hit-test: `pub fn hit_test(world: &mut World, x: i32, y: i32) -> Option<Entity>`: iterate `query_entities::<&Transform>()`, keep those whose `drawn_rect` contains the point, pick max `z`, ties → last spawned (`Entity::index()` larger). `drawn_rect`: `MetaSprite` (size + offset), else `Sprite` (`Sprite::size()` + `offset()`), else `(pos, 16, 16)`.
- Select: left just-pressed → `hit_test(pointer)`; hit: if shift toggle membership else `{hit}`; `primary = hit`; if a `Groups` range contains the hit's tracked index, replace with all group members (resolve `tracked` index ↔ `Entity` through `EditorState.tracked`). Miss: unless shift clear; start marquee anchored at pointer (stored in `EditorState`).
- Marquee: while left held and marquee anchored: `Selection.marquee = Some([ax, ay, px, py])`; on release: entities whose drawn rect overlaps the normalized rect (shift adds), `primary` = last; marquee `None`.
- Drag: left just-pressed on an already-selected entity (or the entity just selected this frame) → `DragState { start_pointer, starts: Vec<(Entity, Vec2)> }`, `GestureState { id += 1, active: true }`, `Gesture Begin` queued; each frame while held: `pos = start + (pointer − start_pointer)`, snapped to 16 when `snap` (apply the snap to the primary and move the rest by the primary's delta, as Zed does); release → `active = false`, `Gesture End` queued. Rows publish the moves automatically.
- Commands: `Nudge` → one gesture (Begin, move, End); `Delete` → for each selected entity `world.despawn`, remove from `tracked`, push its index onto `EditorState.removed`, clear selection; `Duplicate` → no-op in A (reserved, log nothing); `SelectAll` → every tracked entity; `ClearSelection`; `SetTool(n)` → `EditorTool(n)` (clamped to the tool count).
- Camera pan: middle held → `Camera.offset -= (device − prev_device)` in world px (pan uses device deltas so moving the camera does not move the pointer's world position under the cursor).

- [ ] **Step 1: Tests** (one `World`, `SceneRegistry::with_builtins`, spawn two `Transform` entities with a `MetaSprite` is not constructible headless — use the 16×16 fallback, plus one entity with a `Sprite::solid(24, 8)` to cover `Sprite::size`): click inside selects it, click on empty clears, shift-click adds, marquee over both selects both, a press on the selected entity + move 30 px moves both selected entities by 30 (unsnapped) and by 32 with `snap`, `Gesture` ids increment per drag, `Nudge{-16,0}` moves and yields a gesture, `Delete` despawns and records the index, group select expands to the range, middle-drag pans the camera by the device delta, a user tool (`EditorTool(1)`) makes clicks inert for the built-ins, `consumed` makes them inert. `Sprite::size`: handle S32 → (32,32); solid (24,8) → (24,8); grid built via `new_grid` 5×3 tiles → (80,48).
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Run** crate tests. **Step 5: Commit** `editor-runtime: built-in edit systems (select, marquee, drag, commands, camera pan)`.

---

### Task 5: Reports — Selection, Marquee, Gesture, EntityRemoved, tools in HelloAck

**Files:**
- Modify: `crates/editor-runtime/src/link.rs` (`pump_outbound`, `LinkState` mirrors: `sent_selection: Vec<u32>`, `sent_marquee`, `gesture_events: Vec<(u8,u32)>`), `crates/editor-runtime/src/sync.rs` (`editor_report` hands the resources to the link), `crates/editor-runtime/tests/link.rs`

**Interfaces:**
- `pump_outbound(&mut self, mb: &Mailbox, edit: &EditReport, link)` where `pub struct EditReport<'a> { pub selection: &'a [u32] /* tracked indices, sorted */, pub marquee: Option<[i32;4]>, pub gestures: &'a [(u8, u32)], pub removed: &'a [u32] }` — `editor_report` builds it from `Selection` (entities → tracked indices), `Selection.marquee`, and drains `EditorState.removed` / gesture events.
- Selection is published when it differs from `sent_selection` (all datagrams of the split); `Marquee { active }` when it changes; every `Gesture` and `EntityRemoved` is published once (fire-and-forget like `FrameSeq`; a lost `Gesture End` is healed by the next `Begin` on the host, which closes the open gesture).
- `HelloAck.systems` = `["Select"] ++ edit_systems names` (the `SystemTable` handed to `LinkState::new` is now the EDIT table; game names are not sent).

- [ ] **Step 1: Tests** (`tests/link.rs`): after hello, a frame with selection `[2, 0]` publishes `Selection { more: false, [0, 2] }` (sorted) once and not again on an unchanged frame; a 70-entry selection publishes two datagrams (`more` true then false); `Marquee` active then inactive; `Gesture Begin/End` each once; `EntityRemoved` once; `HelloAck` names `["Select", "paint_props"]` for an edit table with one entry.
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Run** crate tests. **Step 5: Commit** `editor-runtime: report selection, marquee, gestures, removals; tools in HelloAck`.

---

### Task 6: Host mirror (`emerald-editor-link`)

**Files:**
- Modify: `crates/editor-link/src/lib.rs`, `crates/editor-link/tests/protocol.rs`

**Interfaces:**
- Senders: `pointer(x: i16, y: i16, buttons: u8, modifiers: u8, snap: bool)`, `command(EditCommand)` (re-export the enum from the runtime), `set_mode(EditorMode)`, `set_tool(u8)`, `groups(&[(u32, u32)])` (splits into ≤ 30 per datagram with `more`). All fire-and-forget `Result<(), Error>`.
- Mirrors: `selection() -> &[u32]` (assembled across `more` datagrams; partial sequences do not replace the mirror until closed), `marquee() -> Option<[i32; 4]>`, `take_gestures() -> Vec<(GestureKind, u32)>`, `take_removed() -> Vec<u32>` (drained by the host), `tool_names()` (the renamed `system_names`). `SysMask`/`set_sys_mask` deleted. Reset all on a greeting.
- The `Cart` fixture in `tests/protocol.rs` runs the REAL edit schedule: build a `World` + `Schedule` via `install(.., game_systems: &[("spin", spin)], edit_systems: &[("mark", mark_consumed)])`, and `cart.frame()` runs `schedule.run(&mut world)`; the `Camera` resource and entities come from a loaded world blob (`world_blob(n)` helper exists).

- [ ] **Step 1: Tests** (journeys, each through `host.*` senders and `cart.frame()` + `host.poll`): (a) click at an entity's position selects it — `host.selection() == [i]`; (b) press, move 30 px, release → rows for the selected entity moved by 30 and `take_gestures()` yields Begin then End with one id; (c) marquee over two entities → selection has both; (d) `Nudge{16,0}` moves the selection by 16 with its own gesture; (e) `Delete` → `take_removed() == [i]`, rows no longer include it, `EntityCount` shrank; (f) `groups(&[(0,2)])` then click entity 1 → selection `[0,1]`, drag moves both; (g) `set_mode(Play)` → clicks change nothing, the `spin` game system ran (observable via a row it moves); back to Edit clears selection; (h) with `set_tool(1)`, a click sets a flag the `mark_consumed` edit system observes and the built-in select does nothing; with tool 0 and `mark_consumed` setting `consumed`, the built-in select still does nothing; (i) `HelloAck` tool names `["Select", "mark"]`; (j) a 70-entity selection round-trips across two datagrams.
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Run** workspace tests. **Step 5: Commit** `editor-link: forward pointer and commands, mirror selection and gestures`.

---

### Task 7: CLI scaffolding — `game_systems()` rename, `edit_systems()` stub, `--edit`

**Files:**
- Modify: `crates/cli/src/commands/editor_cart.rs` (markers, `ensure_*`), `crates/cli/src/commands/generate.rs` (`--edit` flag for `generate system`, `register_in_editor` chooses the table), `crates/cli/src/commands/rm.rs` (strip from either table), `crates/cli/templates/editor-cart/src/main.rs.jinja` (both tables), CLI tests beside each.

**Interfaces:**
- `EDITOR_SYSTEMS_MARKER` → `GAME_SYSTEMS_MARKER = "// emerald:game-systems"` on `pub fn game_systems()`; a legacy `editor_systems()` + old marker is renamed in place by `ensure_game_systems` (idempotent, tested against a lib.rs carrying the old stub with entries). New `EDIT_SYSTEMS_MARKER = "// emerald:edit-systems"` on `pub fn edit_systems() -> &'static [(&'static str, emerald_core::System)] { &[ // emerald:edit-systems ] }` appended once.
- `emd generate system <name> --edit` splices into the edit table (label/path rules unchanged); without `--edit` into the game table. `emd rm system` strips the line from whichever table has it (try both).
- `emd editor-cart` JSON trailer unchanged.

- [ ] **Step 1: Tests**: rename of a legacy stub with two entries keeps the entries under the new name/marker; a second run changes nothing; `edit_systems` stub appended once; `generate system foo --edit` lands in the edit table; `rm system foo` removes it; the template renders the two-table `install` call.
- [ ] **Step 2: Run** → fail. **Step 3: Implement.** **Step 4: Run** workspace tests. **Step 5: Commit** `cli: game_systems/edit_systems tables, generate system --edit`.

---

### Task 8: Gate, review, merge, reinstall

- [ ] **Step 1:** Full gate; `cargo test --workspace` green; clippy on the five crates clean but for pre-existing `crates/core`/`crates/world` lints.
- [ ] **Step 2:** Zed still compiles against this branch? It will NOT (Zed uses `set_sys_mask`, `system_names`, and the runtime's `install` signature only through `editor-link`) — that is Phase 2's job; note the breakage in the report and do not "fix" Zed here.
- [ ] **Step 3:** Fresh opus review of `git diff main...cart-owned-editing` for practices and goal fit (spec sections "Cart runtime", "Reports"). Fix findings; re-gate.
- [ ] **Step 4:** Do NOT merge to emerald main yet: Zed on `ggo` builds against emerald `main` by path, and merging a v3 protocol before Phase 2 lands would break the installed editor. Leave the branch checked out; Phase 2's plan merges both repos together.
