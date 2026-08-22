# Per-frame affine transforms: .spr v5, editor, emerald runtime

2026-08-22. Approved in chat (clay): representation = angle + scale +
shear; scope = full pass (format + editor + emerald runtime). Three
repos: `../ggo` (format + worldlib), `../emerald` (runtime), this zed
fork (sprite panel UI).

## Ground truth (from the repo scouts)

- The hardware stack is complete and LOCKED (ggo `docs/ppu-contract.md`
  §9): sprites select one of **32 shared affine parameter sets**; a set
  is the inverse (screen→texture) matrix `PA PB PC PD` in signed 8.8.
  OAM `attr[5]` enables affine (h/v-flip then IGNORED), `attr[6]`
  doubles the screen rect, `pidx[4:0]` picks the set. The SDK has
  `affine::rotscale(angle256, sx, sy)`, a 256-step integer sin table,
  and syscalls `bg_affine`/`spr_affine`. Integer-only ABI.
- `.spr` codec lives in ggo `sdk/ggo-asset-formats/src/spr.rs` (v4,
  strict version allowlist, no per-record slack). Both emerald's
  runtime and worldlib's editor marshal through it. v3→v4 set the
  version-branch precedent.
- Emerald has zero affine today: `Transform` is pos+z, the OAM writer
  never sets bits 5/6/7, and there is no host-safe `spr_affine`
  wrapper. ggo `docs/ppu-affine-plan.md` Phase 5 explicitly deferred
  "MetaSprite rotation/scale API" to emerald — this spec is that phase.
- In the editor, clip-sequence entries are physical frame copies, so
  per-FRAME transform data is per-clip-entry in exactly the way
  durations already are.

## Format: `.spr` v5 (ggo/sdk/ggo-asset-formats)

Frame record gains a transform block after `duration_ms`:

```
angle256   u8          0 = no rotation, 64 = 90°, matches SDK sin table
sx         i16 LE      8.8 on-screen X scale, 0x0100 = 1.0
sy         i16 LE      8.8 on-screen Y scale, 0x0100 = 1.0
shear_x    i16 LE      8.8, 0 = none
shear_y    i16 LE      8.8, 0 = none
```

- Identity = `(0, 0x0100, 0x0100, 0, 0)`. A frame whose transform is
  identity renders through the legacy non-affine path (bit-exact per
  the emulator's identity test), so upgraded assets look unchanged.
- `SPR_VERSION` bumps to 5. Decoder allowlist becomes {3, 4, 5}; v3/v4
  frames decode with identity transforms. Writers always emit v5.
- `SprData` frame tuple becomes a named struct
  `SprFrame { map, duration_ms, transform: FrameTransform }` with
  `FrameTransform { angle256: u8, sx: i16, sy: i16, shear_x: i16,
  shear_y: i16 }` and `FrameTransform::IDENTITY`; a
  `FrameTransform::matrix() -> (u32, u32)` helper composes
  rotation∘scale∘shear into the packed `(mat_ab, mat_cd)` the syscall
  takes, reusing the SDK sin table (integer-only, i16-saturating,
  screen→texture inversion identical to `rotscale`; shear terms add
  `shear_x`/`shear_y` off-diagonal contributions before inversion
  clamping). Property test: `shear == 0` ⇒ byte-identical to
  `rotscale`.

## Worldlib (ggo/tools/ggo-worldlib)

- `cow::Frame` gains `transform: FrameTransform` (re-exported from
  ggo-asset-formats so there is ONE definition).
- New op `DocOp::FrameTransformSet { frame, transform }` — validated
  (frame in range), one undo step. `FrameAdd { copy_of }` copies the
  source's transform; blank frames get identity; `Resize`/erase leave
  transforms untouched.
- `io::save_sprite`/`open_sprite` round-trip the field; fold-back and
  dedup ignore transforms (they operate on tiles, not frames).
- `preview::compose_frame_rgba` gains a transformed variant
  `compose_frame_rgba_transformed` used by the editor preview: inverse-
  map each destination pixel through the same fixed-point matrix
  (nearest-neighbor, matching hardware §9.1 semantics) into a canvas
  sized for the transformed bounds. Identity short-circuits to the
  existing composer.

## Editor (zed fork, ggo_sprite_panel)

- Each clip-sequence entry grows a second line beside `N ms`:
  `rot / sx / sy / shx / shy` as compact steppers-or-fields.
  - rot displayed in degrees (0–359 mapped to angle256 steps; display
    rounds, storage is the u8 step).
  - sx/sy/shx/shy displayed as decimals (1.0 = 0x100), parsed and
    clamped to the i16 8.8 range; junk reverts like the duration
    editor.
  - Edits go through `FrameTransformSet` via `apply_doc` (undo,
    recompose, error surfacing).
- The previewer shows the SELECTED frame with its transform applied
  (the new transformed composer); onion ghosts stay untransformed
  (cheap, and ghosts are alignment aids).
- The frames LIBRARY stays transform-free: transforms are per-copy
  clip data, exactly like ms. `library_indices` dedup stays keyed on
  tile maps only.
- The hw meter gains a `Sets n/32` figure: distinct non-identity
  transforms across the sprite's frames (the param-set budget).

## Emerald runtime (../emerald)

- `gfx::Frame` gains the transform (from the shared decoder);
  `MetaSprite` carries it per frame.
- Host-safe PPU wrapper `ppu::spr_affine(set, mat_ab, mat_cd)`
  following the existing riscv32/host-shadow pattern.
- Param-set allocator in the render pass: per frame drain, dedup the
  matrices of all VISIBLE affine sprites (identity ⇒ non-affine path);
  first 32 distinct matrices get sets, one `spr_affine` write per
  distinct matrix per frame only when changed; overflow beyond 32 logs
  once and renders those sprites unrotated (never corrupts OAM —
  matches the grid allocator's fail-safe philosophy).
- OAM commit: affine frames OR `AFFINE|DOUBLE_SIZE` plus
  `param_set_bits(pidx)`; centered offsets shift by the extra
  `-side/2` the doubled rect introduces.
- Constraint v1: affine applies to single-OAM frames (≤64px each
  dimension). Multi-cell grid sprites render unrotated with a
  debug-log note; per-cell corner math is a later phase.
- The baker (`crates/assets`) passes transforms through when reading
  Aseprite sources (default identity — Aseprite has no affine).

## Sequencing (cross-repo, in dependency order)

1. ggo: ggo-asset-formats v5 + `FrameTransform` + matrix helper.
2. ggo: worldlib model/op/io/preview.
3. emerald: decoder pickup (shared struct), runtime application.
4. zed: sprite panel editors + transformed preview + meter.
Each step lands with its repo's tests green before the next starts;
the zed step can proceed in parallel with 3 once 2 lands.

## Testing

- ggo-asset-formats: v5 round-trip, v4 decode ⇒ identity, truncation
  rejection, matrix helper vs `rotscale` (shear-zero equivalence),
  saturation at extreme scales.
- worldlib: op apply/undo, copy-of propagation, save/open round-trip,
  transformed compose (90° rotation pixel-exact on an asymmetric
  fixture, identity short-circuit).
- emerald: allocator dedup and 32-overflow fail-safe, OAM bits and
  centered-offset math, host-shadow wrapper, identity ⇒ legacy path.
- zed panel: editor commit/clamp/revert per field, undo, meter count,
  preview swaps to the transformed composite.

## Non-goals

- BG affine (camera/ref-point plumbing) — separate feature.
- Rotating multi-OAM grid sprites as a unit.
- Skew helpers in the SDK (matrix composition lives in the shared
  format crate instead).
- Tweening between frames — transforms are stepwise per frame, like
  duration.
