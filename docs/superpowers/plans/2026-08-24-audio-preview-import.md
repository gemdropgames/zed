# Audio preview + import — plan

Spec: `docs/superpowers/specs/2026-08-24-audio-preview-import-design.md`.
Branches: zed `audio-authoring` (off `ggo`), ggo `ggo-audio` (off `main`).

## Phase 1 — `ggo-audio` lib (`../ggo/tools/ggo-audio`)
What: wav/ogg → mono → resample → `.adp`, read-back, budget arithmetic.
Why: editor must bake at a chosen rate and preview the result; emerald's
copy is unreachable, the SDK crate is `no_std`.
1. Scaffold crate (`[lib] path = "src/ggo_audio.rs"`), workspace member.
2. `decode.rs`: wav (PCM8/16/f32) + ogg (lewton) → `Decoded`. Tests.
3. `bake.rs`: resample, `bake`, `RATES`, `default_rate`, `baked_bytes`,
   `decode_adp`, `SAMPLE_REGION_BYTES` const-assert. Tests.
4. `io.rs`: `write_adp` temp+rename. Test.
5. Commit + push ggo.

## Phase 2 — audio tab (`crates/ggo/audio_panel`)
What: center tab with waveform, transport, Source|Baked A/B via a
standalone `Apu`, rate picker, Import → `assets/<stem>.adp`.
Why: nothing in the editor can play or bake a musician's file today.
6. Workspace deps + `init` registration in `crates/zed/src/main.rs`.
7. `ggo_audio_panel.rs`: interceptor, actions, keys. Test.
8. `audio_item.rs`: `AudioItem`, `open_audio_item`. Test.
9. `load.rs`: off-thread decode + waveform buckets. Test.
10. `preview.rs`: Source / Baked thread over the emu panel's ring. Tests.
11. Render: header, canvas, transport, readout, error line.
12. Import flow. Test.
13. Commit.

## Phase 3 — world budget line
What: `audio N / 384 KiB` in the world toolbar, red when over.
Why: the runtime silently skips over-region uploads.
14. `audio_budget.rs`: collect stems, size, cache. 15. Toolbar readout.
16. Tests. 17. Commit.

## Phase 4 — wrap
18. clippy + test sweep ×2. 19. `MIGRATION.md` rows, tick
`tasks/editor-gaps.md`. 20. Merge `audio-authoring` → `ggo`, push `ggo` + `main`.
