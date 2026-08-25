# Audio preview + import — design

Status: approved 2026-08-24. Task 1 of `tasks/editor-gaps.md`.

## Goal

Musicians deliver `.wav` / `.ogg`. ZedGG must let the studio hear a file,
hear what the hardware will make of it, choose the baked rate, write the
`.adp` the cart ships, and see whether a world's audio fits the 384 KiB
sample region. No synthesis, no sequencer, no PSG/ADSR/pan authoring
(emerald's runtime never programs those, so nothing authored there could
ship).

## Facts this design rests on

- APU contract (`../ggo/docs/apu-contract.md`, locked): 8 sample channels
  playing GGO-ADPCM from a 384 KiB VRAM region at 32 kHz × 4.12 step; 64 B
  blocks of 120 samples. Emerald: ch0 music, ch1–7 SFX, resets the region
  per world, **silently skips** an upload that does not fit.
- `.adp` = `rate_hz u32 | block_count u32 | blocks`. Emerald bakes
  `assets/**/*.wav|ogg` to `.adp` at pack (wav→16 kHz, ogg→8 kHz, mono,
  never upsampled) and copies a pre-baked `.adp` verbatim
  (`AssetKind::Adp`). Worlds reference audio by stem via `Music{stem}` /
  `Sfx{stem}`; runtime loads `{stem}.adp`.
- `ggo_emu_core::apu::Apu` runs standalone (no CPU): `new`, `queue_samples`,
  `play_sample`, `run_frame`, `copy_since`.
- `ggo_emu_panel::audio` already owns a cpal ring (`channel`, `start_output`,
  `AudioStatus`) with the no-device / mute / dropout behaviour.

## Decisions

1. **No emerald changes.** The rate knob is editor-side: bake in the
   editor, write `assets/<stem>.adp`; emerald packs it verbatim. Raw
   wav/ogg drop-in at emerald's default rate keeps working.
2. **New host-side crate `../ggo/tools/ggo-audio`** (`ggo_audio`), not
   `ggo-worldlib` (misnamed for this) and not `ggo-asset-formats`
   (`no_std`, firmware consumer). Deps: `anyhow`, `lewton`,
   `ggo-asset-formats`, `ggo-emu-core` (const-assert only).
3. **New panel crate `crates/ggo/audio_panel`**, center tab per file,
   same `Item` shape as the tileset tab, no document/dirty state.
4. **Budget line in the world panel**, because that is where the
   per-world set is known and where the silent-skip failure can be shown.

## `ggo_audio` API

```rust
pub struct Decoded { pub samples: Vec<i16> /* mono */, pub rate_hz: u32, pub source_channels: u16 }
pub fn decode(path: &Path) -> Result<Decoded>;           // wav PCM8/16/f32, ogg
pub fn decode_adp(bytes: &[u8]) -> Result<Decoded>;      // read-back
pub const RATES: [u32; 3] = [8_000, 16_000, 32_000];
pub fn default_rate(path: &Path) -> u32;                 // wav→16k, ogg→8k, adp→header
pub fn bake(decoded: &Decoded, rate_hz: u32) -> Vec<u8>; // resample (never up), encode_adp
pub fn baked_bytes(sample_count: usize, from_hz: u32, to_hz: u32) -> u32; // region bytes, no header
pub const SAMPLE_REGION_BYTES: u32;                      // == apu::VRAM_SAMPLE_BYTES
pub fn write_adp(root: &Path, rel: &str, bytes: &[u8]) -> Result<()>; // mkdir -p, temp+rename
```

## Audio tab

- Interceptor claims `.wav`, `.ogg`, `.adp` in the primary worktree.
- Regions: header (rel · source rate · channels · seconds) → waveform
  canvas (min/max buckets, playhead) → transport (Play/Stop, Loop,
  Source | Baked, rate picker, Import…) → readout (baked rate, blocks,
  bytes, % of region, seconds) → `CopyableText` error line.
- `.adp` tabs: read-only preview; rate from header; no Import.
- Decode and bake run off-thread with a generation counter; the tab shows
  "decoding…" / "baking…" meanwhile.
- Keys: `space` Play/Stop, `l` Loop; bound like the other panels
  (imperative + keymap-reload observer).
- Import: target rel prefilled `assets/<stem>.adp` (editable); collision
  → `ggo_common::confirm_destructive`; writes via `write_adp`; opens the
  resulting `.adp` tab.

## Preview engine

One OS thread per play (`ggo-audio-preview`), stop flag, loop flag,
playhead `AtomicU64` (frames elapsed).

- Source: decoded PCM pushed straight into the ring; `start_output(…,
  source_rate)`.
- Baked: `Apu::new()`; `queue_samples(0, blocks)`; `play_sample(0, 0, len,
  loop ? 0 : LOOP_NONE, step_for_rate_hz(rate) | vol, adsr 0)`; loop
  `run_frame` → `copy_since` → ring, paced at 60 Hz; one-shot ends after
  `ceil(samples / (rate/60))` frames; `start_output(…, 32_020)`.

## World budget

`audio_budget.rs` in the world panel: every `Music.stem` / `Sfx.stem`
across entities + resolved instances → `<stem>.adp` (header) or
`<stem>.wav|ogg` (`baked_bytes` at default rate, decoded off-thread,
cached by mtime) → `AudioBudget { used, missing }`. Toolbar readout
`audio N / 384 KiB`, `Color::Error` when over, tooltip lists missing
stems. Recomputed with the other asset loads.

## Tests

- `ggo_audio`: wav fixture generated in-test; small checked-in ogg;
  `baked_bytes == bake().len() - 8`; bake → `decode_adp` RMS within
  tolerance; downsample length math; `write_adp` with tempdir.
- Panel: interceptor claims/declines; one tab per path; bucket math;
  preview via `audio::channel` + `RingReader` (no cpal): baked one-shot
  non-silent then ends, loop continues, stop halts; Import writes and
  refuses to clobber without confirm.
- World panel: fixture world with two stems sums; over-region → error;
  missing stem counted 0 and listed.

## Out of scope

Sidecars, emerald changes, PSG/ADSR/pan, sequencer, inspector picker
(task 2 reuses this panel's audition), a `ggo-audio` CLI (30 lines if
wanted later).
