# .spr v5 Affine Frames Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-frame affine transforms (rotation, scale, shear) stored in `.spr` v5, editable beside ms in the sprite panel, applied at play time by the emerald runtime.

**Architecture:** One shared `FrameTransform` type + fixed-point matrix composer in `ggo-asset-formats` (the codec both emerald and worldlib already share). Worldlib adds a doc op and a transformed preview composer; the zed sprite panel adds per-entry editors; emerald adds a host-safe `spr_affine` wrapper, a 32-set dedup allocator, and the OAM affine bits.

**Tech Stack:** Rust; integer-only fixed point (8.8 matrices, 256-step angle); gpui for the editor.

**Spec:** `docs/superpowers/specs/2026-08-22-spr-affine-frames-design.md` (this repo). Read it first; it carries the hardware ground truth and file:line references.

## Global Constraints

- NO floating point in ggo/emerald code paths (`clippy.toml` denies it in emerald; ggo ABI doc: integer-only). All math in i32/i16 fixed point.
- Identity transform = `(angle256: 0, sx: 0x0100, sy: 0x0100, shear_x: 0, shear_y: 0)`; identity frames MUST render via the legacy non-affine path.
- `SPR_VERSION` becomes 5; decoder allowlist {3, 4, 5}; v3/v4 decode with identity transforms.
- Do NOT commit anywhere — three repos are touched (`/home/clay/projects/ggo`, `/home/clay/projects/emerald`, this zed fork); clay commits. Leave all trees dirty and report.
- Per-repo gates after each task: that repo's crate tests green; zed tasks also `./script/clippy -p ggo_sprite_panel`; emerald tasks `cargo clippy` per its workspace config (float-arith deny must stay green).
- Task order 1→7 is dependency order; 5/6 (emerald) and 7 (zed) both depend on 3, not on each other.

---

### Task 1: `FrameTransform` + `.spr` v5 codec (repo: ggo)

**Files:**
- Modify: `/home/clay/projects/ggo/sdk/ggo-asset-formats/src/spr.rs`
- Modify: `/home/clay/projects/ggo/sdk/ggo-asset-formats/src/reader.rs` (add `i16` read helper if absent)
- Test: same files' `#[cfg(test)]` mods

**Interfaces:**
- Consumes: existing `Reader` (`u8/u16/u32`, `done`), `SPR_MAGIC`, `push_tag`.
- Produces (later tasks rely on these exact names):
  - `pub const SPR_VERSION: u8 = 5;` `pub const SPR_VERSION_V4: u8 = 4;` (keep `SPR_VERSION_V3`)
  - `pub struct FrameTransform { pub angle256: u8, pub sx: i16, pub sy: i16, pub shear_x: i16, pub shear_y: i16 }` with `pub const IDENTITY: FrameTransform` and `pub fn is_identity(&self) -> bool`
  - `pub struct SprFrame { pub map: Vec<u16>, pub duration_ms: u16, pub transform: FrameTransform }`
  - `SprData.frames: Vec<SprFrame>` (replaces the tuple)

- [ ] **Step 1: Write the failing tests** (in `spr.rs` tests mod)

```rust
#[test]
fn v5_round_trips_frame_transforms() {
    let mut s = sample_spr(); // adapt the existing fixture builder to SprFrame
    s.frames[0].transform = FrameTransform { angle256: 64, sx: 0x0200, sy: 0x0100, shear_x: 0x0040, shear_y: 0 };
    let blob = encode_spr(&s);
    assert_eq!(blob[4], SPR_VERSION); // 5
    assert_eq!(decode_spr(&blob), Some(s));
}

#[test]
fn v4_blobs_decode_with_identity_transforms() {
    // Hand-encode a v4 blob (copy encode_spr's layout but version 4 and no
    // transform block) and decode: transforms are IDENTITY.
    let blob = encode_v4_fixture();
    let s = decode_spr(&blob).expect("v4 stays decodable");
    assert!(s.frames.iter().all(|f| f.transform == FrameTransform::IDENTITY));
}

#[test]
fn v5_truncation_is_rejected() {
    let blob = encode_spr(&sample_spr());
    for len in 0..blob.len() {
        assert!(decode_spr(&blob[..len]).is_none(), "truncated at {len}");
    }
}
```

- [ ] **Step 2: Run `cargo test --manifest-path /home/clay/projects/ggo/sdk/ggo-asset-formats/Cargo.toml` — FAIL (types missing)**

Note: if the crate only builds inside a workspace, run from `/home/clay/projects/ggo` with the manifest path the existing tests use (worldlib tests ran via `/home/clay/projects/ggo/tools/Cargo.toml`; the sdk crates may have their own workspace — check `sdk/Cargo.toml`).

- [ ] **Step 3: Implement**

In `reader.rs` (if missing): `pub fn i16(&mut self) -> Option<i16> { self.u16().map(|v| v as i16) }`.

In `spr.rs`:
```rust
pub const SPR_VERSION: u8 = 5;
pub const SPR_VERSION_V4: u8 = 4;
// keep SPR_VERSION_V3 = 3

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTransform { pub angle256: u8, pub sx: i16, pub sy: i16, pub shear_x: i16, pub shear_y: i16 }
impl FrameTransform {
    pub const IDENTITY: Self = Self { angle256: 0, sx: 0x0100, sy: 0x0100, shear_x: 0, shear_y: 0 };
    pub fn is_identity(&self) -> bool { *self == Self::IDENTITY }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SprFrame { pub map: Vec<u16>, pub duration_ms: u16, pub transform: FrameTransform }
```

encode: after `duration_ms`, `out.push(t.angle256)` then `sx, sy, shear_x, shear_y` as `to_le_bytes()` each. decode: allowlist `{SPR_VERSION, SPR_VERSION_V4, SPR_VERSION_V3}`; in the frame loop, `let transform = if version >= 5 { FrameTransform { angle256: r.u8()?, sx: r.i16()?, sy: r.i16()?, shear_x: r.i16()?, shear_y: r.i16()? } } else { FrameTransform::IDENTITY };`.

Fix all in-crate users of the old tuple (`spr.rs` tests, `section.rs` if it touches frames).

- [ ] **Step 4: Run the crate's tests — PASS. Do not commit (global constraint).**

---

### Task 2: fixed-point matrix composer (repo: ggo)

**Files:**
- Modify: `/home/clay/projects/ggo/sdk/ggo-asset-formats/src/spr.rs` (impl block on `FrameTransform`)
- Test: `spr.rs` tests + one equivalence test in `/home/clay/projects/ggo/sdk/gemdrop-sdk/src/lib.rs` tests

**Interfaces:**
- Produces: `pub fn matrix(&self) -> (u32, u32)` on `FrameTransform` — packed `(mat_ab, mat_cd)` for `spr_affine`, screen→texture inverse, i16-saturated. Also `pub const AFFINE_ONE_88: i32 = 0x100;` and a crate-local `const SIN_88: [i16; 256]` (copy of `gemdrop-sdk`'s `SIN` table values — duplication pinned by the equivalence test, because the sdk↔formats dependency edge must not exist in either direction).

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn identity_matrix_is_the_unit_matrix() {
    assert_eq!(FrameTransform::IDENTITY.matrix(), (0x0000_0100, 0x0100_0000));
}

#[test]
fn shear_zero_matches_rotscale_semantics() {
    // Mirror gemdrop-sdk's rotscale math locally: for a grid of
    // (angle, sx, sy) incl. extremes, matrix() with zero shear must equal
    // the reference formula PA=(cos<<8)/sx ... with the same clamping.
    for &(a, sx, sy) in &[(0u8, 0x100, 0x100), (64, 0x100, 0x100), (32, 0x200, 0x80), (200, 0x001, 0x7FFF)] {
        let t = FrameTransform { angle256: a, sx, sy, shear_x: 0, shear_y: 0 };
        assert_eq!(t.matrix(), reference_rotscale(a, sx as i32, sy as i32));
    }
}
```
`reference_rotscale` in the test reimplements the sdk formula verbatim (guard against div-by-zero exactly as `rotscale` does: scale 0 → 1/256 clamp).

In `gemdrop-sdk` tests (`sdk/gemdrop-sdk/src/lib.rs`):
```rust
#[test]
fn asset_formats_sin_table_matches_the_sdk_table() {
    for i in 0..256 { assert_eq!(ggo_asset_formats::spr::SIN_88[i], crate::affine::SIN[i]); }
}
```
(Requires `SIN` pub and a dev-dependency `ggo-asset-formats` in gemdrop-sdk — dev-dep only, no production edge. If `SIN` is private, make it `pub` — it is a pure const table.)

- [ ] **Step 2: Run — FAIL (matrix missing).**

- [ ] **Step 3: Implement `matrix()`**

Compose texture→screen `M = R(angle) · S(sx,sy) · Sh(shear)` in i32 8.8, then invert analytically and convert to screen→texture 8.8 with the same `/det` and clamp shape as `rotscale`'s per-axis division; when shear is zero this MUST reduce to exactly the `rotscale` expressions (structure the code so the shear terms literally add to the pre-inversion off-diagonals and vanish at zero). Saturate every entry via `clamp16` (copy: `v.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16 as u32`). Pack `(pa | pb << 16, pc | pd << 16)`.

- [ ] **Step 4: Run both crates' tests — PASS.**

---

### Task 3: worldlib model + op + io (repo: ggo)

**Files:**
- Modify: `/home/clay/projects/ggo/tools/ggo-worldlib/src/sprites/cow.rs` (`Frame` gains `transform`)
- Modify: `/home/clay/projects/ggo/tools/ggo-worldlib/src/sprites/sprite_doc.rs` (new op)
- Modify: `/home/clay/projects/ggo/tools/ggo-worldlib/src/sprites/io.rs` (marshal both directions)
- Tests: those files' tests mods (expect wide fallout: every `Frame { map, duration_ms }` literal gains `transform: FrameTransform::IDENTITY` — mechanical)

**Interfaces:**
- Consumes: `ggo_asset_formats::spr::{FrameTransform, SprFrame}` (Task 1).
- Produces: `cow::Frame { map, duration_ms, transform: FrameTransform }`; `pub use ggo_asset_formats::spr::FrameTransform;` from `sprites::cow`; `DocOp::FrameTransformSet { frame: usize, transform: FrameTransform }`.

- [ ] **Step 1: Failing tests** (sprite_doc.rs)

```rust
#[test]
fn frame_transform_set_applies_and_undoes() {
    let mut store = SpriteDocStore::new(blank_sprite_state(1, 1).unwrap());
    let t = FrameTransform { angle256: 64, sx: 0x0100, sy: 0x0100, shear_x: 0, shear_y: 0 };
    store.apply(DocOp::FrameTransformSet { frame: 0, transform: t }).unwrap();
    assert_eq!(store.state().frames[0].transform, t);
    assert!(store.undo());
    assert_eq!(store.state().frames[0].transform, FrameTransform::IDENTITY);
    assert!(store.apply(DocOp::FrameTransformSet { frame: 9, transform: t }).is_err());
}

#[test]
fn frame_add_copy_of_carries_the_transform_and_blank_frames_are_identity() {
    // set a transform on frame 0, FrameAdd{copy_of: 0} -> copy has it;
    // FrameAdd{copy_of: None} -> identity.
}
```
io.rs test: save a state with a non-identity transform, `open_sprite` it back, transform survives (extend the existing round-trip test).

- [ ] **Step 2: Run worldlib suite (`cargo test --manifest-path /home/clay/projects/ggo/tools/Cargo.toml -p ggo-worldlib`) — FAIL/compile errors.**

- [ ] **Step 3: Implement** — add the field (default via `FrameTransform::IDENTITY` in every constructor incl. `blank_sprite_state`, `resize_frame_map` product frames keep their source frame's transform where a frame survives; NEW frames from FrameAdd blank = identity; copy_of clones source's). Op arm mirrors `FrameDuration`'s shape (validate index, clone state, set). io marshal: `to_spr_data` maps `f -> SprFrame { map, duration_ms, transform }`; open maps back. Mechanically add `transform: FrameTransform::IDENTITY` to every test literal.

- [ ] **Step 4: Full worldlib suite green.**

---

### Task 4: worldlib transformed preview (repo: ggo)

**Files:**
- Modify: `/home/clay/projects/ggo/tools/ggo-worldlib/src/sprites/preview.rs`
- Test: its tests mod

**Interfaces:**
- Produces: `pub fn compose_frame_rgba_transformed(state: &SpriteState, frame: usize, lcd: bool) -> image::RgbaImage` (same signature family as `compose_frame_rgba`): identity → delegate to `compose_frame_rgba`; else inverse-map each destination pixel through `transform.matrix()` (screen→texture, §9.1 semantics: center-anchored, nearest-neighbor, out-of-texture = transparent), canvas = double the frame's px dimensions (the DOUBLE_SIZE rect, so rotated corners never clip).

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn identity_transform_composes_byte_identical_to_the_legacy_path() { /* compare .as_raw() */ }

#[test]
fn a_90_degree_rotation_moves_an_asymmetric_pixel_exactly() {
    // 1x1-tile sprite, single opaque pixel at (1, 0) relative to center;
    // angle256 = 64 -> that pixel must appear at the 90°-rotated position
    // in the doubled canvas, and the original position must be transparent.
}
```

- [ ] **Step 2: FAIL. Step 3: implement with pure integer math (i32 accumulators, >>8). Step 4: worldlib suite green.**

---

### Task 5: emerald host-safe `spr_affine` (repo: emerald)

**Files:**
- Modify: `/home/clay/projects/emerald/crates/core/src/gfx/ppu.rs`
- Test: its tests (follow the existing host-shadow wrapper pattern at ppu.rs:241-295 — read it first)

**Interfaces:**
- Produces: `pub fn spr_affine(set: u32, mat_ab: u32, mat_cd: u32)` — riscv32: `sys::spr_affine`; host: records into a shadow `[[u32; 2]; 32]` readable by tests (mirror how `set_scroll`/`oam_set` shadow). Also `pub fn spr_affine_shadow() -> ...` test accessor if the existing pattern exposes one.

- [ ] Steps: failing test (host build: calling `spr_affine(3, a, b)` shows up in the shadow; set >= 32 is a debug-checked no-op), FAIL, implement, emerald workspace tests + clippy green.

---

### Task 6: emerald runtime application (repo: emerald)

**Files:**
- Modify: `/home/clay/projects/emerald/crates/core/src/gfx/anim.rs` (`Frame` gains `transform: FrameTransform`, populated in `MetaSprite::new` from `spr.frames[i].transform`)
- Modify: `/home/clay/projects/emerald/crates/core/src/gfx/sprite.rs` (static sprite carries frame 0's transform)
- Modify: `/home/clay/projects/emerald/crates/core/src/gfx/oam.rs` (`OamSprite` gains `affine: Option<u8 /*pidx*/>`; `set_pos` ORs `oam::AFFINE | oam::DOUBLE_SIZE` + `param_set_bits(pidx)` into attr/palette when Some; centered offset shifts the extra `-side/2`)
- Modify: `/home/clay/projects/emerald/crates/core/src/gfx/render.rs` (param-set dedup allocator in the frame pass)
- Tests: those files' tests

**Interfaces:**
- Consumes: `FrameTransform::{is_identity, matrix}` (Tasks 1-2), `ppu::spr_affine` (Task 5), `sys::oam::{AFFINE, DOUBLE_SIZE, param_set_bits}`.
- Produces: `render`-internal `AffineSets { by_matrix: /* (u32,u32) -> u8, insertion-ordered, cap 32 */ }` with `fn set_for(&mut self, m: (u32, u32)) -> Option<u8>`; `None` past 32 (sprite renders unrotated; `debug_log` once per frame).

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn affine_sets_dedup_and_cap_at_32() {
    let mut sets = AffineSets::default();
    let a = (1u32, 2u32);
    assert_eq!(sets.set_for(a), Some(0));
    assert_eq!(sets.set_for(a), Some(0), "same matrix reuses the set");
    for i in 1..32 { assert_eq!(sets.set_for((i as u32 + 10, 0)), Some(i as u8)); }
    assert_eq!(sets.set_for((999, 999)), None, "the 33rd distinct matrix overflows");
}

#[test]
fn an_affine_frame_commits_the_oam_bits_and_the_param_set() {
    // Host-shadow test: sprite with a rotated frame; after a render pass
    // the OAM shadow's attr has AFFINE|DOUBLE_SIZE, palette carries
    // pidx[3:0], and spr_affine shadow slot 0 holds transform.matrix().
    // An identity-frame sprite in the same pass keeps plain attr bits.
}

#[test]
fn multi_cell_grid_sprites_stay_unrotated() {
    // >64px footprint + non-identity transform -> rendered via the legacy
    // path (no AFFINE bit), matching the fail-safe philosophy.
}
```

- [ ] **Step 2: FAIL. Step 3: implement** — in the render pass, before the `render_at` scans, walk visible sprites once collecting non-identity single-OAM matrices into `AffineSets`; emit `spr_affine` for sets whose matrix changed since last frame (keep the previous frame's table to diff); pass each sprite's `Option<pidx>` down into `render_at`/`OamSprite`. Centered anchor: when affine+double, subtract an extra `(w/2, h/2)` screen px.

- [ ] **Step 4: emerald workspace tests + clippy (float-deny) green.**

---

### Task 7: zed sprite panel editors + preview + meter (repo: zed fork)

**Files:**
- Modify: `crates/ggo/sprite_panel/src/ggo_sprite_panel.rs` (EditTarget variants, sequence-entry transform line, meter)
- Modify: `crates/ggo/sprite_panel/src/loader.rs` (preview composes via the transformed composer for the shown frame)
- Modify: `crates/ggo/sprite_panel/src/edits.rs` (parse/format helpers)
- Tests: those files (TDD, repo's established patterns)

**Interfaces:**
- Consumes: `ggo_worldlib::sprites::cow::FrameTransform`, `DocOp::FrameTransformSet`, `compose_frame_rgba_transformed` (Tasks 3-4).
- Produces (panel-internal): `EditTarget::{Rot, ScaleX, ScaleY, ShearX, ShearY}` single editors bound to the selected frame, rendered in the clips row beside the existing `ms` editor; `edits::{parse_angle_deg -> Option<u8>, format_angle_deg(u8) -> String, parse_fixed88 -> Option<i16>, format_fixed88(i16) -> String}` pure + unit-tested (deg↔step: `step = round(deg * 256 / 360) % 256`; fixed: decimal with 2 places, clamp to i16 range, junk → None).

- [ ] **Step 1: Failing tests**

```rust
// edits.rs
#[test]
fn angle_and_fixed_parsers_round_trip_and_reject_junk() {
    assert_eq!(parse_angle_deg("90"), Some(64));
    assert_eq!(format_angle_deg(64), "90");
    assert_eq!(parse_fixed88("1.0"), Some(0x0100));
    assert_eq!(parse_fixed88("2.5"), Some(0x0280));
    assert_eq!(parse_fixed88("junk"), None);
}

// panel: commit path
#[gpui::test]
async fn test_transform_editors_commit_through_the_doc(cx: &mut TestAppContext) {
    // ready_panel; commit_edit(EditTarget::Rot, "90"); assert
    // frames[selected].transform.angle256 == 64; undo reverts; junk in
    // ScaleX reverts to the doc value like the duration editor.
}

// meter
#[test]
fn hw_meter_counts_distinct_non_identity_transforms() {
    // tiles::hw_meter_line gains "Sets n/32" -- distinct non-identity
    // transform matrices across frames.
}

// preview
#[gpui::test]
async fn test_preview_shows_the_transformed_frame(cx: &mut TestAppContext) {
    // set a 90° rotation on the shown frame; the preview image dimensions
    // double (the transformed canvas), identity frames keep legacy dims.
}
```

- [ ] **Step 2: FAIL. Step 3: implement** — editors follow the Duration editor's exact plumbing (`ensure_editors` targets, `edit_display_text`, `commit_edit` arm applying `FrameTransformSet` with the parsed field merged into the frame's current transform); render them in the clips row's ms cluster with XSmall labels `rot/sx/sy/shx/shy`. Preview: `refresh_after_doc_change`/loader compose the SHOWN frame with `compose_frame_rgba_transformed`. Meter: extend `tiles::hw_meter_line` signature with the frames slice it already receives.

- [ ] **Step 4: `cargo test -p ggo_sprite_panel` + `./script/clippy -p ggo_sprite_panel` green.**

---

### Task 8: cross-repo verification sweep

- [ ] `cargo test` green in: ggo sdk workspace (asset-formats + gemdrop-sdk), ggo tools workspace (worldlib), emerald workspace (+ its clippy), zed `-p ggo_sprite_panel` + all ggo panels (`cargo test -p ggo_sprite_panel -p ggo_world_panel -p ggo_import_panel`), zed clippy, `cargo build -p zed`.
- [ ] Manual sanity list for clay: open a sprite, set rot 90 on a clip entry, preview rotates, save; reopen — persists; emerald cart build still packs the v5 asset (run `emd build` on a scaffold if cheap).
- [ ] Report per-repo dirty file lists; NO commits.
