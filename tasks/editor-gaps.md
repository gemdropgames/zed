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

- [x] Inspector: Asset fields keep their stem picker; a missing stem is flagged red (soft refusal — still committed, since you often name the asset before importing it) and a resolving one gets an open jump for any extension via the path-open interceptors (2026-08-24)
- [x] Inspector: inline warning badge + tooltip for a stem that does not resolve (2026-08-24)
- [ ] Render `loader.rs`'s `node["error"]` in the canvas instead of dropping it (`MIGRATION.md:144`)
- [x] Activation: the `file_types` glob ships in `assets/settings/default.json`; the per-repo copies are redundant (2026-08-24)
- [ ] `ggo-lsp` skeleton (`LspAdapter`): diagnostics for dangling stems and unknown components
- [ ] LSP completions: component names, field names, stems, `[[instance]]` world stems
- [ ] LSP code actions named in the spec: snap / center / offset
- [ ] LSP hardware-budget diagnostics (tile/sprite counts per world)

## 3. Emulator debug surfaces — `done 2026-08-25`

Run / Stop / Mute only. GB Studio 4.3's debugger shows live background +
overlay tilemaps. The drive loop already checks a stop flag once per turn
(`emu_panel/src/drive.rs:390`); pause and single-step are the same mechanism.
Refs: GB Studio 4.3 debugger, Mesen/bgb-style VRAM viewers.

- [x] Pause / resume flag in `drive::run`, transport button + keybinding (`ctrl-alt-p`, 2026-08-25)
- [x] Frame advance (`ctrl-alt-.`, 2026-08-25)
- [x] Tilemap viewer per layer with the scroll window outlined (debug column, 2026-08-25)
- [x] OAM composite + list (2026-08-25)
- [x] Palette grid with hover readout (2026-08-25)
- [x] Tile sheet with bank/palette selectors (2026-08-25)
- [x] ~~Integer-scale framebuffer~~ — stale: the pane already paints nearest-neighbour; ledger entry struck (2026-08-25)
- [x] Release the pad mask while the window is inactive (2026-08-25)
- [x] Hidden tab auto-pauses, resumes on return (2026-08-25)
- [x] Runs over `MAX_FRAMES` ingest their first 100k frames with a note (2026-08-25)
- [x] Pane runs flush saves like the standalone (`savefile` moved into the core, 2026-08-25)

## 4. World editor interaction floor — `done 2026-08-25`

Selection is `Option<Selection>` (`world_panel/src/ggo_world_panel.rs:614`).
No rubber-band, shift-click, group move, copy/paste, duplicate, entity list
(`MIGRATION.md:150`), or delete confirm (`MIGRATION.md:153`).
Refs: LDtk, Godot 2D editor.

- [x] Selection set instead of `Option`; shift-click add/remove (2026-08-25)
- [x] Rubber-band select on empty-space drag (2026-08-25)
- [x] Group move as one gesture / one undo entry (`WorldOp::MoveMany`, 2026-08-25)
- [x] Copy / paste / duplicate as world-file TOML on the clipboard (2026-08-25)
- [x] Delete confirm for instance removal (2026-08-25)
- [x] Entity / instance list in the dock; click / shift-click select (keyboard nav not done) (2026-08-25)
- [x] Fix redo-of-add leaving the instance subtree unresolved (2026-08-25)
- [x] Fix the world panel's atlas leak (two-stage retirement + on_release) (2026-08-25)

## 5. Map editor — `done 2026-08-25` (collision / layers / slider deferred)

`MapTool` is exactly Brush / RectFill / Eyedropper / Eraser
(`map_panel/src/ggo_map_panel.rs:433`). No flood fill, autotile, layers, cell
multi-select, or collision flags. Per-tileset `cols` was lost, so metatiles
laid out 6 or 12 wide are no longer rect-selectable as one stamp
(`MIGRATION.md:132`, `:453`) — a regression from ggo-ide.
Refs: Tiled terrain sets + automapping, Pixelorama 1.1 autotile.

- [x] Flood fill tool (`MapOp::Fill`) (2026-08-25)
- [x] Cell multi-select + copy / paste / delete of regions; brush/eraser drags are one undo step (2026-08-25)
- [x] Per-tileset `cols` as a sidecar (`.ggo-ide/<rel>.editor.json`, `ggo_worldlib::sprites::tileset_meta`) read by both panels; the dead `tileset_cols:*` db rows are left as-is, nothing reads them (2026-08-25)
- [x] Autotile terrains: blob-47 (8-neighbour mask), in-panel terrain editor, resolve-on-paint with shift-erase; stored in the same sidecar (2026-08-25)
- [ ] Collision flags per tile (if the `.map`/PPU contract has a bit for it; otherwise an IntGrid-style side layer)
- [ ] Layers — only if the `.map` format grows them; otherwise document that priority lives at the world `[[background]]` level
- [ ] Slider primitive for zoom + palSub (see §10)

## 6. Asset pipeline — `done 2026-08-25`

PNG only (`import_panel:130`), wizard has no undo, no keybindings (`:193`),
no OS drag-and-drop (`:68`), no re-import on change, no thumbnails anywhere,
no in-panel picker anywhere.
Refs: Godot import dock, GB Studio assets folder.

- [x] OS file drag-and-drop anywhere in the editor → import wizard (2026-08-25)
- [x] Import record (source, mtime, crop, settings) in the `.editor.json` sidecar; re-import banner on open (checked on open, no watcher) (2026-08-25)
- [x] Aseprite `.ase`/`.aseprite` reader in worldlib → tileset, or one sprite frame per source frame (2026-08-25)
- [x] Thumbnails for `.til` / `.spr` / `.png` rows in the project panel (2026-08-25)
- [x] Palette surgery in the import preview: swap, move, sort, reset (tileset mode) (2026-08-25)
- [x] Keybindings for the import wizard (2026-08-25)

## 7. Pixel tools — `done 2026-08-25`

Tileset editor tools are Pencil / Eraser / Picker / Select
(`tileset_panel/src/ggo_tileset_panel.rs:196`). Palette is 16 swatches +
per-channel steppers (`palette_widget.rs`).
Refs: Pixelorama, LibreSprite.

- [x] Bucket fill (floods the composed sheet across tiles) (2026-08-25)
- [x] Line, rect, ellipse tools (shift = filled) (2026-08-25)
- [x] Brush size 1–4 px (2026-08-25)
- [x] Move (drag inside the marquee) + flip H/V (2026-08-25)
- [x] Mirror painting, per-tile axes (2026-08-25)
- [x] Delete row / column edge bars (`TilesetOp::DeleteRow/DeleteColumn`) (2026-08-25)
- [x] Magnified single-tile canvas as a focus mode on the sheet (2026-08-25)
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
