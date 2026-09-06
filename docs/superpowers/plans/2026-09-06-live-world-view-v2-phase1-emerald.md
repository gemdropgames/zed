# Live World View v2 — Phase 1 (emerald protocol) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The viewer cart reports its camera every frame and publishes each entity's real drawn footprint, so a host can place overlays on the emulator picture without guessing.

**Architecture:** One new cart→host wire kind (`Camera`, `0x89`) emitted beside `FrameSeq`; two new `Mailbox` fields the sync system fills from the `Camera` resource; entity rows sized from `MetaSprite::size()` plus its draw offset; the host `LinkMailbox` mirrors the camera. Protocol version 1 → 2.

**Tech Stack:** Rust, `no_std` cart runtime (`emerald-editor-runtime`), std host crate (`emerald-editor-link`), cargo workspace at `/home/clay/projects/emerald`.

**Spec:** `/home/clay/projects/zed/docs/superpowers/specs/2026-09-06-live-world-view-v2-design.md` ("Protocol change (emerald)").

## Global Constraints

- Work on branch `live-world-view-v2` off emerald `main` (`a0b687f`). Commit per task. No AI co-author trailers.
- Gate before every commit: `cd /home/clay/projects/emerald && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Kind byte for `Camera` is `0x89`; payload is `x i32, y i32` little-endian Q16.16 (9 bytes with the kind).
- `LINK_PROTO_VERSION` becomes `2`.
- Rows without a `MetaSprite` keep `16x16` at the transform.
- Comments explain "why" only. No `unwrap()` outside tests.

---

### Task 1: `CartMsg::Camera` on the wire

**Files:**
- Modify: `crates/editor-runtime/src/wire.rs` (kind table doc ~line 34-43, constants ~line 60-78, `CartMsg` enum ~line 158, `encode_cart` ~line 586, `decode_cart` ~line 650, tests ~line 960-1000)

**Interfaces:**
- Produces: `CartMsg::Camera { x: i32, y: i32 }`; `wire::LINK_PROTO_VERSION == 2`; `KIND_CAMERA_REPORT: u8 = 0x89`.

- [ ] **Step 1: Add the round-trip test case**

In the `cart_cases()` table in `wire.rs` tests (the list ending with `(cart_rt(CartMsg::FrameSeq { seq: u32::MAX }), 5, false)`), append:

```rust
            (cart_rt(CartMsg::Camera { x: -(3 << 16), y: 240 << 16 }), 9, false),
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p emerald-editor-runtime wire::tests`
Expected: compile error, `no variant named Camera`.

- [ ] **Step 3: Implement the kind**

Constants block:

```rust
const KIND_FRAME_SEQ: u8 = 0x88;
const KIND_CAMERA_REPORT: u8 = 0x89;
```

Doc table row under `0x88`:

```
//! | 0x89 | `Camera`        | `x i32, y i32` — the cart's camera offset, Q16.16, every frame     |
```

Enum, after `FrameSeq`:

```rust
    /// Where the cart's camera is (Q16.16), published every frame so a
    /// host can place overlays on the picture the cart drew.
    Camera { x: i32, y: i32 },
```

`encode_cart`, after the `FrameSeq` arm:

```rust
        CartMsg::Camera { x, y } => {
            writer.u8(KIND_CAMERA_REPORT)?;
            writer.i32(x)?;
            writer.i32(y)?;
        }
```

`decode_cart`, before `_ => return None`:

```rust
        KIND_CAMERA_REPORT => CartMsg::Camera {
            x: reader.i32()?,
            y: reader.i32()?,
        },
```

Version:

```rust
pub const LINK_PROTO_VERSION: u8 = 2;
```

If `CartMsg` derives `PartialEq`/`Debug` only, no further change; if any `match` over `CartMsg` elsewhere in the crate is exhaustive (grep `CartMsg::FrameSeq`), add the arm there in the following tasks.

- [ ] **Step 4: Run tests**

Run: `cargo test -p emerald-editor-runtime`
Expected: PASS (the `unknown_kind_bytes_decode_to_none` test must still use a kind that is unknown; if it used `0x89`, change it to `0xF0`).

- [ ] **Step 5: Commit**

```bash
git add crates/editor-runtime/src/wire.rs
git commit -m "editor-runtime: Camera report kind, link proto v2"
```

---

### Task 2: Mailbox carries the camera; rows carry the drawn footprint

**Files:**
- Modify: `crates/editor-runtime/src/mailbox.rs` (`Mailbox` struct ~line 130-160, `Mailbox::new` ~line 190-205)
- Modify: `crates/editor-runtime/src/sync.rs` (republish block ~line 130-152)
- Test: `crates/editor-runtime/tests/sync.rs`

**Interfaces:**
- Produces: `Mailbox { pub camera_x: i32, pub camera_y: i32, .. }`; `pub fn entity_rect(pos: (i32, i32), sprite: Option<((u16, u16), (i16, i16))>) -> (i32, i32, u16, u16)` in `sync.rs` (public so the test crate reaches it).

- [ ] **Step 1: Write the failing tests**

Append to `crates/editor-runtime/tests/sync.rs`:

```rust
#[test]
fn entity_rect_uses_the_sprite_footprint_and_offset() {
    use emerald_editor_runtime::entity_rect;
    // No sprite: 16x16 at the transform.
    assert_eq!(entity_rect((5 << 16, 7 << 16), None), (5 << 16, 7 << 16, 16, 16));
    // A 32x16 sprite drawn 4px left / 2px up of its transform.
    assert_eq!(
        entity_rect((5 << 16, 7 << 16), Some(((32, 16), (-4, -2)))),
        ((5 - 4) << 16, (7 - 2) << 16, 32, 16)
    );
}

#[test]
fn the_camera_resource_lands_in_the_mailbox_each_frame() {
    let mut mb = Box::new(Mailbox::new());
    let mut world = World::new();
    let mut st = EditorState::default();
    mb.cmd_kind = CMD_CAMERA;
    mb.cmd_x = Fixed::from_int(9).0;
    mb.cmd_y = Fixed::from_int(-4).0;
    mb.cmd_seq = 1;
    process(&mut mb, &mut world, &registry(), &mut st);
    assert_eq!(mb.cmd_ack, 1);
    assert_eq!(
        (mb.camera_x, mb.camera_y),
        (Fixed::from_int(9).0, Fixed::from_int(-4).0),
        "the republish copies the Camera resource into the mailbox"
    );
}
```

(Same shape as `camera_command_inserts_resource_when_absent` at ~line 129: `registry()` and the imports already exist in this file.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p emerald-editor-runtime --test sync`
Expected: compile errors (`entity_rect` and `camera_x` missing).

- [ ] **Step 3: Implement**

`mailbox.rs`, in `pub struct Mailbox`, directly after `pub frame_seq: u32,`:

```rust
    /// The cart's camera offset (Q16.16) as of the last republish, for the
    /// link to report. Appended after `frame_seq` so every field the RAM
    /// host reads by `offset_of!` keeps its offset.
    pub camera_x: i32,
    pub camera_y: i32,
```

and in `Mailbox::new()` after `frame_seq: 0,`:

```rust
            camera_x: 0,
            camera_y: 0,
```

`sync.rs`: add the pure helper (near the top of the file, after the imports):

```rust
/// The rect a host should outline for an entity: the sprite's drawn
/// footprint when it has one (its size and draw offset, which already
/// folds `.centered()` in), else a 16x16 box at the transform.
/// `pos` is Q16.16; the offset is whole pixels.
pub fn entity_rect(
    pos: (i32, i32),
    sprite: Option<((u16, u16), (i16, i16))>,
) -> (i32, i32, u16, u16) {
    match sprite {
        Some(((w, h), (ox, oy))) => (
            pos.0.wrapping_add((ox as i32) << 16),
            pos.1.wrapping_add((oy as i32) << 16),
            w,
            h,
        ),
        None => (pos.0, pos.1, 16, 16),
    }
}
```

`lib.rs` already has `pub use sync::*;`, so nothing to export.

Replace the republish loop body (the `// ponytail: 16x16 default footprint` block) with:

```rust
        let sprite = world
            .get::<MetaSprite>(e)
            .map(|ms| (ms.size(), ms.offset));
        let (x, y, w, h) = entity_rect((t.pos.x.0, t.pos.y.0), sprite);
        mb.entities[n] = EntityRect { index: idx, x, y, w, h };
        n += 1;
```

After `mb.frame_seq = mb.frame_seq.wrapping_add(1);` add:

```rust
    if let Some(camera) = world.try_resource::<Camera>() {
        mb.camera_x = camera.offset.x.0;
        mb.camera_y = camera.offset.y.0;
    }
```

If `World` has no `try_resource` (only `try_resource_mut`), use `try_resource_mut` and read through it; do not add a new World API.

- [ ] **Step 4: Run tests**

Run: `cargo test -p emerald-editor-runtime`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/editor-runtime/src/mailbox.rs crates/editor-runtime/src/sync.rs crates/editor-runtime/tests/sync.rs
git commit -m "editor-runtime: publish camera and sprite footprints in the mailbox"
```

---

### Task 3: The cart emits `Camera` every frame

**Files:**
- Modify: `crates/editor-runtime/src/link.rs` (`pump_outbound`, ~line 349-410)
- Test: `crates/editor-runtime/tests/link.rs`

**Interfaces:**
- Consumes: `Mailbox::camera_x/camera_y`, `CartMsg::Camera`.
- Produces: every connected frame's outbound ends `FrameSeq, Camera`.

- [ ] **Step 1: Update the expectations**

In `tests/link.rs`, every assertion that lists outbound kinds gains `0x89` at the end. Known sites: line ~130 (`[0x81, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88]` → append `0x89`, message "…, FrameSeq, Camera"), line ~624 (`"unchanged table: only FrameSeq"` → the list becomes `[0x88, 0x89]`, message "only FrameSeq and Camera"), line ~949 (same). Also add one new test:

```rust
#[test]
fn the_camera_is_reported_each_frame_from_the_mailbox() {
    let mut mb = Mailbox::new();
    let mut st = LinkState::new(SYSTEMS);
    let mut link = VecLink::default();
    link.hello();
    st.pump_inbound(&mut mb, &mut link);
    st.pump_outbound(&mb, &mut link);
    link.drain_cart();

    mb.camera_x = 3 << 16;
    mb.camera_y = 4 << 16;
    st.pump_outbound(&mb, &mut link);
    let out = link.drain_cart();
    let camera = out
        .iter()
        .find_map(|d| match wire::decode_cart(d) {
            Some(CartMsg::Camera { x, y }) => Some((x, y)),
            _ => None,
        });
    assert_eq!(camera, Some((3 << 16, 4 << 16)));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p emerald-editor-runtime --test link`
Expected: the kind-list assertions and the new test FAIL.

- [ ] **Step 3: Implement**

At the end of `pump_outbound`, after `emit(link, &mut out, &CartMsg::FrameSeq { seq: mb.frame_seq });`:

```rust
        // Unconditional, like `FrameSeq`: a dropped report is replaced a
        // frame later, so there is nothing to diff or retry.
        emit(
            link,
            &mut out,
            &CartMsg::Camera {
                x: mb.camera_x,
                y: mb.camera_y,
            },
        );
```

- [ ] **Step 4: Run the whole crate**

Run: `cargo test -p emerald-editor-runtime`
Expected: PASS. Fix any remaining kind-list assertion the grep `grep -n "0x88" crates/editor-runtime/tests/link.rs` still shows without a `0x89` beside it.

- [ ] **Step 5: Commit**

```bash
git add crates/editor-runtime/src/link.rs crates/editor-runtime/tests/link.rs
git commit -m "editor-runtime: report the camera beside FrameSeq"
```

---

### Task 4: Host mailbox mirrors the camera

**Files:**
- Modify: `crates/editor-link/src/lib.rs` (struct fields ~line 240-275, `new` ~line 276, `HelloAck` arm ~line 567, `FrameSeq` arm ~line 651, accessors ~line 385-410)
- Test: `crates/editor-link/tests/protocol.rs` (`camera_and_preview_commands_reach_the_cart`, ~line 417)

**Interfaces:**
- Produces: `LinkMailbox::camera(&self) -> Option<(i32, i32)>` (raw Q16.16; `None` until the first report of the current session).

- [ ] **Step 1: Extend the test**

In `camera_and_preview_commands_reach_the_cart`, after the first `cart.frame();` and its assert, add:

```rust
    host.poll(Instant::now()).expect("poll succeeds");
    assert_eq!(
        host.camera(),
        Some((7 << 16, 8 << 16)),
        "the cart reports the camera the host set"
    );
```

And in `a_version_mismatch_is_reported_not_connected` confirm it uses `wire::LINK_PROTO_VERSION` (not a literal `1`); if a literal, replace it with the constant.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p emerald-editor-link --test protocol camera_and_preview`
Expected: FAIL, `no method named camera`.

- [ ] **Step 3: Implement**

Field, next to `frame_seq: u32,`:

```rust
    /// The cart's camera (Q16.16), from its per-frame report. `None`
    /// until the first report after a greeting.
    camera: Option<(i32, i32)>,
```

Initialise `camera: None` in `new`. In the `HelloAck` arm, where the other per-session state is reset (`self.entities.clear()` or similar), add `self.camera = None;`. New arm after `FrameSeq`:

```rust
            CartMsg::Camera { x, y } => {
                let changed = self.camera != Some((x, y));
                self.camera = Some((x, y));
                Ok(changed)
            }
```

Accessor next to `frame_seq()`:

```rust
    pub fn camera(&self) -> Option<(i32, i32)> {
        self.camera
    }
```

- [ ] **Step 4: Run the workspace gate**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS. (`crates/editor`'s Iced host reads the RAM mailbox and is untouched.)

- [ ] **Step 5: Commit**

```bash
git add crates/editor-link/src/lib.rs crates/editor-link/tests/protocol.rs
git commit -m "editor-link: mirror the cart's camera report"
```

---

### Task 5: Cross-repo check and merge

**Files:** none new.

- [ ] **Step 1: Build the zed consumer against this branch**

Run: `cd /home/clay/projects/zed && ./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`
Expected: PASS. Zed's `live.rs` uses `LinkMailbox` only, so the new variant needs no match arm there; the version constant flows through `emerald_editor_link`. A zed test that pins `LINK_PROTO_VERSION == 1` (grep `predates the link protocol` in `ggo_world_panel.rs` tests) must use the constant, not a literal.

- [ ] **Step 2: Review, then merge**

Dispatch a fresh opus reviewer over `git diff main...live-world-view-v2` in emerald for (1) practices, (2) whether the cart now reports camera + footprints as the spec says. Fix findings, re-run the gate, then:

```bash
cd /home/clay/projects/emerald && git checkout main && git merge --ff-only live-world-view-v2 && git push origin main
```
