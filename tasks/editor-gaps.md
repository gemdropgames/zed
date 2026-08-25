# ZedGG editor gaps — task list

Derived from the adversarial review of ZedGG against open-source game editors
(Godot 4.6, GB Studio 4.3, LDtk, Tiled, Pixelorama 1.1 / Aseprite, TIC-80).
Report: https://claude.ai/code/artifact/598c7d51-3907-4641-9493-4e2956153bcb
Date: 2026-08-24. Evidence refs point at `crates/ggo/*` and `docs/ggo/MIGRATION.md`.

Ordered by distance from "fully featured gamedev UX" × number of domains touched.
Effort tags are gut calls from the code inventory, not estimates.

---

## 1. Audio authoring — `done 2026-08-24` (scope narrowed)

Musicians deliver `.wav` / `.ogg`; the editor's job is to hear them, hear
what the hardware makes of them, pick the baked rate, write the `.adp` the
cart ships, and keep a world's audio set under the 384 KiB sample region.
Emerald is untouched (it packs a pre-baked `.adp` verbatim). Shipped as
`../ggo/tools/ggo-audio` + `crates/ggo/audio_panel` + the world panel's
budget line. Spec: `docs/superpowers/specs/2026-08-24-audio-preview-import-design.md`.

- [x] Decide the authoring model: import wav/ogg, no synthesis, no sequencer
- [x] Audition: audio tab plays Source or Baked (real `Apu`, ADPCM, 32 kHz mix) through the emu pane's cpal ring
- [x] Rate picker (8/16/32 kHz) + Import → `assets/<stem>.adp`
- [x] Project-panel routing for `.wav` / `.ogg` / `.adp`
- [x] World toolbar `audio N / 384 KiB` readout, red when over, missing stems listed
- [ ] `ggo-audio bake` CLI (30 lines) so scripts can bake without the editor — only if wanted
- [ ] Re-size the world budget when a file changes without a panel refresh (mtime watch)
- Not planned: SFX synthesis, tracker, PSG/ADSR/pan authoring (no delivery path in emerald's runtime)

## 2. Typed asset refs + world-TOML language server — `medium → large`

Correctness regression: `FieldKind::Asset(_)` is handled as `Str` at
`world_panel/src/inspector.rs:113` (display) and `:159` (commit), so a
`Sprite.stem` can point at a file that does not exist with no error
(`MIGRATION.md:154`). `ggo_language` is syntax + outline only and "does not
activate today" without a per-project `file_types` entry (`MIGRATION.md:257`);
the spec's LSP is unbuilt (`MIGRATION.md:484`).
Refs: LDtk typed fields / entity refs, Godot GDScript LSP.

- [ ] Inspector: render `FieldKind::Asset` as a picker over existing stems of the right kind; refuse commit of a missing stem
- [ ] Inspector: show an inline error (not a string) for a stem that no longer resolves
- [ ] Render `loader.rs`'s `node["error"]` in the canvas instead of dropping it (`MIGRATION.md:144`)
- [ ] Activation: ship `path_suffixes`/a default `file_types` glob so `GGO World` turns on without hand-editing `.zed/settings.json`; de-duplicate the glob that lives in two repos (`MIGRATION.md:602`)
- [ ] `ggo-lsp` skeleton (`LspAdapter`): diagnostics for dangling stems and unknown components
- [ ] LSP completions: component names, field names, stems, `[[instance]]` world stems
- [ ] LSP code actions named in the spec: snap / center / offset
- [ ] LSP hardware-budget diagnostics (tile/sprite counts per world)

## 3. Emulator debug surfaces — `medium`

Run / Stop / Mute only. GB Studio 4.3's debugger shows live background +
overlay tilemaps. The drive loop already checks a stop flag once per turn
(`emu_panel/src/drive.rs:390`); pause and single-step are the same mechanism.
Refs: GB Studio 4.3 debugger, Mesen/bgb-style VRAM viewers.

- [ ] Pause / resume flag in `drive::run`, transport button + keybinding
- [ ] Frame advance (single-step one vsync while paused)
- [ ] Tilemap viewer (background + overlay) reading core state between steps
- [ ] OAM / sprite table viewer
- [ ] Palette viewer
- [ ] VRAM / tile viewer
- [ ] Integer-scale framebuffer (replace `img().w_full().h_full()` linear blur — `MIGRATION.md:187`)
- [ ] Release the pad mask on window deactivation (stuck keys — `MIGRATION.md:579`)
- [ ] Pause (or at least stop pumping) when the emulator tab is hidden (`MIGRATION.md:196`)
- [ ] Truncate instead of reject runs over `MAX_FRAMES = 100_000` at ingest (`MIGRATION.md:582`)
- [ ] Call `savefile::flush_save` so pane runs persist saves (`drive.rs:325`)

## 4. World editor interaction floor — `medium`

Selection is `Option<Selection>` (`world_panel/src/ggo_world_panel.rs:614`).
No rubber-band, shift-click, group move, copy/paste, duplicate, entity list
(`MIGRATION.md:150`), or delete confirm (`MIGRATION.md:153`).
Refs: LDtk, Godot 2D editor.

- [ ] Selection set instead of `Option`; shift-click add/remove
- [ ] Rubber-band select on empty-space drag (currently deselect)
- [ ] Group move as one gesture / one undo entry (extend the existing gesture-id coalescing)
- [ ] Copy / paste / duplicate entities and instances (`ctrl-c` / `ctrl-v` / `ctrl-d`)
- [ ] Delete confirm for instance removal (reuse `ggo_common::confirm_destructive`)
- [ ] Entity / instance list in the dock (dock already owns the document); click-to-select, keyboard nav
- [ ] Fix redo-of-add leaving the instance subtree unresolved (`MIGRATION.md:557`)
- [ ] Fix atlas leak: call `drop_image` on cache rebuild, per the emu panel's pattern (`MIGRATION.md:567`)

## 5. Map editor — `medium`

`MapTool` is exactly Brush / RectFill / Eyedropper / Eraser
(`map_panel/src/ggo_map_panel.rs:433`). No flood fill, autotile, layers, cell
multi-select, or collision flags. Per-tileset `cols` was lost, so metatiles
laid out 6 or 12 wide are no longer rect-selectable as one stamp
(`MIGRATION.md:132`, `:453`) — a regression from ggo-ide.
Refs: Tiled terrain sets + automapping, Pixelorama 1.1 autotile.

- [ ] Flood fill tool (`MapOp::Fill`)
- [ ] Cell multi-select + copy / paste / cut of regions
- [ ] Restore per-tileset `cols` as a sidecar (`.ggo-ide/<rel>.editor.json`) read by both the map stamp picker and the tileset editor; delete the dead `tileset_cols:*` db rows
- [ ] Autotile / terrain rules: pick a rule format (Tiled Wang sets or Pixelorama rules), rule editor, apply-on-paint
- [ ] Collision flags per tile (if the `.map`/PPU contract has a bit for it; otherwise an IntGrid-style side layer)
- [ ] Layers — only if the `.map` format grows them; otherwise document that priority lives at the world `[[background]]` level
- [ ] Slider primitive for zoom + palSub (see §10)

## 6. Asset pipeline — `medium`

PNG only (`import_panel:130`), wizard has no undo, no keybindings (`:193`),
no OS drag-and-drop (`:68`), no re-import on change, no thumbnails anywhere,
no in-panel picker anywhere.
Refs: Godot import dock, GB Studio assets folder.

- [ ] OS file drag-and-drop onto the workspace → import wizard
- [ ] Remember the source PNG path in the `.editor.json` sidecar; offer re-import when it changes (mtime watch)
- [ ] Aseprite `.ase`/`.aseprite` reader → tileset + frames (documented format, small)
- [ ] Thumbnails for `.spr` / `.til` in the project panel (or a GGO asset browser dock)
- [ ] Palette surgery in the import preview: ramps, sort, swap (currently one-shot quantization — `import_panel:76`)
- [ ] Keybindings for the import wizard (`bind_panel_keys` is empty)

## 7. Pixel tools — `small each`

Tileset editor tools are Pencil / Eraser / Picker / Select
(`tileset_panel/src/ggo_tileset_panel.rs:196`). Palette is 16 swatches +
per-channel steppers (`palette_widget.rs`).
Refs: Pixelorama, LibreSprite.

- [ ] Bucket fill
- [ ] Line, rect, ellipse tools
- [ ] Brush size
- [ ] Move / transform selection (the marquee exists; it can only copy/paste)
- [ ] Mirror / symmetry painting
- [ ] Delete row / column (only `InsertRow` / `InsertColumn` exist — `:706`, `:722`)
- [ ] Magnified single-tile canvas (zoom currently magnifies the whole sheet)
- [ ] Palette: hex entry, ramps, sort, swap (model on the sprite panel's `PaletteRemap`)
- [ ] Sprite preview: checker / black / white background picker + LCD scanline filter (`MIGRATION.md:128`)
- [ ] Fix atlas leak in the sprite panel (`MIGRATION.md:571`)

## 8. Hot iteration — `small → large`

Every change is save → `emd pack-ggo --world <stem>` → Run.
Refs: Defold, Godot.

- [ ] Watch mode: on save of any asset under the open world, re-pack and restart the cart in the emu tab (one keystroke loop) — `small`
- [ ] Keep the emulator's input/pad + window position across the restart
- [ ] True asset hot-swap: core accepts a new asset section mid-run — `large`, needs `ggo-emu-core` support

## 9. Deploy to hardware — `medium`

Only "Run hardware diagnostics" (`ggo-diag --launch`). Flashing lives in the
ggo repo's `.zed/tasks.json` with raw stdout and no verdict parsing
(`MIGRATION.md:223`, `:225`).
Refs: GB Studio ROM export, TIC-80 cart export.

- [ ] "Flash to board" context-menu entry on a `.cart`/`.ggo` → `ggo-diag` provision + launch flow
- [ ] State machine over `ggo-diag` stdout with parsed verdicts on the emu status row
- [ ] Export cart to a chosen path (the pack already exists; expose it without running)

## 10. Editor chrome consistency — `medium`

Map, emerald, import are right-dock panels; sprite, tileset, emu, charts are
center tabs; world is both. Keybindings are bound imperatively, not in
`assets/keymaps/*.json` (`UPSTREAM.md:128`); charts and import bind nothing.
`ui` has no slider primitive so every continuous control is a stepper.
Refs: Godot 4.6 movable/floatable docks.

- [ ] Move the map editor to a center tab per `.map` (same shape as tileset)
- [ ] Move the import wizard to a center tab (or a modal) — decide, then do
- [ ] Declare all `ggo_*` keybindings in `assets/keymaps/default-*.json` so the keymap editor sees and rebinds them; keep the reload tripwire test
- [ ] `ui::Slider` primitive; replace steppers for onion opacity, zoom, palSub, preview size
- [ ] Emerald: free numeric cadence field instead of the fixed picker (`MIGRATION.md:179`); drag-to-reorder schedule rows
- [ ] Emerald: undo for schedule edits (or a visible "revert to manifest" that isn't only the error path)
- [ ] Charts: frame-0 ignore-set editor + "N of M excluded" readout (`MIGRATION.md:585`)
- [ ] Charts: split `ggo_charts_panel.rs` (5.5k lines) before adding to it (`MIGRATION.md:529`)

---

## Docs

- [ ] `docs/ggo/MIGRATION.md:131` — tileset row still says "read-only viewer"; it is a full pixel editor since `bc1e852103`..`59214eb6d0`
- [ ] `docs/ggo/MIGRATION.md` — sprite / emu / charts described as dock panels; they are center tabs since `0e935bf412` / `f4cdc1bf91`
- [ ] Add this file's completed items back to `MIGRATION.md`'s ledger as they close

## Not doing (on purpose)

- A scripting language — gameplay is Rust in Zed; Bevy's archived editor shows the alternative isn't cheap
- A Godot-style scene tree — worlds + `[[instance]]` already are the scene format
- 3D anything
