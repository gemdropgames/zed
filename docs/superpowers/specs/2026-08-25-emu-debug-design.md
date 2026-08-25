# Emulator debug surfaces — design

Status: approved 2026-08-25. Task 3 of `tasks/editor-gaps.md`.

## Goal

Pause, single-frame step, and four live viewers (tiles, tilemap, OAM,
palettes) in the emulator tab, plus five known emu bugs. GB Studio 4.3's
debugger is the bar: see the live background and overlay tilemaps while
the cart runs.

## Facts this rests on

- The emulator state (`Cpu`, `Mmu`, `Peripherals { ppu, apu, .. }`) is
  built inside the drive thread (`emu_panel/src/drive.rs`) and never
  crosses to the UI thread. The only frame boundary is the `Vsync` arm.
- `ggo_emu_core::ppu::Ppu` keeps tiles / maps / palettes / OAM / scroll /
  layer state private; the only public read is `decode_tile_rgba`
  (palette 0 only). Layouts: `../ggo/docs/ppu-contract.md` §1, §11.
- The pane already paints nearest-neighbour (`paint_image(.., nearest =
  true)`); the "linear-upscale blur" note in `MIGRATION.md` is stale.
- Keys stick because `release_all_buttons` only runs on focus-out; a
  window deactivation with focus kept fires nothing.
- `EmulatorItem` implements no `deactivated`; a hidden tab keeps running.
- `ingest.rs` rejects a run over `MAX_FRAMES = 100_000` outright.
- `savefile::flush_save` lives in the `ggo-emu` binary crate; the panel
  deliberately does not depend on that crate.

## Decisions

1. **Snapshot slot, not request/reply.** The drive thread writes a
   `PpuSnapshot` into an `Arc<Mutex<Option<Arc<PpuSnapshot>>>>` at every
   vsync (~138 KB memcpy). Viewers read the latest when they render. No
   second channel; `Session` stays consuming-`wait` as today.
2. **Pause/step at the frame boundary.** `pause: AtomicBool`,
   `step: AtomicU32` on `Session`; the loop parks after publishing a
   frame while `paused && step == 0 && !stop`, pushing silence into the
   audio ring each 16 ms so the device stays live without counting
   dropouts. Pacing resets on resume.
3. **Viewers live in a toggleable debug column of the emu tab**, four
   tabs, decoded off-thread from the latest snapshot; throttled to 10 Hz
   while running, immediate on pause/step. Images follow the pane's
   `drop_image` discipline.
4. **Core read API is additive** (`../ggo/tools/ggo-emu-core`): a
   `PpuSnapshot` struct + `Ppu::snapshot_into` + pure decode helpers that
   take an explicit palette. `savefile.rs` moves from the `ggo-emu` binary
   into the core (the binary re-exports it).

## `ggo_emu_core` additions

```rust
pub struct PpuSnapshot {
    pub tiles: Vec<u8>,            // VRAM_TILE_CAP * TILE_BYTES, 4bpp
    pub maps: [Vec<u16>; 4],       // 32x32 cells: [9:0] tile [13:10] pal [14] h [15] v
    pub palettes: [Vec<u16>; 2],   // 16x16 RGB565 per bank (0 bg/fg, 1 sprite)
    pub oam: Vec<[u8; 8]>,         // 128 entries, contract §1
    pub scroll: [(u16, u16); 4],
    pub layer_enable: [bool; 4],
    pub layer_prio: [u8; 4],
}
impl Ppu { pub fn snapshot_into(&self, out: &mut PpuSnapshot); pub fn snapshot(&self) -> PpuSnapshot; }
impl PpuSnapshot {
    pub fn tile_rgba(&self, tile: u16, bank: usize, palette: usize, hflip: bool, vflip: bool, out: &mut [u8]); // 16x16 RGBA
    pub fn map_cell(&self, layer: usize, x: usize, y: usize) -> MapCell;   // decoded fields
    pub fn oam_entry(&self, index: usize) -> OamEntry;                      // decoded fields
}
pub mod savefile;  // moved from ggo-emu; `flush_save(cart_path, &save) -> io::Result<()>`, `load_save(cart_path) -> Option<Vec<u8>>`
```

## Drive loop

- `Session::{pause, resume, step, is_paused, snapshot}`; `Session::snapshot()`
  returns `Option<Arc<PpuSnapshot>>` (clone of the slot).
- Vsync arm order: publish frame → pump audio → write snapshot → flush
  save if dirty → pace → park while paused (silence pushed per 16 ms;
  a pending `step` runs exactly one more vsync then parks again) →
  latch input → ticks.
- `DebugTick` is the step action; `run` clears pause.

## Panel

- Transport: Run · Stop · **Pause/Resume** · **Step** · Mute · **Debug**;
  readout shows `paused` / `frame N`.
- Keys (all `ctrl-alt-`, bare letters are pad keys): `p` pause/resume,
  `.` step, `d` debug column.
- Debug column (right of the screen, ~360 px, `v_flex` with a tab row):
  - **Tiles**: bank selector (bg/fg, sprite) + palette selector 0–15; the
    1024-tile sheet as a 32×32 grid at 1× (512×512, scrollable), hover
    readout `tile N`.
  - **Map**: layer 0–3 selector; the 32×32 map composed at 1× (512×512)
    with the visible 320×240 window outlined at the layer's scroll; hover
    readout `cell (x,y) tile N pal P h v`; disabled layers greyed.
  - **OAM**: enabled entries first; each row `#i x y tile size flips pal
    prio` + a 16×16 thumbnail.
  - **Palettes**: two banks × 16 rows × 16 swatches; hover readout
    `bank/pal/slot = #RRGGBB (0xNNNN)`.
- Decode runs in `cx.background_spawn` from the latest snapshot into
  RGBA buffers → `RenderImage`s; a generation counter drops stale
  results; images retire through the pane's existing atlas discipline.
- Throttle: while running, re-decode at most every 100 ms and only when
  the column is open; on pause/step, immediately.

## Bug fixes

- Blur: doc-only — strike the `MIGRATION.md` rows.
- Stuck keys: `cx.observe_window_activation` → `release_all_buttons` when
  the window deactivates.
- Hidden tab: `Item::deactivated` → `session.pause()` with
  `auto_paused = true`; the next `render` (only visible items render)
  resumes if `auto_paused`. A user pause is never auto-resumed.
- MAX_FRAMES: keep the first 100 000 frames, ingest them, and report
  `truncated to 100000 frames` in the ingest status.
- Saves: on vsync when `p.save_dirty` and at run end, `savefile::flush_save`
  next to the cart, same path rule as the standalone.

## Tests

- Core: `snapshot_into` round-trips every field after the syscall
  handlers write them; `tile_rgba` honours palette + flips; `map_cell` /
  `oam_entry` bit decoding against contract §1.
- Drive: pause parks (frame number stops advancing; silence keeps
  flowing); step advances exactly one vsync; resume continues; a paused
  run still stops; snapshot slot is fresh after a step.
- Panel: transport/keys drive the session; debug column decodes a
  fixture snapshot into images of the expected sizes; OAM ordering;
  hidden-tab auto-pause + render auto-resume; window deactivation
  releases the pad; ingest truncation at 100 001 frames; save flushed
  after `save_write`.

## Out of scope

CPU-level breakpoints/step-into, memory hex viewer, input recording,
full-system mode, cache viewers (the wasm data panel's cache export).
