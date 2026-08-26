# ggo-ide → fork: feature disposition audit

Date: 2026-08-08 (end of F3), rows amended 2026-08-09 (end of F4: explorer-driven
panel routing + the read-only tileset viewer; then X4, which removed the last
in-panel file picker — the emulator's `.cart` dropdown), 2026-08-10 (end of
F5.3: the emerald mutation/reorder ops, the emd version lock, and the four
carried panel deferrals — onion skin, world view controls, arrow nudge, goto
sprite), and 2026-08-10 (end of **F5.4**: the reports-depth block — KPI tiles,
run detail header, failure tables, stored UART console, history rail,
click-to-inspect, I$ profile table, historic overlays, drag-zoom — plus
emulator audio), and finally 2026-08-10 (**F5.5**, which deleted `ggo-ide`
itself and gave this file its last edit as a migration document — see
"`tools/ggo-ide` no longer exists" below, and the closing section). Fork
branch `ggo`.

What this is: a row for **every** user-facing ggo-ide feature, and what — if
anything — answers it in the fork today. It exists so nobody has to guess
whether a thing was ported, replaced, or simply dropped. It is a reference,
not a status report to be proud of; the honest answer for a large part of the
Assets suite is "not covered".

**Sources of truth.** ggo-ide's surface was enumerated from
`ggo/tools/ggo-ide/src/pages/` + `nav.rs` and from
`ggo/docs/ggo-ide-feature-inventory.md` (the redesign brief, which is the
complete list). **That first path no longer exists on disk** — see the section
below; read it as `main@281fd557^:tools/ggo-ide/src/pages/`. The inventory
doc survives in the ggo repo and is the more useful of the two anyway. The
intended dispositions come from
`ggo/docs/superpowers/specs/2026-08-07-zed-pure-extension-design.md` (its
feature-disposition table) as amended by
`2026-08-07-zed-fork-design.md` (native dock panels, F1-F3 phasing). What
actually shipped was read out of `crates/ggo/*` in this fork, plus the ggo
repo's `.zed/settings.json`, `.zed/tasks.json` and
`scripts/check-zed-config.sh`.

**Status legend.**

| | meaning |
|---|---|
| ✓ | covered in the fork (possibly by a different surface — a task or a CLI counts) |
| partial | the capability exists but is materially smaller than ggo-ide's |
| deferred | intended, not built yet; the rationale says why |
| dropped | deliberately not coming back |

---

## `tools/ggo-ide` no longer exists

**It was deleted from the ggo repo on 2026-08-10, in `281fd557` (PR #90).**
That decision was taken deliberately, as F5.5 Task D2, after Task D1 audited
its 1193 executing tests for coverage that would die with it.

Three things follow, and they are why this section sits above the table
rather than in a footnote:

1. **Every "reference implementation" citation in this fork's docs and code
   comments now resolves into git history, not into a working tree.** A
   reader following one needs `git show`, not `ls`. The ggo repo's existing
   convention for these is the git-qualified `main@<sha>:tools/ggo-ide/…`
   form; anything in this file that names a bare `pages/…` or
   `tools/ggo-ide/…` path means `main@281fd557^:tools/ggo-ide/…` in the ggo
   repo.
2. **Every present-tense sentence about ggo-ide below describes code as it
   stood at `281fd557^`.** They were not rewritten into the past tense — a
   hundred-odd comparison sentences retensed is churn that would make this
   file harder to diff against the phase reports that produced it, and the
   ggo repo made the same call for its own ~90 "ported from ggo-ide"
   provenance citations, which D2 deliberately kept. Read "ggo-ide shows a
   red Failed-to-load banner" as "ggo-ide showed one, at `281fd557^`".
3. **The "dropped" rows and the deferred set are now genuinely unavailable**,
   not merely un-ported. Until F5.5 there was a fallback: the pixel editor,
   the project library and the full-system window still ran if you launched
   ggo-ide. There is no such fallback now. The closing section names exactly
   what that costs.

The ggo workspace went **2314 → 1109** tests across that deletion, a drop of
1205 that accounts exactly: ggo-ide's 1193 executing tests plus the 12 tests
of `ggo_worldlib::emerald::toml_sections`, an orphaned helper deleted in the
same PR. `ggo-worldlib` itself went 675 → 663, entirely those 12. **This
fork lost nothing**: the ten ggo crates are at 703 before and after, because
the fork never depended on ggo-ide — only on `ggo-worldlib`, which survives
and is the whole point of the eight extraction PRs that preceded this.

---

## 1. App shell

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| 8-page nav rail | Dock panels with toggle buttons: GGO World, GGO Sprite, GGO Charts, GGO Emulator (+ file tree, editor, terminal, task picker) | ✓ | |
| Single window, settings DB opened before first paint | Zed window; `~/.ggo/ggo_ide.db` opened lazily and only by the charts/emu panels | ✓ | |
| App-wide active project + persistence + fan-out | Workspace's first visible worktree root | partial | Panels read the open folder. No in-app project switch, no persisted "active project", no per-panel project guard view. |
| Nav side-effects (leave Assets → pause timeline; leave Emulator → pause; enter World → rescan) | Panels re-enumerate on `set_active`; the sprite panel pauses playback on edit | partial | Docks have no page-crossing event. A cart keeps running when the emu dock is collapsed; only focus loss stops input reaching it. |
| Cross-page handoffs (Reports Re-run, World Emulate, inspector "→" sprite, Emulator View in Reports) | emu → charts "View in Reports"; World → Emulator on "Emulate this world" (F5.2/S4); emu → charts on Re-run's ingest-complete (F5.2/S4); world → sprite on the MetaSprite `stem` jump (F5.3/E5) | partial | **All four hops now ship.** Still `partial` for one residual: the World → Emulator hop lands in CART mode, not ggo-ide's full-system boot — see §4's Emulate row and the Known-deferrals entry. |
| Native OS confirm dialogs with per-dialog action binding | none | dropped | The destructive operations that used them (world delete, sprite delete, file delete, project remove) are not in the fork. |
| Window-close unsaved-changes guard naming dirty pages | `ggo_common::prepare_to_close_dirty`, wired through each panel's `Panel::prepare_to_close` | ✓ | |
| Typing guard (single-letter hotkeys suppressed while a text field is live) | gpui focus contexts (`not_editing` predicate on the sprite panel; emu pad keys are focus-scoped) | ✓ | Real focus/blur exists here, so the Iced workaround is unnecessary. **Task 10 (2026-08-25):** every GGO binding is declared in `assets/keymaps/default-{linux,macos}.json` under its panel's context (`GgoMapPanel`, `GgoTilesetPanel`, …) instead of being bound in code, so the keymap editor lists and rebinds them and a reload rebuilds them like any other; panel tests bind the asset through `ggo_common::bind_default_keymap`. Continuous controls (zoom, palSub, ghost opacity) are `ui::Slider`s now, not `-`/`+` steppers. |
| Theme (22 Iced themes, persisted) | Zed themes | ✓ | |
| `capture_lint` source lint | none | dropped | Guards an Iced `ButtonReleased`/`and_capture` bug class that has no gpui analogue. |

## 2. Projects page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Project library (list, add existing, select, remove) | Zed open-folder / recent projects | dropped | The page is superseded by the editor's own project model; the spec's disposition table already called the library and its persistence dropped. |
| Launcher view (branding screen when nothing is open) | Zed's own welcome/recent-projects screen | dropped | Same. |
| "Create Emerald project" (`emd new <name>`) | `emd: new project scaffold` task | partial | Zed's `tasks.json` has no interactive input variable, so the name comes from `GGO_NEW_NAME` or the task picker's edit-and-spawn flow — not a prompt. **As of F5.5 this task is the only project-scaffolding affordance that exists anywhere** (the ggo repo's `.zed/tasks.json` says so in the task's own comment): ggo-ide's Projects page wrapped the same `emd new <name>` call and is gone. The capability survives; scaffolding as a *GUI action* does not. |
| Saved launch-target badges | none | dropped | Display-only data in ggo-ide with no consumer. |

## 3. Assets page (pixel-art suite)

This is the largest gap in the migration and the least ambiguous: per the fork
spec, `ggo_sprite_panel` is "animation creation/editing (clips/timeline)
and tile setting … **not a full pixel editor**". Pixel authoring is not
covered by the fork at all.

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Browser rail: fuzzy filter over all sections, refresh | Zed project panel + file finder; clicking a `.spr`/`.til`/`.cart`/world `.toml` there routes directly into its GGO panel (F4) | partial | Generic file search, not asset-typed sections with extension badges. The in-panel pickers this row used to credit are all gone as of X4 — see the Assets/World/cart-library rows and Known deferrals for what explorer-driven routing does and doesn't cover. |
| Sprite rail ops: New / Duplicate / Rename / Delete `.spr` | Project-panel context menu: **New Sprite…** / **New Metasprite…** on an assets dir, **Duplicate Sprite** + **Rename Sprite…** + **Delete Sprite** on a `.spr`, contributed by `ggo_sprite_panel` (F5.0 + F5.2) | ✓ | All four. Duplicate goes through worldlib `open_sprite`->`save_sprite` from the LIVE document, so the copy gets its own `.til`/`.pal` and carries unsaved edits; Delete confirms, names unsaved edits, and unlinks the `.spr` only (the sidecars are shareable). **New** creates a blank `.spr` bound to a tileset picked in the panel (Sprite seeds one frame, Metasprite seeds frames plus a first clip); the binding is chosen rather than guessed, because a `.spr` — unlike a `.map` — has no legal unbound form (`open_sprite` hard-errors on an unreadable `.til`/`.pal`) and its pool IS that `.til`. That makes New subject to the same pool-sharing hazard Duplicate un-shares to avoid, so the form **defaults to a tileset no sprite owns yet**, labels the ones that are owned (`tiles/x.til (used by y.spr)`) and warns inline when one is picked deliberately — binding to a sprite-owned `.til` is legal and offered, but it makes every later save of either sprite rewrite the other's tiles *and* palette and blocks Dedup / Palette-remap on both. Divergence from ggo-ide, whose New Sprite wrote a PRIVATE trio: that needs a pixel editor to grow the one blank tile it starts with, and the fork has none by design. **Rename** moves the `.spr` only, within its directory: the `.til`/`.pal` rels are assets-root-relative, so they survive untouched, and the sidecars stay shareable. Divergence to note: ggo-ide's duplicate was a raw byte copy that made the two sprites pool-sharing siblings (`pages/assets/sprite.rs:626-631`); ours un-shares, because a shared pool makes `DocOp::Dedup`/`DocOp::PaletteRemap` return `DocError::PoolShared` on BOTH sprites (`sprite_doc.rs:627,:677`) — private sidecars are the only variant where either stays dedupable. The cost is real: `save_sprite` writes the full pool to the copy's `.til`, so duplicating out of a large shared tileset gives the copy a complete private pool, a cart-size multiplier per duplicate. |
| New tileset / new map | `ggo_map_panel`'s **New Map…** (F5.1/M2, on an assets dir) and `ggo_emerald_panel`'s **New Tileset…** (F5.2/S3, see §5) | ✓ | Two different panels/entries, not one — New Tileset is an emerald-panel form (blank `.til`/`.pal`), so it is also listed in §5's table; New Map is `ggo_map_panel`'s own affordance (blank, unbound `.map`). Both create blank assets rather than importing. |
| Files tree (create folder, delete file/dir, extension badges) | Zed project panel | partial | Create/delete/rename exist; the GGO extension badges do not. |
| **Pixel editor**: 14 tools, brush sizes, mirror, pixel-perfect, shapes, marquee/lasso/wand, floating-selection move/flip/rotate, eyedropper, zoom/pan, per-editor undo | none | dropped | Explicit spec scope call. Pixel authoring is an external editor plus an import path. |
| **Palette panel**: RGB565 16-slot editor, draft picker (5/6/5 sliders, 565 + 888 hex, quantized preview), swap / sort / ramp, shared-palette warnings | none | dropped | Part of the pixel-editor scope not taken. |
| **Tile-pool panel** + dedup preview/apply | none | dropped | Same. Dedup still happens implicitly on metasprite save (worldlib's fold-back). |
| Animation timeline: transport, clip lanes (name/from/to/loop/delete/add), frame strip (select, add, duplicate, delete, move, per-frame ms), playback honouring durations + clip range + loop | `ggo_sprite_panel` | ✓ | |
| Onion skin (toggle, back/fwd ghost counts, opacity) | `ggo_sprite_panel` `onion.rs` + a control row under the transport (F5.3/E5) | ✓ | Toggle, back/fwd counts (clamped 0–3), opacity and the `opacity * (1 - (|dist|-1) * 0.3)` falloff are ggo-ide's values; frame selection is worldlib's own `timeline_ops::onion_frames`, not a second copy. Ghosts carry ggo-ide's directional red/blue tint (E5 fast-follow) and paint farthest-first under the live frame. **Three** deliberate divergences (the third was documented in the code but missing from this row until F5.5). (1) Opacity is a `-`/`+` stepper rather than a slider (`ui` has no slider; same range, same 0.05 step, so the reachable values are identical). (2) Ghosts are suppressed while the transport is playing (ggo-ide stacks them under a moving image, which only smears it). (3) **The tint does not track the opacity control.** `onion.rs::tint_strength` scales its falloff by `DEFAULT_OPACITY` (0.5), not by the live `OnionState::opacity` that ggo-ide's `ghost_layers` multiplies in — because the tint is baked into a `RenderImage` cached by `(dist, frame idx)` alone, and keying that cache on the live opacity would rebuild every ghost image on every opacity step. The two match exactly at freshly-opened defaults, and the stepper still governs each ghost's overall on-screen strength through `alpha_at`; what differs is the tint's *relative* strength once you move the stepper off 0.5. Reasoned in place at `sprite_panel/src/onion.rs:64-85`. |
| Preview panel (1:1 live frame, background picker, LCD filter) | sprite panel centre preview | partial | Live composed frame with aspect fit shipped; checker/black/white background picker and the LCD scanline filter did not. |
| Hardware budget meter (4 traffic-light rows + tooltips) | sprite panel `hw_meter_line` | partial | Same four value/cap pairs from worldlib `sprites::hw`, condensed into one header line — no per-row dots or tooltips. |
| Per-cell tile assignment from the pool | sprite panel tile picker + preview cell click | ✓ | F5.2 gave the picker a real source: it composes the BOUND TILESET (the sprite's pool is that `.til` byte for byte) as one sheet via worldlib `unpack_til_to_indices` → `compose_tile_grid` → `indices_to_rgba`, the same three calls `ggo_tileset_panel` makes. |
| Tileset editor (`.til`): overview grid, magnified tile canvas, pencil/fill/eyedropper, palette slot edit, append/duplicate/delete-last, cols setting | `ggo_tileset_panel` — read-only overview grid + 16-slot palette swatch row, integer zoom 1x-8x | partial | Viewing shipped (F4): routed off a `.til` click in the project panel, no in-panel picker. The **magnified tile canvas**, pencil/fill/eyedropper, palette slot editing, append/duplicate/delete-last and the cols setting are all still dropped — the fork's zoom magnifies the whole sheet, not one selected tile, so there is no per-tile editing surface at all. This is a viewer, not the editor ggo-ide had; do not read it as more than that. **Superseded by later tasks (2026-08-25):** the panel is an editor now — pencil/eraser/picker/select, plus **fill** (floods the composed sheet across tile borders), **line / rect / ellipse** (shift = filled, live preview), **brush size** 1–4 px (`[`/`]`), **per-tile mirror** H/V, marquee **move** (drag inside it) and **flip** H/V (`shift-h`/`shift-v`), edge bars that add *or delete* a row/column, palette slot editing, the cols setting (shared sidecar), and a **focus mode** (double-click a tile / `f`) that magnifies one tile with every tool working through the same sheet mapping — the "magnified tile canvas" ggo-ide had, done as a view of the sheet rather than a second surface. Geometry lives in `ggo_worldlib::sprites::pixel_tools`; every gesture reaches the document as one undo step through the existing stroke path. |
| Map editor (`.map`): tileset binding, multi-tile stamp, H/V flip, palSub, brush/rect-fill/eraser/eyedropper, grid, zoom, resize | `ggo_map_panel` (F5.1 M2) — the fork's one editing surface | partial | Maps are **authored here, never imported** (user's call). Ported: tileset binding (`MapOp::BindTileset`, with the bound tileset's tiles as a rect-selectable stamp strip), multi-tile stamp placement, H/V flip and palSub on the stamp, rect-fill, eraser, eyedropper, grid toggle, pan, resize, undo/redo/save with the dirty guard. Opened by clicking a `.map` in the explorer, or created by "New Map…" on an assets directory (blank + **unbound**: cells are pool indices, so a guessed binding is worse than none — you bind from the panel). Not ported at first: the float zoom slider (the fork uses an integer 1x–8x ladder, so 16px tiles never resample) — task 10 (2026-08-25) gave both zoom and palSub a real `ui::Slider` over that ladder. **The real loss is the per-tileset `cols` setting.** ggo-ide resolves the tile-strip column count through the shared `resolve_and_migrate_cols`, keyed on the `.til` in `~/.ggo/ggo_ide.db`, so its map and tileset editors lay a given tileset out identically and the user can set that width. The fork has no db-backed cols and falls back to 8 columns (clamped) in both `ggo_map_panel` and `ggo_tileset_panel` — they still agree with each other, but not with a tileset authored at some other width. Because `cols` **is** the stamp coordinate system (`build_stamp` indexes `row * cols + col`), a tileset whose metatiles were laid out spatially contiguous at, say, 6 or 12 wide has them scattered across the fork's 8-wide reflow: those metatiles are no longer rect-selectable as one stamp, and have to be placed tile by tile. Defensible while the fork has no settings store, but it is a workflow regression for any non-8-wide tileset, not just a cosmetic relayout. **F5.5 sharpened this into a concrete orphan.** The db-backed setting is not merely un-read — the `tileset_cols:*` rows in `~/.ggo/ggo_ide.db` are the *output* of the legacy-sidecar migration two rows up, and with ggo-ide gone **nothing writes them and nothing reads them**: `grep -rn tileset_cols crates/ggo/` returns zero hits, and both `ggo_tileset_panel::loader::grid_cols` and `ggo_map_panel::loader::grid_cols` pass `None` for that chain step. Any width a user ever set is now unreachable data in a live database (one such row exists on this machine today, pointing at a path that no longer exists). Reconnecting it is a small read against a db both panels' sibling panels already open — it is the cheapest of the losses listed in the closing section. **Closed 2026-08-25 (task 5):** `cols` now lives in the per-tileset editor sidecar `<worktree>/.ggo-ide/<rel>.editor.json` (`ggo_worldlib::sprites::tileset_meta`), written by the tileset panel's cols stepper and read by both panels, so the map stamp strip is laid out at the authored width again; the orphaned `tileset_cols:*` db rows are simply left behind (nothing reads them — set the width once in the tileset panel). The same sidecar carries the tileset's **autotile terrains**. The map panel also grew **flood fill** (`MapOp::Fill`, 4-connected on the packed cell), a **Select** tool with copy / paste / delete (in-panel clipboard; paste lands under the cursor, else at the selection's corner), brush/eraser/terrain **drags as one undo entry** (`MapDocStore::begin_stroke`), and a **Terrain** tool: blob-47 terrains (8-neighbour mask, diagonals only with both adjacent edges) edited in-panel — name, 3×3 neighbour pad, "Assign" the stamp's first tile to that mask — and painted by re-resolving the touched cells and their neighbours (`terrain::resolve` → `MapOp::SetCells`; shift-drag erases). Layers and collision flags remain unported. Maps also still *render* in the world panel as backgrounds/tilemaps. **Task 10 (2026-08-25):** the map editor is a **center tab per `.map`** (`ggo_map_panel::MapEditorItem`, the tileset item's shape) rather than a right-dock panel; the tab carries the dirty dot and the close-dirty prompt. |
| PNG import wizard (crop, tileset/sprite/metasprite modes, cell grid, quantized preview, commit, source-PNG cleanup) | `ggo_import_panel` (F5.1 Task I2) — tileset-only | partial | Ported: crop rect + cell-grid overlay, quantized preview, commit to `.til`/`.pal` (assets-root-relative sidecars, overwrite-collision confirm), source-PNG cleanup confirm, "Import as tileset…" on a `.png` in the explorer, opening the result in `ggo_tileset_panel` on commit. **Sprite and Metasprite import modes are NOT ported, by design** — the domain model changed: a sprite is one frame and a metasprite is clips over frames, both assembled from tiles in `ggo_sprite_panel`, not decoded straight from a PNG, so there is no destination for a `.spr` import to land in. `WizardState::sprite_import`/`Mode::{Sprite,Metasprite}` moved into worldlib verbatim (Task I1, dependency-light so splitting the file wasn't worth it) but the panel never calls them. **Task 6 (2026-08-25) widened the pipeline:** sources are `.png`, `.ase` and `.aseprite` (worldlib's own reader — visible layers flattened, raw/linked/zlib cels, all colour depths; tilemap layers refused, blend modes composited as normal), reachable by the explorer entry, the picker, or **dropping the file anywhere in the editor** (`workspace::register_external_drop_interceptor`, the same seam shape as `intercept_path_open`); a multi-frame Aseprite source imported "as sprite" becomes one `.spr` with a frame per source frame. Each commit writes an **import record** (source, mtime, crop, settings) into the written `.til`'s editor sidecar; opening that `.til` compares the source's mtime and offers **Re-import…**, which reopens the wizard with the settings restored (ctrl/cmd-r in the wizard replays them for the current destination). The wizard has keys (enter Import, escape clear crop, ctrl/cmd-o pick, +/- zoom) and **palette surgery** on the quantized preview — click-click swap, move, brightness sort, reset — tileset mode only. `.til`/`.spr`/`.png` rows in the project panel show a 16px **thumbnail** in place of the file icon (`workspace::ggo_thumbnails`, decoded off-thread, mtime-keyed). Not built: live file watching (checked on open), Aseprite tags → clips, an asset-browser dock. **Task 10 (2026-08-25):** the wizard is a **singleton center tab** (`ggo_import_panel::ImportItem`) — the explorer entry, an OS drop, the tileset panel's Re-import and the picker all land in it. |
| Legacy `.meta.json` sidecar import | none | dropped | One-shot migration aid for a format generation that is already past. **Corrected in F5.5**: an earlier draft of the D1 audit called this a capability lost with ggo-ide. It is not. The *reader* is `ggo_worldlib::sprites::io::read_legacy_sidecar` (`io.rs:910`), which survives, which this fork already depends on by path, and which was never ggo-ide's only caller. What died is ~20 lines of glue (`pages/assets/mod.rs::resolve_and_migrate_cols`). No fork crate calls `read_legacy_sidecar` today, so the row stays `dropped` — but it is a wiring job away, not a port, and there is no stranded data: zero `.meta.json` sidecars and no `.ggo-ide/` directory exist. **The real loss is the inverse** — see the map-editor row's `cols` note. |
| Save / dirty marker / save-status / discard guards | Per-panel Save button + dirty dot + `ctrl-s`/`cmd-s` + inline `save failed:` | partial | Per-document save and dirty state shipped; the cross-page discard confirms and the "doc changed during save" race message did not. |
| Draggable splitters (rail / right column) persisted | Zed dock resize | ✓ | |

## 4. World editor page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| World file picker (list + open) | Zed project panel (browse `worlds/**/*.toml`) + click-to-open, routed by `ggo_world_panel::intercept_world_open` into the docked panel | ✓ | Explorer-driven since F4 — the in-panel picker was removed; list+open is now the project panel plus the interceptor-routed open, which also runs `prepare_to_close_dirty` before switching documents. |
| "+ New world" (snake_case validated, overwrite confirm) | Project-panel context menu: **New World…**, contributed by `ggo_emerald_panel` (F5.2/S3) | ✓ | Right-click the project's `assets/` (or anything under it) → New World…; name validated snake_case with the same `valid_item_name` the component/system forms use, written through `emd generate world`. One divergence: a name collision REFUSES ("already exists") rather than offering to overwrite — `emd generate` has no overwrite flag, so there is nothing to confirm into. |
| Delete world (confirm + rescan) | Project-panel context menu: **Delete World**, contributed by `ggo_world_panel` (F5.0) | ✓ | Confirms (naming the world by stem AND file, and saying so when the open document has unsaved edits), unlinks, clears the panel if that world was open, and re-enumerates so `+ Instance` stops offering it. One thing ggo-ide didn't do either: `[[instance]]` references to the deleted world are not chased down, so they now dangle — see the Known-deferrals note about `loader.rs`'s unrendered `node["error"]`. |
| Canvas rendering: sprites, metasprites (per-clip), tilemaps, text, rect fills, transform markers, instance gizmos, merged backgrounds, error placeholders, selection outline | world panel `canvas` + `loader` (worldlib compose / `build_draw_list`) | ✓ | |
| Click-select + drag-move with one undo entry per gesture | ✓ | ✓ | One deliberate divergence: empty-space left-drag deselects instead of panning; pan is middle-drag. |
| Wheel zoom / pan | ✓ (cursor-anchored zoom, middle-drag pan) | ✓ | Zoom is cursor-anchored here, which ggo-ide's is not. |
| Snap-to-tile checkbox | ✓ | ✓ | |
| Grid checkbox, "Reset" view, preview-size stepper (`- Preview Nx +`) | world panel `render_view_controls` (F5.3/E5) | ✓ | ggo-ide's two rows merged into one (the dock is narrower), with `Snap` moved next to `Grid` as it sits there. Grid defaults on and draws on 16 world-px lines in ggo-ide's own grey; the stepper is ggo-ide's `step_scale` verbatim (1–4, default 2). One divergence in **Reset**: ggo-ide re-frames against a hardcoded canvas size, this sets pan to "never laid out" so the next paint re-runs the same initial centering a world-open does — same intent, one copy of the framing rule instead of two. At 2× the exact multiple only shows once the dock is widened (the canvas is capped to the dock). |
| Sidebar entity/instance lists | none (selection is canvas-driven; inspector shows the selection) | partial | No list-based selection or navigation; a hard-to-click entity has no list fallback. |
| "+ Entity" (default Transform at view centre) | Add-entity toolbar button | ✓ | |
| "+ Merge" fuzzy world search → add instance | "+ Instance" dropdown over cycle-guarded `merge_candidates` | partial | Instance add shipped, including the cycle guard and immediate subtree resolve; the fuzzy-search picker UI became a plain dropdown. |
| Delete entity / remove instance | `Delete selected` button + `delete`/`backspace` | partial | ggo-ide confirms before removing an instance; the fork does not confirm anything. |
| Inspector: schema-driven typed fields (int/fixed/str/bool/vec2), asset dropdowns, MetaSprite clip dropdown, per-component Remove, "+ Add component…" | world panel `inspector` | partial | Commit on Enter **and** on blur — strictly better than ggo-ide's Enter-only. **But the asset dropdowns are not ported, and F5.5 found this is a correctness regression, not a cosmetic one.** The fork treats `FieldKind::Asset(_)` exactly like `Str` — verified at `world_panel/src/inspector.rs:113` (display) and `:159` (commit), where both arms are literally `Some(FieldKind::Str) \| Some(FieldKind::Asset(_))`. ggo-ide rendered an `Asset(ext)` field as a `pick_list` over the stems that actually exist, so only a real asset could be committed. Here it is a free-text editor: a `Sprite.stem` can be typed to point at a `.spr` that does not exist, the value lands in the world doc, and nothing complains until load time — where §4's own "dangling `[[instance]]` refs are invisible" ledger entry says the resulting `node["error"]` is never rendered either. Two unvalidated-reference paths, one unrendered error. Downgraded ✓ → partial here rather than left as a footnote. |
| MetaSprite `stem` "→" goto sprite on Assets | Inspector jump button beside the MetaSprite component's Remove, into `ggo_sprite_panel` (F5.3/E5) | ✓ | Resolves `{stem}.spr` under the OPEN DOCUMENT's asset root and re-relativizes into the worktree, since every explorer-driven open takes a worktree-relative path. The button exists only when a destination does: an unauthored stem gets no button rather than a disabled one or a sprite panel parked in an error state. |
| Background merging from instances (priority, first-claimant, drawn at origin) | ✓ (worldlib `backgrounds::MergedBackground`) | ✓ | |
| Undo / redo / dirty / Save (`WorldDocStore` + `write_world`) | ✓ | ✓ | |
| "Emulate" (save, full-system build with this boot world, jump to Emulator) | Project-panel context menu: **Emulate this world (cart)**, contributed by `ggo_emu_panel` (F5.2/S4) | partial | Save-if-dirty → `emd pack-ggo --world <stem>` → run in the emu panel, reusing its existing run path (dock focused). CART mode, not ggo-ide's full-system boot (`emubuild::build_full_system_with_world`, GemOS image, FAT card) — see the Known-deferrals entry below for the three concrete losses (no OS boot, different perf profile, no run-kind column yet). **Watch mode (task 8, 2026-08-25):** a **Watch** toggle beside Run re-packs and restarts the last emulated world whenever anything under the project changes (the project's own worktree events, so external tools count; the pack output dir and `.ggo-ide/` are ignored; 300 ms debounce). A restart is not a finished run: no report hop, the held pad is kept (not cleared) and re-published to the new session, mute/debug/console stay, a paused run resumes. One pack at a time — saves during a pack queue one more; the watch goes through the same save-if-dirty path as Emulate; opening a plain `.cart` ends it. Mid-run asset hot-swap inside `ggo-emu-core` is not built. |
| Arrow-key nudge (1 px / 16 px with Shift) | Eight panel-scoped actions on `left/right/up/down` + `shift-…` (F5.3/E5) | ✓ | Delta comes from worldlib's `drag_ops::nudge_delta` (the same function ggo-ide routes its key names through), snap applies to the result exactly as the drag applies it, and the move is the same `WorldOp`. Panel-focused only, so an inspector field editor keeps the arrows for cursor movement. **Better than ggo-ide here**: a run of nudges shares one gesture id, so it coalesces into a single undo entry — ggo-ide pushes one per keypress and buries the pre-nudge position. |
| Confirm dialogs (delete world, overwrite, remove instance, remove Transform from a visual entity) | none | dropped | See §1 — no confirm system in the fork. |

## 5. Emerald page (ECS manifests)

The pure-extension spec expected "full for ops; no dashboard view". F5.2/S3
landed the *creation* half of the ops surface as `ggo_emerald_panel` (right
dock, "GGO Emerald"); F5.3 landed the rest of the ops — remove, field
add/remove, the schedule run-list editor and the version lock — plus a
three-tab browser of the manifests the ops act on. What is still missing is
the *dashboard*: the browser lists names, fields and each system's schedules,
but `emerald.toml` itself is still read as text.

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Project tab: structured `emerald.toml` viewer | Open `emerald.toml` in the editor | partial | Text, not a structured section/entry view. **F5.5 note for whoever builds the structured view later**: there used to be a worldlib helper shaped exactly for it — `ggo_worldlib::emerald::toml_sections`, with `TomlSection`, `TomlEntry`, `entries_of`, `display_of` and `ROOT_SECTION_NAME`. It was **deleted** in `281fd557` alongside ggo-ide (its only consumer), with its 12 tests. That was the right call — dead code carrying green tests reads as covered when nothing exercises it — but it means this row's substitute (the editor) is the *shipped* answer, not a placeholder in front of a waiting helper. A structured viewer starts from `281fd557^`, not from worldlib's current API. |
| Components / Systems / Schedules browsing (module groups, list → detail) | emerald panel three-tab browser (F5.3/E2): rows grouped by module (shared bucket first), a detail pane per selection — a component's field list, a system's "in update, render" line, a schedule's ordered run list | partial | The dashboard shipped, but smaller than ggo-ide's: no `[scene]` tag, no per-row field/system counts, and **no distinct load states** — `read_manifest` treats an unreadable or malformed manifest as an ABSENT one, where ggo-ide shows a red "Failed to load: …" naming the file (see Known deferrals). `emerald.toml` itself is still read as text in the editor (row above). |
| Create component/system/schedule (validated forms → `emd generate …`) | emerald panel forms, opened from the project panel's context menu | ✓ | Right-click the project root or `manifests/` → New Component… / New System… / New Schedule…; the form also generates resources, modules and worlds via its kind selector. Validated with worldlib's own `valid_item_name`/`valid_field_spec` before spawning, so an invalid name never reaches `emd`, and a component's PascalCase "stored as" preview is shown exactly where ggo-ide showed it. A new component refreshes the world panel's inspector schemas in place; a new world opens in the world panel. **Resource and Module have the same form** (reachable via the kind selector inside any of the three menu entries) **but no menu entry of their own** — a six-entry directory menu appended to upstream's own Duplicate/Rename/Delete was judged worse than a selector (see the contributor's own doc comment). |
| New tileset (blank `.til`/`.pal` pair) | emerald panel form, on any assets directory | ✓ | Not a ggo-ide feature and not an `emd` verb — added here because a `.til` is a prerequisite for New Sprite / a bound `.map`, and the import panel needs a source PNG. The palette is worldlib's own `.pal`-less fallback, read back rather than restated. |
| Remove component/system/schedule, add/remove field (with cascade-aware and compiler-check confirms) | emerald panel: per-row trash + per-field trash + "+ Field" row, each through a confirm, then worldlib's own argv (F5.3/E2) | ✓ | All four ops, and the "Reverted" compiler-rollback case is surfaced as its own run state — a revert reads differently from a failure, because nothing changed on disk. Confirms: a **system** remove names every schedule that references it (exact — `schedules_using_system`, cadence suffixes included, and a run list is the only place a system is named). A **component** remove names the worlds that still place it — **more than ggo-ide, which confirmed a component remove with no cascade at all** — but with a stated limit that is printed in the prompt, not just documented: **Rust code that names the component is not scanned**, and neither is any world outside `<assets>/worlds`. Finding code references properly means compiling (which `emd rm` then does), and a grep-shaped guess that says "3 files" where the compiler says 11 is worse than admitting the limit. Field removal carries ggo-ide's "this runs a compiler check and can take 30 s or more" warning, and every run is bounded by a 600 s `EMD_TIMEOUT` ggo-ide had too. |
| Schedule ordered run-list editor (reorder, cadence, add/remove, optimistic commit) | emerald panel Schedules detail pane (F5.3/E3): numbered rows, Up/Down, trash, "+ System" picker, per-row cadence, optimistic commit through `emd schedule set` | partial | Everything but the cadence input. **Cadence is a fixed picker (1, 2, 3, 4, 6, 8, 12, 16), not ggo-ide's free numeric field**, so a run list that already carries `@5` DISPLAYS it (the picker's label is the current value) but cannot have it re-selected once changed — the trade is that an invalid cadence is unreachable rather than merely rejected. Dangling refs to deleted systems are marked and still removable, as in ggo-ide. The rollback is a **re-read of the manifest**, not a remembered vector, so it shows what `emd` actually left on disk, with a visible "Edit not applied" notice ggo-ide's silent revert never had. One honesty note on the confirms: `emd schedule set` runs **no compiler check**, so this editor's prompts don't promise one — only the removes and field ops do, because only they compile. |
| emd version-lock banner + gating + mtime poll + mid-run drift check | emerald panel `lock.rs` (F5.3/E4): banner above the form, every mutating control gated, background mtime poll, post-run trailer re-verify | ✓ | All four mismatch phrasings come from `ggo_worldlib::emerald`'s `EmdError` verbatim (asserted by equality, not by copied literals), with one fork-local sentence appended where the remedy is "where is emd" — `GGO_EMD` or `PATH` — and not on a CLI-version drift, whose own line already says what to update. The gate lives on `start_run`, so every `emd` this panel spawns passes it, and `CliNew`/`Unchecked` gate exactly as hard as `CliOld` (ggo-ide's rule, kept). Divergences: the poll is **30 s, not 5** (the post-run trailer check is the real safety net; the poll only decides how fast the banner catches up), there is no **Re-check button** (the poll plus the panel's own refresh cover it), and `Unchecked` is not silent — ggo-ide disabled the buttons and explained nothing, this says "Checking the emd version…". |
| emd run console panel (live merged stdout/stderr, Cancel) | Zed terminal panel | ✓ | |

## 6. Emulator page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Embedded 320×240 screen, integer scale, latest-wins frames | `ggo_emu_panel` live video (`RenderImage` per frame, bounded depth-1 channel) | partial | Scaling is gpui's `img()` fit, i.e. linear — not the guaranteed integer nearest-neighbour ggo-ide hard-coded. See Known deferrals. |
| "Build & run project" (full system: OS + FAT image + game) | `emd: run` task (launches the standalone `ggo-emu`); `ggo-emu --full-system CARD_DIR` for the OS boot itself | partial | The panel runs `.cart` files only. The full-system build+boot path stays external — and so, therefore, does full-system *audio*: F5.4/R6 gave the panel a stream, but only on the cart path (see the audio row). **F5.5 verified what "external" now means, because an earlier draft overstated it.** Full-system boot did **not** die with ggo-ide: `build_full_system` was ~30 lines of glue over `ggo_emu_core::fullsystem::{os_image::build_os_image, card::build_fat_image}`, both of which survive, and `ggo-emu --full-system CARD_DIR` (plus `--no-cart` / `--os-only`) still drives them. `emd pack-ggo` → `ggo-emu --full-system <card dir>` reproduces the build-and-run end to end. **What is genuinely gone is the interactive, windowed full-system session**: `native::Display` is wired only to `ggo-emu`'s cart path, so the surviving full-system entry point is *headless* — a boot plus a perf capture to an instruction/frame budget, not a playable window. Nothing anywhere renders a full-system frame to a screen you can sit in front of. One further narrowing: `run_full_system` prints its profile report but never calls `report::write_db`, so profiled db rows are cart-mode only from here. |
| Run / Stop | ✓ (`ctrl-alt-r` / `ctrl-alt-s`; Run over a running cart restarts cleanly) | ✓ | |
| Cart library (`~/.ggo/ggo-ide/carts`: list, Upload with magic-header validation, Remove) | Zed project panel — clicking a `.cart` there selects it in the emu panel, routed by `ggo_emu_panel::intercept_cart_open` | partial | Browsing instead of a managed library — no upload, no delete, no header validation (a bad cart fails loudly at Run, deliberately). Explorer-driven since F4 X4: the panel's own `.cart` dropdown and its filesystem walk are gone, so this row is now the project panel plus the interceptor. A click **selects** the cart; Run stays an explicit user action, because starting a cart spawns an emulator thread and takes over the keyboard. Clicking a different cart mid-run stops the running one through the normal `finish_run` path, so its perf data still reaches `ggo_ide.db`. |
| Keyboard → gamepad (18-button cart map) | ✓ (18-bit level-triggered mask, 10 of 17 keys pinned key-by-key against the standalone binary) | partial | Only the cart map exists (`emu_panel/src/input.rs`); ggo-ide's second full-system `FS_BTN_*` map has no fork equivalent, since the panel has no full-system mode. Focus-scoped, so pad keys never leak into an editor. **F5.5 measured the net loss precisely, and it is smaller than it first looked.** Cart-mode keyboard input survives *twice* — `emu_panel/src/input.rs:30` here and `tools/ggo-emu/src/native.rs:282` (winit) in the ggo repo — so playing a cart with a keyboard is not at risk. But `pages/emulator.rs:1266-1286` was the **only** place in any codebase that mapped keys onto `firmware/ggo-hal/src/dev_input.rs`'s hardware button layout, and it is gone. **Net: you can no longer press UP/DOWN in the GemOS launcher by hand.** The only remaining driver of that register is `--auto-input`'s scripted F1 pulse (`fullsystem/bus.rs:769`) and an unused wasm export. Re-adding it is a keymap onto a register that still exists, not a lost mechanism — but until someone does, the OS menu is only reachable by script. |
| Audio + Mute/Unmute + underrun counter | `ggo_emu_panel` audio out (F5.4/R6): the panel owns a `cpal` output stream, Mute/Unmute on `ctrl-alt-m`, dropout count in the stats row | partial | **The panel owns the stream, but only for the length of a run** — it is opened on the emu thread inside `drive::run`, after the cart parses, and dropped before the perf JSON is serialised, so a slow ingest cannot charge dropouts against it (`emu_panel/src/audio.rs`, `drive.rs:354-384`). Three honest limits. (1) **Cart mode only** — audio is wired through `EmuPanel::run` → `drive::start`, and the panel has no full-system mode (row above), so `emd: run` / standalone `ggo-emu` remains the only with-audio *full-system* path. (2) **The counter counts dropout EVENTS, not starved samples or frames** — one `fetch_add` per short callback however many stereo pairs came up empty, which is the diagnostic "did the ring run dry this callback", not a duration (pinned by `one_short_callback_counts_exactly_one_dropout_however_many_pairs_were_empty`). (3) **Linux/ALSA only in practice** — the COM-apartment reasoning for opening on the emu thread is written down, but no Windows/CoreAudio run has happened. A machine with no device is not an error: `start_output` is infallible, records `Unavailable(reason)` with cpal's verbatim text, prints `[audio unavailable] …` to the console and runs silent. No volume control (mute is a submission gate that clears the queue). |
| Stats line (fps / drops / step+blit ms) | ✓ | ✓ | |
| Live guest UART console (collapsible, N lines, 2000-line ring) | ✓ (plus cart `log()` output, via ggo PR #75's `Peripherals::log_sink`) | ✓ | |
| End-of-run perf ingest into `ggo_ide.db` (+ status line, + "View in Reports") | ✓, natively — Stop / cart exit / fault / restart all funnel through one `finish_run` | ✓ | Subsumes the spec's CLI-chained `ggo-emu --perf` → `ggo-cli ingest` task. |
| Implicit pause on navigating away, resume on return | `EmulatorItem::deactivated` → `EmuPanel::auto_pause`; the next render resumes (2026-08-25) | ✓ | A hidden emulator tab parks the cart at its next frame; only a visible item renders, so coming back is what resumes it. A pause the user made is never auto-resumed. |
| Reports "Re-run" entry point | Re-run button in the charts panel's run-detail header (F5.4/R3), through `ggo_common`'s `CartRunner` registry into `ggo_emu_panel::rerun` | ✓ | The last of the cross-panel hops. It routes to the emu panel's **existing** run path (`stop` then `run`, the same entry S4's context menu uses), not a second one, via a registry in `ggo_common` so the charts panel never depends on the emu crate. Two named-reason disabled states rather than a dead button: `NO_RERUN_PATH` when the run's `label` carries no project-relative cart path (so `ggo-emu`, `ggo-server` and ggo-ide-ingested runs cannot be re-run — ggo-ide gates its own button the same way, on `rerun::matches_stored`), and `NO_CART_RUNNER` when no emu panel has registered. Different surface from ggo-ide's, whose button sits at the cart level; that the cart level is collapsed away is counted once, in §7's drill-down row. |
| Build error surfaced in a scrollable read-only editor | Terminal output of the `emd:` tasks | ✓ | |

## 7. Reports page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Carts → Runs → Detail drill-down with slim back headers | `ggo_charts_panel`: flat run picker (cart · label · started_at) → detail with a Back button | partial | The cart level is collapsed away; there is no per-cart grouping or last-run relative time. |
| Per-cart "Re-run" | Project-panel context menu: **Re-run (perf)** on a `.cart`, contributed by `ggo_emu_panel` (F5.2/S4) | ✓ | Different surface than ggo-ide's (a context-menu entry on the cart file, not a button inside the Reports page), but the capability is complete end to end: re-runs the cart through the panel's own run path and, once perf ingest lands, focuses the charts panel and opens that run — the reverse hop of the "View in Reports" one §1's cross-page-handoffs row already credited. |
| Run detail header + config line (budget, wire-model constants, wire-wait tag) | Detail header with Back, Re-run, title, ignore caption and a config line (F5.4/R2) | ✓ | The config line is `ggo_worldlib::charts::reports::kpi::run_config_line` **verbatim** — the same function ggo-ide's page calls, not a re-implementation — so the budget, scanout/refill/writeback constants and the idealized / calibrated / pessimistic-2x wire-wait tag are the same strings in both. A device run (no wire model) gets `hardware counters (no wire model)` instead of fabricated constants. Absent only when the run row itself is missing. |
| "Copy for agent" text export | none | deferred | Same list. One of the two feature stragglers left after F5.4 (the other is §10's version/environment row). |
| Failed-asset-loads table, Panics table | Both tables in the run detail (F5.4/R2), rendered first so a frameless panicking run still shows them | ✓ | Empty states are two different facts, not one: `none recorded` when the run captured UART and nothing matched, versus a deliberately hedged line for a run with no UART at all — `"no UART lines captured for this run (none emitted, a diag run, or a run predating UART persistence)"`. A test forbids narrowing that to any single cause, because the data cannot distinguish them. |
| Guest UART console for a stored run | `Console — guest UART` in the run detail (F5.4/R3), from `perf_db::run_uart` | ✓ | Scrollable (`uniform_list`, virtualized) and monospace (`buffer_font`). Every stored line — the 2000-line ring is producer-side, in the emu panel, so the reader imposes no second cap. The emu panel's *live* console is a separate surface and stays as-is; the two share the row renderer, not the state. |
| Ignored-frames editor (chips, comma/space input, "N of M excluded") | Default frame-0 ignore + a muted `"N frame(s) ignored"` caption | partial | Parity of the *filter* shipped; the editor did not — an ignore set is a UI task, not a port. **After F5.4 this is the reports page's one remaining feature gap vs ggo-ide** (everything else on this table is ✓ or a stated divergence). |
| KPI tile rows (Frames, Over-budget, Avg wire vs budget, IPC, I$/D$ hit rate, max misses, conditional PPU/APU tiles) | Wrapping tile row above the plots (F5.4/R2), derived by `ggo_worldlib::charts::reports::kpi` | ✓ | Eight unconditional tiles in ggo-ide's order, plus five conditional ones on ggo-ide's own thresholds. **Read an absent conditional tile carefully.** Peak sprites/scanline, VRAM uploads/frame and APU underruns hide at exactly zero, so absence there does mean "never measured" and a measured zero is never rendered as a `0`. The two working-set tiles hide unless the max exceeds `kpi::TILE_CACHE_TILES` (**64**), so **absence means "at or below cache capacity, *or* never measured", and this surface cannot tell you which** — a real 40-tile working set renders nothing. That is faithful to ggo-ide (`RunPage.tsx:365/373`) and deliberately unchanged; a qualifier that separates the two cases is a later call. The whole row is suppressed when no frames survive the ignore filter. |
| Chart set (wire vs budget, wire breakdown, cache misses, syscalls, tile working set, I$ misses + evictions by function, i/d-miss histograms, PPU evictions, tile-load wire, APU fetch wire, instructions) | ✓ — `chart_set::build_charts` mirrors `reports.rs::charts_section`, same order and same gating | ✓ | |
| Historic overlay (up to 5 prior runs, age-faded) | Historic toggle above the plots (F5.4/R5); prior runs picked by `charts::reports::historic`, drawn by `chart_geom` | ✓ | Same four charts ggo-ide overlays (wire cycles, cache misses, tile working set, instructions), same picker (`pick_prior_ids`: same `cart_id`, lower `run.id`, descending, capped at `HISTORIC_OPACITY.len()` = 5), same age ramp `[0.5, 0.36, 0.26, 0.18, 0.12]`. Overlays **contribute to the y-scale** — the fold and the polyline loop read one `drawn_overlays` list, specifically so scale and paint cannot disagree. Three things to know. **Alignment is by index position, not by frame number** (ggo-ide's contract, kept): a shorter prior run stops early rather than dropping to zero, and a prior run with fewer than 2 points on the axis is dropped from both scale and paint. `pick_prior_ids` assumes run ids are allocated in **ingest** order (they are — an `INTEGER PRIMARY KEY` rowid), so "prior" means ingested earlier, **not** captured earlier. And there is still **no run-kind column**, so a cart's cart-mode and full-system runs can be overlaid on one chart with nothing marking which is which (see the Known-deferrals entry). Two deliberate divergences: the toggle is *disabled* with a reason at zero priors where ggo-ide renders a live checkbox next to "(0 prior runs)", and the overlay loads with the run rather than lazily behind the toggle — costing up to six extra queries per selection, paid whether or not the toggle is used. |
| Click-to-inspect frame → per-function misses/evicted table | Click a frame on a selectable chart (F5.4/R4) → a caller/callee misses+evicted pane | ✓ | Hit-tested by `chart_geom::frame_at` under the *current* zoom view, so the pane and the plot cannot disagree about which frame was clicked. **The pane renders directly beneath the chart that opened it, not in ggo-ide's one fixed slot** — deliberate, because a fixed mid-list slot is usually scrolled off-screen from the click in a 360 px dock; the consequence is that clicking the same frame number on a *different* chart moves the pane rather than closing it (ggo-ide's `pickFrame` toggles on frame alone, because its slot never moves). The empty state names a state and never a cause: `no per-function rows recorded for this frame`, asserted character for character, where ggo-ide claims `"No I$ misses recorded this frame."` |
| I$ profile table with sortable header | Profile table last in the detail (F5.4/R4), `misses` header toggles direction | ✓ | Same single sortable column ggo-ide has (one `profile_sort_ascending` bool there too), with the arrow doubling as affordance and readout; both orders are precomputed off-thread by `charts::reports::profile::aggregate_profile_sorted`, so a click sorts nothing at click time. Three distinct empty states where ggo-ide has one, and the split is more truthful than ggo-ide's: it keys its fallback off the *ignore-filtered* rows, so a run whose only rows are on frame 0 gets told it has no profile data at all. **Worth knowing: the emu panel never writes profile rows** — `drive` passes `None` for both dumps — so this table is empty for every panel-produced run; every profile row in `ggo_ide.db` came from a native `ggo-emu --profile` ingest, and the empty state says so. That is equally true of ggo-ide, which reads the same table. **F5.5 checked whether this table and the two per-function charts became a dead feature when ggo-ide went, and they did not.** An earlier draft of the audit claimed ggo-ide's `cart_elf_bytes` was the only *producer* of `profile`/`dprofile` rows, which would have left this surface rendering something nothing could generate. That is **disproved**: `ggo-emu <cart> --profile <elf>` loads the ELF symbols (`tools/ggo-emu/src/lib.rs:241`) and `report::write_db` persists both dumps **by default** (`lib.rs:522-544`), into `cfg.perf_db.or_else(report::default_db_path)`. `cart_elf_bytes` was best-effort *auto-discovery* of the companion ELF from `emerald.toml` — convenience, so the GUI could ship a profiled build without the user typing `--profile`. Convenience is what was lost. This panel's own `inspect.rs:64` already documented the surviving producer correctly. |
| Reads `~/.ggo/ggo_ide.db` off-thread, stale-response-guarded | ✓ — the same DB file, not a copy; load-generation guards on both the list and the detail | ✓ | |

## 8. Device page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Serial port picker (scan `/dev/serial/by-id`, auto-select a lone port, refresh) | `GGO_TTY` env var read by the tasks | partial | No discovery UI; you set the variable or accept `/dev/ttyUSB0`. |
| Flash `.ggo` via `ggo-diag --provision --launch` (file picker + magic check, Full-run/Skip-PnR toggle, collect seconds, baud) | **Flash-and-run button** in the emulator transport and the world panel toolbar (task 9, 2026-08-25); the `ulx3s: flash bitstream (fujprog)` task remains for a bare bitstream | partial | The button runs `ggo-diag --project <open project> --tty <port> --skip-pnr`: pack with `emd pack-ggo`, write the flash-backed sd-emu card image (GemOS + assets + game), flash the cached bitstream with `fujprog`, boot-verify over UART, and record the run in `~/.ggo/diag.db` — the rows the charts panel's device rail already lists. Output **streams** (`ggo_common::run_streaming_async`, cancellable by dropping the task — pressing the button again cancels): raw lines to the console pane, and lines matching `ggo-diag`'s own event grammar (`==> phase`, `--> component`, `[boot]`, `diag step N:`, `RESULT:`) drive the status row, so a `RESULT: FAIL` is an error even on a zero exit. Still `partial` against ggo-ide's Device page: no file picker (the open project is the subject), no Full-run/Skip-PnR toggle (always `--skip-pnr` — a full SoC place-and-route is ~20 min and a game change never needs new gateware), and no collect-seconds/baud form (the CLI's defaults). **Prerequisites open a page, not an error line**: flashing with anything missing opens a **GGO Hardware** center tab listing every requirement (game project, GGO repo, `ggo-diag`, `emd`, board) as found-with-its-path or missing-with-its-remedy, with **Install tools** for the three ZedGG can fix, **Flash now** once they are, and **Re-check** to re-probe; the board and the `dialout` group are shown as instructions since neither can be installed. Install output streams into the page and the rows flip as they land. The installable ones clone the repo (`~/.ggo/ggo` unless `GGO_REPO` says otherwise) and `cargo install`s the binaries — from a local checkout when one exists, else from git — streaming into the same console. The OSS CAD Suite (yosys / nextpnr / ecppack / **fujprog**) is not a step: `ggo-diag`'s own `toolchain::ensure` downloads it on first run. A missing board is reported, never worked around. **Not** cart-to-QSPI: `ggo-flash` is still a stub (`exit(2)`), so the game reaches the board through the card image. |
| "Run hardware diagnostics" (built-in diagnostic cart, no project needed) | Project-panel context menu: **Run hardware diagnostics**, contributed by `ggo_emu_panel` (F5.2/S4) | partial | `ggo-diag --tty <port> --skip-pnr --launch`, gated to a right-click on a directory (any directory, not tied to an emerald project — the spec's "no project needed"). Needs a GGO repo checkout (`GGO_REPO`) and a board; missing either names exactly what's missing on the status row and spawns nothing. One-shot, not streamed, no cancel, no baud/collect-seconds/port picker — ggo-ide's Device page is the model for all three, unbuilt here. |
| Live run log with stick-to-bottom autoscroll, Cancel, PASS/FAIL/TIMEOUT verdict | Terminal panel output; Ctrl-C | partial | Raw stdout with no verdict parsing and no state machine. |
| History rail (clone `~/.ggo/diag.db`, 50 recent runs, per-run log viewer) | Device-run rail in the charts panel (F5.4/R3): clones `~/.ggo/diag.db` into `ggo_ide.db`, lists 50 recent runs, selection drives the panel, per-run log viewer | ✓ | Clones rather than reading live, for ggo-ide's own reason (it opens `diag.db` read-write to migrate, and two writers on one file is the failure it was avoiding). ggo PR #88 made that clone **reconcile** instead of copy-once, which fixed two permanent data-corruption paths and self-heals databases already corrupted by the old shape. `HISTORY_LIMIT = 50`, ordered `started_at DESC, id DESC`. Three named absent states rather than a blank or a panic — no `diag.db`, a `diag.db` with no runs, and a pre-migration `diag.db` — and they render *under* any rows already loaded, not instead of them. Two limits to read honestly: rows carry only `started_at`, `state` and `verdict`, so **runs are listed undifferentiated** — the `runs` table still has no run-kind column, so nothing marks a cart-mode capture apart from a full-system one (as in ggo-ide); and the rail lives in the charts panel, not on a Device page, because §8's flash/diagnostics surface is not here. |
| UART monitor | `ulx3s: UART monitor` task (mirrors `fpga-test`'s `configure_tty`, documents the FT231X re-enum gap) | ✓ | |

## 9. Shared chart widgets

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Line / StackedArea / Histogram with nice-step gridlines, compact tick labels, axis captions | `chart_geom` (renderer-independent `ChartScene`) + `chart_paint` (gpui) | ✓ | |
| Hover crosshair + flipping tooltip | ✓ | ✓ | |
| Dashed budget line participating in the y-scale | ✓ | ✓ | |
| Click-drag x-zoom + double-click reset | ✓ (F5.4/R5): one `resolve_gesture` resolver — double-click resets, a drag of `DRAG_MIN_PX` (3 px) or more zooms, anything shorter falls through to frame selection | ✓ | Line charts only, which is ggo-ide's own scope (`stacked.rs`/`histogram.rs` both say "no zoom, no drag") and not a fork shrinkage. `zoom_domain` re-reads through the *current* scale so drags compound, clamps inside the full domain and enforces a `MIN_ZOOM_DOMAIN_WIDTH` so a zoom-to-nothing is unreachable. Zoom is keyed by chart index and cleared on selection change, since an index means nothing across chart sets. |
| Click-to-select a frame (drives the inspect panel) | ✓ (F5.4/R4) — `ChartSpec::selectable`, hit-tested by `chart_geom::frame_at` | ✓ | Exactly the four charts `RunPage.tsx` passes `onSelect` to: cache misses, tile working set, and the two per-function charts — pinned by a test that names all four. Ties resolve to the earlier frame; histograms are never selectable; a keyboard-dispatched click with no cursor selects nothing. |
| Historic overlays (grey, age-ramped opacity, contribute to y-scale) | ✓ (F5.4/R5) — one implementation, `chart_geom::drawn_overlays`, read by both the y-scale fold and the polyline loop | ✓ | Grey is `palette.text.opacity(age)` (theme text dimmed, not a hardcoded grey, so it follows light/dark). Painted **before** the live series so the current run draws over its history, and x-clipped to the visible zoom domain — a deliberate divergence from ggo-ide's `line.rs`, reasoned in place. Alignment, picker and run-kind caveats: §7's historic-overlay row. |
| Cached static layer, cheap hover redraws | Per-chart scene memo (F5.4/R5): `CachedScene { spec, size, view, scene }`, invalidated on `Arc::ptr_eq(spec)` / size / zoom-view change | partial | A hover move no longer rebuilds every chart — but it still rebuilds **one**, because the hovered chart's spec is what carries the readout. Measured at the 100k-frame cap in a debug build: ~11.8 ms per memoized hover move, at or past a frame budget once the rest of the frame is counted; the `Arc` clone share of that is 0.4%, so the residual is a single chart's `O(frames)` rebuild and the real fix is decimating by index **before** `plot_points` materialises 100k points that `envelope` immediately cuts to 2048. Cache is a field of the panel entity, so it is per panel, not per window (they coincide today — `init` creates one panel per workspace). |

## 10. Settings page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Theme | Zed settings | ✓ | |
| Repository path | `GGO_PROJECT_DIR` env (tasks) / the open worktree | partial | No stored setting; the tasks default to `..` and the panels use the worktree root. |
| Serial device (tty) + scanned-port dropdown | `GGO_TTY` env | partial | Free-form env var, no scan, no picker. |
| Baud rate (validated positive integer) | `GGO_BAUD` env | partial | No validation; a bad value fails at `stty`. |
| `emd` binary path (blank = resolve from PATH) | PATH | partial | The override is gone; `emd` must be on PATH. |
| Version/environment row + `emd` re-check | none | deferred | Nothing probes `emd --version` **for display** in the fork. Note what does exist: F5.3/E4's version *lock* (§5) checks the binary and gates every mutating control on the result, with a 30 s mtime poll. What is missing is the Settings-page readout of the version/environment and its manual re-check button. One of the two feature stragglers left after F5.4. |
| Per-key persistence in the settings DB | `.zed/settings.json` (repo-shared) + shell env (per-user) | ✓ | Split exactly as the spec's disposition table proposed. |

## 11. Fork-only additions (no ggo-ide equivalent)

| Feature | Where | Notes |
|---|---|---|
| "GGO World" language: tree-sitter grammar, syntax highlighting, bracket/quote autoclose, outline symbols for `[[entity]]`/`[[instance]]`/`[[background]]` | `ggo_language` + the `file_types` glob shipped in `assets/settings/default.json` (`// GGO` line, 2026-08-24) | Native registration (no extension sandbox), grammar via `tree-sitter-toml-ng` since upstream extracted TOML. **Activates everywhere since 2026-08-24**: the glob ships in the fork's default settings, so a game project needs no `.zed/settings.json` of its own; the per-repo copies (`ggo`'s) are now redundant and `ggo_language`'s `the_default_settings_ship_the_project_glob` test fails if an upstream merge drops the line. This is a config-placement gap, not a code gap; fixing it is copying config into the game repos, not moving anything between `ggo` and this fork. |
| `.zed/tasks.json` task layer (6 tasks) + `scripts/check-zed-config.sh` JSONC validator | ggo repo | Committed, per-user values via env, all six verified against real invocations. |
| Native perf ingest straight from the emulator pane into `ggo_ide.db` | `ggo_emu_panel::ingest` | Replaces the spec's CLI-chained ingest task. |
| **LSP** (diagnostics, completion, hover, code actions on worlds/manifests) | — | The pure-extension spec's central mechanism. **Deferred**: `ggo_language` registers grammar + queries only; there is no `ggo-lsp` binary and no `LspAdapter`. Every capability the old spec assigned to the LSP (world validation, hardware-budget diagnostics, snap/center/offset code actions, import diagnostics) is therefore unbuilt. |
| Legend band on chart widgets | `chart_geom.rs` `legend_layout` (:736), `legend_height` (:766), consumed at :1001 and :1024 | ggo-ide's charts have no legends (a deliberate omission); the fork adds one. Documented as a divergence in the module doc (`chart_geom.rs:25-29`). **Stale cross-reference corrected in F5.5**: this note used to point at a "§9's 'No legends' row" that does not exist — §9 has no such row, and never did in any version of this file. There is nothing to except this from; the legend band is simply fork-only surface, not a parity gap. |
| Emulator Pause / Resume + Step (`ctrl-alt-p`, `ctrl-alt-.`) | `drive::Session::{pause, resume, step}` (2026-08-25) | The drive loop parks after publishing a frame, feeds silence so the device stays live, and keeps the parked time out of the cart's clock. Step while running is a pause, never a skipped frame. ggo-ide had neither. |
| Emulator debug column (`ctrl-alt-d`): tile sheet, per-layer tilemap with the scroll window outlined, OAM composite + list, palette grid, hover readouts | `ggo_emu_panel::debug` over `ggo_emu_core::ppu::PpuSnapshot` (2026-08-25) | The drive thread refills a snapshot slot every vsync; viewers decode off-thread, throttled to 10 Hz while running and immediately on pause/step, images retired through the pane's atlas discipline. Replaces the wasm data panel ggo-ide dropped. |
| Pane runs persist saves | `drive.rs` via `ggo_emu_core::savefile` (moved from the `ggo-emu` binary, 2026-08-25) | Same rule as the standalone: `<card dir>/savs/<NAME>.sav`, flushed on a dirty frame at most once a second and at run end. |
| World editor selection set: shift-click, rubber-band, `ctrl-a` / `escape`, group drag and nudge as ONE undo entry (`WorldOp::MoveMany`) | `ggo_world_panel` + `ggo-worldlib::world_doc` (2026-08-25) | ggo-ide was single-select with no marquee. The inspector edits the primary (last-selected) item; every selected item gets its own outline. |
| World editor copy / paste / duplicate (`ctrl-c` / `ctrl-v` / `ctrl-d`) as world-file TOML on the OS clipboard | `ggo_world_panel` + `world_file::{fragment_to_toml, parse_fragment}` (2026-08-25) | Paste is one `WorldOp::Batch`; it lands at the cursor when it is over the canvas (snapped with Snap on), else one tile right/down. Pasted instances go through the cycle guard and are resolved so they render. |
| Entity / instance list in the world dock | `ggo_world_panel::render_entity_list` (2026-08-25) | Rows `#i <component>[ · stem]` and `⧉ <world>`; click / shift-click select like the canvas. Closes the "hard-to-click entity has no list fallback" gap in §4. |
| Instance removal asks first | `ggo_world_panel::delete_selected_impl` (2026-08-25) | A set holding instances confirms (`ggo_common::confirm_destructive`); entities alone delete without a prompt (one undo away). Multi-delete is one `WorldOp::Batch`. |
| Audio tab: waveform, play/stop/loop, **Source \| Baked** A/B (the blob through a standalone `ggo_emu_core::apu::Apu` into the emu pane's cpal ring), rate picker, **Import → `assets/<stem>.adp`** | `ggo_audio_panel` + `../ggo/tools/ggo-audio` (2026-08-24) | ggo-ide could only import (with a rate knob) and play the source through `<audio controls>`. The rate knob is editor-side by design: emerald packs a pre-baked `.adp` verbatim and knows nothing of the editor. No synthesis, sequencer, or PSG/ADSR/pan authoring — emerald's runtime never programs those. |
| World toolbar `audio N / 384 KiB` readout | `ggo_world_panel::audio_budget` (2026-08-24) | Every `Music`/`Sfx` stem the world (and its resolved instances) names, sized off-thread; red when over the APU sample region, since the runtime silently skips an upload past it. Cache clears on panel refresh, so a re-imported file re-sizes when the panel is next shown. |
| `.wav` / `.ogg` / `.adp` explorer routing | `ggo_audio_panel::intercept_audio_open` | Same interceptor pattern as `.spr` / `.til` / `.cart` / `.map` / worlds. |

---

## Counts

**These are the final counts. This file's last edit as a migration document
was F5.5; the table below is not expected to move again.**

| Status | Rows |
|---|---|
| ✓ | 55 |
| partial | 36 |
| deferred | 2 |
| dropped | 11 |
| **total** | **104** |

(F4 wrap: the `.til` tileset-editor row moved dropped → partial now that
`ggo_tileset_panel` ships a read-only viewer. Before F4: ✓ 31 / partial 28 /
deferred 31 / dropped 13. The sprite/world-picker rows also changed *text*
(explorer-driven instead of in-panel pickers) but not *status* — both were
already ✓/partial and stay there.)

(F5.2/S3: the "create component/system/schedule" row moved deferred → ✓ with
`ggo_emerald_panel`, and a new "New tileset" row lands at ✓ (a fork-only
affordance, so the total grows by one). Before S3: ✓ 33 / partial 31 /
deferred 28 / dropped 11, total 103.)

(X4: **no status moved, so these counts are unchanged** — ✓ 31 / partial 29 /
deferred 31 / dropped 12 both before and after. The cart-library row was
already `partial` for the in-panel dropdown and stays `partial` for
explorer-driven selection: browsing is still materially smaller than ggo-ide's
managed library with upload, delete and magic-header validation. What changed
is that row's *text*, plus the Assets browser-rail row's and the routing
deferral's.)

(F5.0/G2: **two rows moved.** "Delete world (confirm + rescan)" deferred → ✓
and "Sprite rail ops" deferred → partial, both on the project-panel context-menu
contributor hook. Before G2: ✓ 31 / partial 29 / deferred 31 / dropped 12.
After: ✓ 32 / partial 30 / deferred 29 / dropped 12.)

(F5.2/S2: **one row moved.** "Sprite rail ops: New / Duplicate / Rename / Delete `.spr`" partial → ✓, now that New Sprite…/New Metasprite… and Rename Sprite… ship alongside Duplicate and Delete. Before S2: ✓ 32 / partial 32 / deferred 28 / dropped 11. After: ✓ 33 / partial 31 / deferred 28 / dropped 11.)

(F5.1 wrap: **two more rows moved**, plus a table correction. "Map editor"
went dropped → partial when M2 shipped `ggo_map_panel` (fix-round commit
`811a84d872`, ahead of this wrap — that commit updated the row's own text but
never re-tallied the table below it, so the printed counts had already
drifted one partial/one dropped out of step with the actual rows; folded in
here rather than left to compound). "PNG import wizard" went deferred →
partial when `ggo_import_panel` shipped (Task I2), tileset-only. Before F5.1
(the last *tallied* snapshot, i.e. G2's numbers above, which by the time of
this wrap no longer matched the table): ✓ 32 / partial 30 / deferred 29 /
dropped 12. After: ✓ 32 / partial 32 / deferred 28 / dropped 11.)

(F5.2 wrap/S5: **five rows moved**, closing out the phase. "New tileset / new
map" (§3) deferred → ✓, now that both New Tileset… (S3) and New Map… (F5.1)
exist. "+ New world" (§4) deferred → ✓ via `ggo_emerald_panel`'s New World…
(S3). "Emulate" (§4) deferred → partial via "Emulate this world (cart)"
(S4) — cart mode, not full-system boot. Per-cart "Re-run" (§7) deferred → ✓
via the "Re-run (perf)" context-menu entry (S4) — a different surface
(context menu, not a Reports-page button) but the same capability end to
end. "Run hardware diagnostics" (§8) deferred → partial via the S4 context-menu
entry — one-shot, no streaming/cancel/options. "Create component/system/
schedule" (§5) and "Sprite rail ops" (§3) were already ✓ from S2/S3 and are
unchanged here; their honesty notes (Resource/Module have forms but no menu
entry) were tightened without a status change. Before this wrap: ✓ 35 /
partial 31 / deferred 27 / dropped 11, total 104. After: ✓ 38 / partial 33 /
deferred 22 / dropped 11, total 104.)

(F5.3 wrap: **seven rows moved**, the largest single-phase movement so far and
the last of the emerald ops. Six deferred → ✓: "Remove component/system/
schedule, add/remove field" (§5, E2), "emd version-lock banner + gating + mtime
poll + mid-run drift" (§5, E4), "Onion skin" (§3, E5), "Grid checkbox / Reset /
preview stepper" (§4, E5), "Arrow-key nudge" (§4, E5) and "MetaSprite `stem` →
goto sprite" (§4, E5). One deferred → partial: "Schedule ordered run-list
editor" (§5, E3) — everything ported except a free cadence field, which is a
fixed picker here. Two rows changed *text* without changing status:
"Components/Systems/Schedules browsing" (§5) stays `partial` — the three-tab
browser shipped, but with no `[scene]` tag, no per-row counts and no distinct
load-failure state — and "Cross-page handoffs" (§1) stays `partial` now that
all four hops ship, on the one residual that its Emulate hop is cart mode.
Before this wrap: ✓ 38 / partial 33 / deferred 22 / dropped 11, total 104.
After: ✓ 44 / partial 34 / deferred 15 / dropped 11, total 104.)

(F5.4 wrap: **thirteen rows moved**, the largest single-phase movement, and
the one that empties the deferred column. Twelve deferred → ✓: the Reports
"Re-run" entry point (§6, R3), run detail header + config line (§7, R2),
failed-asset-loads + Panics tables (§7, R2), guest UART console for a stored
run (§7, R3), KPI tile rows (§7, R2), historic overlay (§7, R5),
click-to-inspect frame (§7, R4), I$ profile table with sortable header (§7,
R4), the device history rail (§8, R3), click-drag x-zoom + double-click reset
(§9, R5), click-to-select a frame (§9, R4) and historic overlays on the chart
widgets (§9, R5). One deferred → partial: emulator audio + Mute + underrun
counter (§6, R6) — the panel owns the stream now, but only on the cart path
and only exercised on Linux/ALSA, and the counter counts dropout *events*.
Three rows changed *text* without changing status: "Build & run project"
(§6) now says full-system audio is what stays external; "Ignored-frames
editor" (§7) is flagged as the reports page's last remaining gap; and
"Cached static layer" (§9) stays `partial` on a measured residual rather
than on the old "scenes rebuilt on hover" — R5 added the memo, and one
`O(frames)` rebuild per hover move survives it. Before this wrap: ✓ 44 /
partial 34 / deferred 15 / dropped 11, total 104. After: ✓ 56 / partial 35 /
deferred 2 / dropped 11, total 104.)

(**F5.5 wrap: one row moved, and it moved the wrong way** — the only such
entry in this table's history, which is why it gets said plainly rather than
folded into a total. §4's **inspector row went ✓ → partial.** Nothing
regressed in the code; the audit did. The row enumerates "asset dropdowns"
among the features it claims to cover, and the fork does not have them: it
maps `FieldKind::Asset(_)` onto `Str` (`world_panel/src/inspector.rs:113`,
`:159`), so an asset reference is unvalidated free text where ggo-ide offered
a `pick_list` over stems that exist. That is "materially smaller than
ggo-ide's" by this file's own legend, and it was carrying a ✓ for three
phases. Found by D1's audit while reading ggo-ide's inspector tests, i.e. by
looking at the thing being deleted — which is an argument for having done the
coverage audit before the `rm`, not after. No other row moved: F5.5 shipped
no features, it deleted the reference implementation. Before this wrap:
✓ 56 / partial 35 / deferred 2 / dropped 11, total 104. After: ✓ 55 /
partial 36 / deferred 2 / dropped 11, total 104. Several rows changed *text*
without changing status — the legacy `.meta.json` row, the map editor's
`cols` note, "Build & run project", the keyboard/gamepad row, the I$ profile
table and §5's structured-viewer row, each correcting or narrowing a claim
about what the deletion cost; see the closing section.)

(Counted over §§1-10; §11's fork-only rows are not dispositions of a ggo-ide
feature and are excluded, except that the LSP row there is a deferral in its
own right.)

**The remaining deferred set, named in full.** After F5.4 it is three items,
and only one of them is large:

1. **LSP** (§11) — the pure-extension spec's central mechanism, and the only
   substantial thing left. `ggo_language` registers a grammar and queries;
   there is no `ggo-lsp` binary and no `LspAdapter`, so world validation,
   hardware-budget diagnostics, snap/center/offset code actions and import
   diagnostics are all unbuilt. It is not counted in the table above (§11
   rows are not ggo-ide dispositions) but it is a real deferral and the
   honest headline for "what is left".
2. **§10's version/environment row + `emd` re-check** — nothing in the fork
   probes `emd --version` for display. Note the version *lock* does ship
   (§5, F5.3/E4); what is missing is the Settings-page readout and its
   manual re-check button.
3. **§7's "Copy for agent" text export** — never started.

That is the whole deferred column: LSP plus two stragglers. Everything else
outstanding is now either a `partial` with its shortfall written into the row,
a deliberate `dropped`, or an entry in the ledger below.

Read that shape honestly: the panels the fork set out to build (world,
sprite, charts, emulator) are largely ✓ — and as of F5.4 the Reports surface
is too, which was the last big block of "deferred" outside the LSP. Emerald
ops run in-panel, gated on the version lock, rather than in a terminal. The
remaining *ancillary* pages — Device, Settings, and Emerald's `emerald.toml`
viewer — are still mostly "partial via a task or a text file"; and the
pixel-art half of Assets is a deliberate drop with no replacement in this
repo.

---

## Closing: what is actually gone, in plain terms

This section exists so that nobody arriving in six months has to reconstruct
the answer by reading 104 table rows. It is the whole picture as of F5.5,
with ggo-ide deleted.

### Four things you could do before and cannot do now

Not "not ported" — **not available anywhere**, in any repo, by any command.
Each was verified against both trees rather than inferred from a report.

1. **Play the full system interactively.** You can still *boot* it:
   `ggo-emu --full-system CARD_DIR` (and `--no-cart` / `--os-only`) drives
   `ggo_emu_core::fullsystem::{os_image::build_os_image, card::build_fat_image}`,
   which survive, and `emd pack-ggo` → `ggo-emu --full-system <dir>`
   reproduces ggo-ide's Build-&-run end to end. But that path is **headless**
   — `native::Display` is wired only to `ggo-emu`'s cart path — so what you
   get is a boot plus a perf capture to an instruction/frame budget, not a
   window. The fork's `ggo_emu_panel` is cart-only. **No surface anywhere
   renders a full-system frame to a screen.**
2. **Press a button in the GemOS launcher.** `pages/emulator.rs`'s `fs_key_bit`
   was the only keyboard map onto `firmware/ggo-hal/src/dev_input.rs`'s
   hardware button layout in any codebase. Cart-mode keyboard input survives
   twice (`ggo_emu_panel`'s `input.rs` here, `ggo-emu`'s `native.rs` there),
   so playing a *cart* is fine — but the OS menu's UP/DOWN can now only be
   driven by `--auto-input`'s scripted F1 pulse. Combined with (1): the OS is
   bootable and unwatchable and untouchable.
3. **Scaffold a project from a GUI.** `emd new <name>` is alive and well; the
   ggo repo's `emd: new project scaffold` task is now its only affordance,
   and it takes its name from `GGO_NEW_NAME` or the task picker's
   edit-and-spawn flow rather than a prompt. The Projects page — library,
   add/select/remove, launcher screen — was a deliberate `dropped`, and Zed's
   own project model is the replacement.
4. **Reach a per-tileset `cols` width you previously set.** The `tileset_cols:*`
   rows in `~/.ggo/ggo_ide.db` now have **no writer and no reader**: ggo-ide
   was both, and no fork crate mentions the key. *Resolved 2026-08-25:* the
   width is a per-tileset sidecar (`.ggo-ide/<rel>.editor.json`) shared by
   both panels; the db rows stay orphaned — re-set the width once in the
   tileset panel.

### Three things that were *reported* lost and are not

Recorded because each was a live claim during F5.5, and believing any of them
would send someone rebuilding something that already works:

- **Full-system boot itself.** Partial, not lost — see (1) above. What died
  was ~100 lines of glue and the window, not the engine.
- **`profile`/`dprofile` row production.** `ggo-emu <cart> --profile <elf>`
  loads the symbols and `report::write_db` persists both dumps **by default**.
  ggo-ide's `cart_elf_bytes` was auto-*discovery* of the companion ELF, i.e.
  convenience. **The charts panel's I$ profile table and its two per-function
  charts are not rendering a dead feature** — the opposite conclusion would
  have been serious, so it is stated positively here.
- **Reading legacy `.meta.json` sidecars.** The reader is
  `ggo_worldlib::sprites::io::read_legacy_sidecar`, it survives, this fork
  already depends on it by path, and it was never ggo-ide's only caller. No
  fork crate calls it — but that is unwired, not unavailable, and there is no
  data to strand (zero sidecars exist).

### The deferred set, in full

Three items. Unchanged by F5.5, which shipped no features.

1. **LSP** (§11) — the pure-extension spec's central mechanism and the only
   large thing left. `ggo_language` registers a tree-sitter grammar and
   queries; there is no `ggo-lsp` binary and no `LspAdapter`, so world
   validation, hardware-budget diagnostics, snap/center/offset code actions
   and import diagnostics are all unbuilt. Not counted in the table (§11 rows
   are fork-only), but it is the honest headline for "what is left".
2. **§10's version/environment row + `emd` re-check** — nothing probes
   `emd --version` *for display*. The version **lock** does ship (§5, F5.3/E4)
   and gates every mutating emerald control with a 30 s mtime poll; what is
   missing is the Settings-page readout and its manual re-check button.
3. **§7's "Copy for agent" text export** — never started.

### Coverage that is now single-covered here

D1's audit classified ggo-ide's 1193 executing tests before the deletion and
moved the one genuine orphaned assertion into worldlib (plus six gap-fills,
worldlib 668 → 675). What it also found is that for a number of behaviours
**the fork's own test is now the only test in existence**. Four are
outright gaps — the fork implements the arm and nothing pins it:

- `emu_panel/src/drive.rs:455`'s `FrameEvent::Fault` arm is untested here;
  ggo-ide's `illegal_instruction_body_faults_on_first_step` was the only cover.
- `emu_panel/src/ingest.rs` asserts only the *empty* profile/dprofile case;
  ggo-ide's populated-row test was the only cover of the populated one.
- `charts_panel/src/chart_geom.rs::bins` has three `bins_*` tests, none of
  them the two exact `Histogram.tsx` parity vectors — the only record of the
  `all_int → step.ceil()` rounding contract, which dies with them. Also now
  unpinned: `zoom_domain`'s right-to-left drag, `edge_marks`' half-integer
  round-away-from-zero, `format_tick`'s negative-with-suffix (`-12k`), and
  `accumulate` at three series deep.
- `world_panel/src/inspector.rs` implements the `FieldKind::Fixed` arm
  (`:158`) with no test naming it; ggo-ide had ten `commit_field` tests.

None is a crash risk today. They are named so that a future change to any of
those four is understood to be unguarded.

### The debts this migration is knowingly leaving behind

Scheduled, not forgotten. Fuller entries in the ledger below.

- **The missing run-kind column.** Cart-mode and full-system runs land in one
  `runs` table distinguished only by a path convention in `label`. Deferred by
  F5.2, F5.3 **and** F5.4 — three phases — while becoming load-bearing in
  three surfaces (the run picker, the historic overlay, the device history
  rail). One schema column plus three small reads.
- **`charts_panel/src/ggo_charts_panel.rs` is 4,998 lines**, up ~1,700 in
  F5.4 alone. Split it before adding to it.
- **The 100k-frame hover ceiling.** ~11.8 ms per memoized hover move in a
  debug build, at or past a frame budget. The memo already cut this from
  O(charts × frames) to one chart; the residual is that single chart's
  `O(frames)` rebuild, and the fix is decimating **by index before**
  `plot_points` materialises 100k points that `envelope` immediately cuts
  to 2048.
- **Up to 10 short-lived tokio runtimes per run selection** (four for
  `load_run_samples`, up to six more for the historic overlay, paid whether
  or not the toggle is used). The fix is worldlib-side: a shared connection
  the panel borrows instead of a runtime per query.

---

## Known deferrals (consolidated ledger)

Open items carried out of F1-F3 that are not feature-disposition rows — bugs,
guards and follow-ups that someone has to pick up:

**worldlib / ggo repo**

- **PaletteSet slot guard** and **last-frame `FrameDelete` guard** — the next
  worldlib doc-op hardening PR (follow-on to ggo #73). Panels re-check both
  before applying, so this is defence in depth, not a live crash.
- **~~Redo of an add leaves the instance subtree unresolved.~~ Resolved
  2026-08-25.** `redo_impl` re-resolves every instance that comes back with
  neither `resolved` nor `error` and refreshes the image cache.
- **`ggo-db` index PR**: `frame(run_id)` and `profile(run_id)` have no index;
  the charts panel's per-run sample query full-scans. Not felt at current run
  counts.
- **`scripts/check-zed-config.sh` trailing-comma stripping** can mangle string
  content containing `,]` or `,}` (fix-forward, low: no such string exists in
  either committed file today).

**fork**

- **Atlas retention in the two document panels.** Only `ggo_emu_panel`
  implements the `Window::drop_image` release contract (double-buffered retire,
  full release on stop, `on_release` at teardown). ~~`ggo_world_panel` rebuilds
  its whole image cache on add-instance and on every world switch~~ (resolved
  2026-08-25: every rebuild queues the images the new cache no longer holds,
  a world switch queues the whole cache, the canvas item's render drops them
  two-stage and `on_release` sweeps the rest), and
  `ggo_sprite_panel` rebuilds every frame + pool-tile `RenderImage` after
  *every* op/undo/redo/save and never calls `drop_image`, so it still leaks
  atlas tiles at edit frequency. Bounded by session length, not by a loop, so it
  was not an F2/F3 blocker; it is still a real leak.
- **~~Linear-upscale blur in the emu pane.~~ Resolved.** The pane paints
  through `paint_image(.., nearest = true)` at an aspect-true fit (see
  `render_screen` / `scaled_frame_bounds`); this entry was stale.
- **~~Stuck keys on window deactivation.~~ Resolved 2026-08-25.** Every
  render while the window is inactive releases the pad
  (`Window::is_window_active` in `EmuPanel::render`); frames keep the
  renders coming, so the release is immediate.
- **~~`MAX_FRAMES = 100_000` ingest rejection.~~ Resolved 2026-08-25.** A
  run past ~28 minutes at 60 Hz now ingests its first 100 000 frames and
  the ingest row says `truncated to 100000 of N frames` instead of
  refusing the whole run.
- **Frame-0 ignore-set editor — the reports page's one remaining gap vs
  ggo-ide (as of F5.4).** The charts panel applies ggo-ide's default (drop
  frame 0), captions it, and threads the ignore set through every derivation
  that needs it (KPI tiles, charts, overlays, the profile table's
  "all rows ignored" state), but there is **no chip editor**: no way to add
  or remove a frame from the set, and no "N of M excluded" readout. Every
  other §7 row is now ✓ or a stated divergence, so this is the whole
  remainder.
- **Hover rebuild ceiling (narrowed by F5.4/R5, not closed).** It used to be
  O(charts × frames) per mouse-move frame; the scene memo cut it to one
  chart's rebuild, because the hovered chart's spec carries the readout.
  Measured at the 100k-frame cap in a debug build: **~11.8 ms per memoized
  hover move**, which is at or past a frame budget once the rest of the frame
  is counted. The `Arc` clone share is **0.4%**, so the residual is the single
  `O(frames)` rebuild and not the caching. The real win is decimating **by
  index before `plot_points`** rather than after: today `plot_points`
  materialises 100k points that `envelope` immediately reduces to 2048.
- **Glob duplicated between the fork and the repo.** `ggo_language`'s
  `PROJECT_FILE_TYPE_GLOB` and the ggo repo's `.zed/settings.json` `file_types`
  entry must agree, and nothing checks that they do across the two repos.
- **Explorer-driven routing is project-panel-only (F4).** `cmd-p` file finder,
  go-to-definition, drag-and-drop, the `zed <path>` CLI, and session restore
  all call `Workspace::open_path_preview`/`open_paths` directly, bypassing
  `Workspace::intercept_path_open` — opening a `.spr`/`.til`/`.cart`/world
  `.toml` through any of them yields a dead editor tab instead of routing into
  its panel. Sharper since X4: with the emu panel's `.cart` dropdown removed,
  the project panel is the *only* way to get a cart into the emulator, so a
  cart reached through the file finder has no fallback at all. Named upgrade
  path: option 2 in
  `.superpowers/sdd/explorer-routing-investigation.md` (hooking inside
  `open_path_preview` itself, `workspace.rs:4713`), not taken because
  `cx: &mut App` there forces a fake `Task` return (toast or leak) instead of
  a clean decline.
- **`grid_cols` clamp diverges from ggo-ide's flat 8-column tileset layout.**
  `ggo_tileset_panel`'s fallback (`GRID_COLS_FALLBACK(8).min(tile_count.max(1))`)
  clamps a short tileset to its own tile count instead of padding out to a
  full 8-wide row of blanks, so a sheet under 8 tiles lays out differently
  than ggo-ide renders the same file. Deliberate (documented on the fn); noted
  here because it is a visible rendering difference, not just an internal one.
- **Upstream's generic "Duplicate" is still in the menu for a `.spr` (F5.0).**
  `project_panel.rs`'s own "Duplicate" (`:1182`) sits three separator groups
  above `ggo_sprite_panel`'s "Duplicate Sprite", and it performs precisely
  the raw byte copy the sprite entry exists to avoid: the resulting file points
  at the ORIGINAL's `.til`/`.pal`, which flips both sprites into `pool_shared`
  and makes `DocOp::Dedup`/`DocOp::PaletteRemap` fail on the source. Two
  near-identically-labelled entries, one of which quietly damages the file you
  copied from. Not fixable in F5.0: G1's contributor hook only APPENDS a
  `Vec<ContextMenuItem>` and has no way to suppress or replace an upstream
  entry. **F5.2 input**: the registry may need a suppress/replace capability,
  not just append.
- **Dangling `[[instance]]` refs are invisible (pre-existing F1 gap, now
  reachable).** `ggo_world_panel::loader` populates `node["error"]` for an
  instance whose world does not resolve (`loader.rs:339`), but `inspector.rs`
  never renders it — ggo-ide did (`world/inspector.rs:410-412`). The canvas
  shows a placeholder and nothing says why. Latent until F5.0: "Delete World"
  is the first fork action that MANUFACTURES dangling refs, so it is worth
  surfacing in F5.2.
- **Linux fallback-prompt race.** A second file click while a
  `prepare_to_close_dirty` save prompt is already up silently replaces the
  first prompt with the second; the first resolves to Cancel with no data
  loss (the document that would have closed just stays open and dirty), but
  it vanishes without the user explicitly dismissing it. Structurally hard to
  test in-tree (platform prompt stacking); carried forward from X1 fix
  round 1 rather than fixed.
- **"Emulate this world" is CART mode, not ggo-ide's full-system boot
  (F5.2/S4).** ggo-ide's world-page Emulate builds a whole system —
  `backend::emubuild::build_full_system_with_world` stages the project with
  `emd pack-ggo`, builds a GemOS image out of `firmware/system`, packs a FAT
  card, and boots the SoC. The fork's `ggo_emu_panel::drive` is `ggo-emu`'s
  cart-mode loop, so the entry (labelled **"Emulate this world (cart)"** for
  exactly this reason) builds `emd pack-ggo --world <stem>` and runs the
  cartridge. `ggo_emu_core::fullsystem::run` DOES exist, so this is a scope
  decision, not an impossibility — but three things are genuinely lost until
  it is revisited:
  1. **The game never boots through GemOS.** The OS→cart handoff, the syscall
     surface, card/FAT asset streaming and the save flush are all unexercised,
     so the entry no longer answers "does this world boot on the device" — only
     "does this world run".
  2. **Different perf profile.** Cart mode keeps `PerfSim`'s `CART_XIP_BASE`
     cache base (`emu_panel/src/drive.rs`), so OS code is absent from the I$
     model and from frame cost. Numbers from this entry are not comparable
     with ggo-ide's.
  3. **Both kinds land in the same `runs` table, with no mode column**
     (`emu_panel/src/ingest.rs` keys on the cart path; the `label` column is
     the rel path). It shows up in three places now. In the run **picker
     list** (C1): a cart-mode run and a full-system run of the same game
     share one `cart_name`, so two rows can read as just `"demo"` next to
     each other with nothing marking which is which except an opaque
     free-text `label`, and ggo-ide's own runs leave `label` unset entirely.
     In the **historic overlay** (F5.4/R5), which the entry as written in
     F5.2 said did not exist yet and now does: `pick_prior_ids` scopes on
     `cart_id` alone, so a cart-mode run's ghosts can be full-system runs of
     the same cart — two different workloads on one chart, with only the age
     ramp to tell them apart. And in the device **history rail** (F5.4/R3),
     whose rows carry `started_at`/`state`/`verdict` and nothing else.
     Until a mode column exists, the only discriminator is `label`: `None` =
     ggo-ide, `target/ggo-emulate/*.ggo` = this entry, anything else = a cart
     clicked in the fork's explorer — none of that is surfaced in any of the
     three UIs. **Still the recommendation**: add an explicit run-kind column
     and show it in the picker, the rail and the overlay, rather than leaning
     on that convention. (Overlaying possibly-different workloads of one cart
     is ggo-ide's assumption too, so R5 matched it rather than inventing a
     fork-only filter — but matched, not fixed.)
- **An unreadable manifest reads as an absent one (F5.3/E2).**
  `manifests::read_manifest` maps any read/parse failure to "no such manifest",
  where ggo-ide shows a red "Failed to load: …" naming the file and fails the
  whole load. Two consequences: a malformed `manifests/systems.toml` presents
  as an empty Systems tab rather than a broken one, and — worse — a schedule
  ROLLBACK that happens to land in that moment collapses the schedule's row
  list to empty instead of showing the saved list plus the "Edit not applied"
  notice, because the rollback is a re-read. Needs a third state
  (`absent` / `loaded` / `failed`) threaded through the browser.
- **`worlds_using_component` scans the whole assets tree on the UI thread
  (F5.3/E2).** Every trash click on a component walks `assets/worlds/**` and
  TOML-parses each world before the confirm can be shown. Fine for the world
  counts in play today, synchronous and unbounded in principle; the same work
  belongs on the background executor with the confirm awaiting it.
- **A second trash click while a confirm is up drops the first continuation
  (F5.3/E2).** The pending-confirm slot is single-valued, so opening a second
  destructive confirm silently discards the first one's continuation. No data
  is lost (the discarded op never ran), but the first dialog's answer goes
  nowhere. Same shape as the Linux fallback-prompt race above, one layer up.
- **`emerald_dir` falls back to the worktree root (F5.3/E2).** A project whose
  emerald checkout is NESTED (worktree root is not itself the emerald project)
  lists nothing until the user right-clicks into the nested directory, because
  the panel's default root is the worktree root and only a context-menu action
  re-points it. Discoverability, not correctness: the panel is empty and its
  empty-state text is the only hint.
- **Onion `ghost_cache` grows with the strip, not with the live ghost count
  (F5.3/E5).** It reads as a small cache — at most six ghosts are on screen at
  once — but the key is `(dist, frame idx)` and the only eviction is the
  wholesale clear on a doc mutation, so scrubbing a long clip with onion on
  accumulates up to `6 × frame_count` composed images between edits. Same
  family as the atlas-retention entry above: bounded by session length and by
  the next edit, not by a loop.
- **`kill_on_drop` does not reach `emd`'s `cargo` grandchild (F5.3/E2).** The
  600 s `EMD_TIMEOUT` drops the child, which kills `emd` — but the `cargo
  check` it spawned keeps running to completion, still holding the build lock.
  The comments now say so rather than implying the timeout reclaims the
  machine; a real group kill needs the child put in its own process group
  (`setsid` / `process_group(0)`) and a signal to the negated pgid, which is
  platform work this phase did not take.
- **`AudioOut::stop()` does not join the stream thread (F5.4/R6).** Stopping
  a run drops the handle and returns; cpal tears the stream down
  asynchronously, so a Run issued immediately after a Stop can have **two
  output streams alive for roughly one buffer period (~16 ms)**. Audible as a
  brief doubling at restart, nothing worse. Same shape, one layer up: a
  verdict write from run A that lands after run B has been selected shows
  A's status on B's row — cosmetic, corrected by the next refresh, and worth
  fixing with the same generation guard the detail loads already use.
- **Emulator audio is untested off Linux, and cart-mode only (F5.4/R6).**
  Every run of it has been Linux/ALSA. The Windows/CoreAudio path is written
  against cpal's documented COM-apartment rule (the device is opened on the
  emu thread for exactly that reason) but has never been executed, and
  `snd_pcm_open` has no timeout around it. Audio is wired only through
  `drive::run`, the cart path, so the panel has no full-system audio at all.
  **`pump_audio` is the seam** a full-system mode would tap: it is the single
  point where the emulator's sample output reaches the ring, so full-system
  audio is a call site, not a redesign.
- **Up to 10 short-lived tokio runtimes per run selection (F5.4/R3, R5).**
  Each worldlib query opens its own connection and spins its own runtime:
  four for `load_run_samples`, then up to six more once R5 added the
  historic overlay (one `cart_runs` + up to five `run_frames`) — and the
  overlay six are paid on every selection whether or not the toggle is ever
  switched on. Not felt at current run sizes. The fix is worldlib-side: a
  shared connection (or a connection pool) that the panel borrows, rather
  than a runtime per query.
- **A zoomed chart can keep a frame selected outside its own window
  (F5.4/R5).** Zoom and frame selection are independent, so the inspect pane
  goes on showing a frame the plot no longer draws. ggo-ide behaves the same
  way (its zoom and `selFrame` are likewise independent), so this is matched,
  not fixed. Related and separate: the scene cache is a field of the
  `ChartsPanel` entity, i.e. **keyed per panel, not per window** — they
  coincide today because `init` creates one panel per workspace, but nothing
  enforces that.
- **Non-finite overlay values are unguarded (F5.4/R5).** The only `is_finite`
  filter in `chart_geom` is in `bins()`, for histograms. Overlay (and live
  line) values go straight into the y-scale fold and the point mapper. Not
  reachable today — every overlay field is an `i64 as f32` cast — but a
  producer of float columns would not be caught, and a single NaN poisons the
  chart's whole y-scale.
- **`has_data()` ignores the `historic` field (F5.4/R5).** A `ChartSpec`
  carrying only overlays and no live series reports "no data", so
  `build_chart_scene` and `frame_at` early-out and the overlays are dropped
  **silently**. Deliberate and documented ("an overlay is context for a run,
  not a run"), and unreachable from `chart_set`, which never builds an
  overlay-only spec — noted here because it is a silent drop rather than a
  visible one if that ever changes.
- **Hardware diagnostics is one-shot, with no streaming, cancel or options
  (F5.2/S4).** The "Run hardware diagnostics" entry spawns
  `ggo-diag --tty <port> --skip-pnr --launch` and shows the transcript when it
  finishes. ggo-ide's Device page streams lines live, can cancel the child,
  offers baud / collect-seconds / port pickers and clones `diag.db` rows back
  into `ggo_ide.db` afterwards; none of that is here. The repo checkout must
  also be pointed at with `GGO_REPO` — ggo-ide lives inside the repo and
  auto-detects from `CARGO_MANIFEST_DIR`, which a fork whose worktree is the
  user's game project cannot do.
- **`ggo_charts_panel.rs` is 4,998 lines (F5.5).** It grew ~1,700 lines in
  F5.4 alone. Nothing is wrong with it, and that is the problem: it is a
  single file holding the run picker, the detail view, every KPI and chart
  render path, the device rail, the inspect pane and the profile table.
  **Split it before adding to it** — the natural seams are already module
  boundaries elsewhere in the crate (`chart_set`, `chart_geom`, `report`,
  `inspect`, `history`).
- **`tileset_cols:*` is an orphaned settings key (F5.5).** The rows exist in
  `~/.ggo/ggo_ide.db`, ggo-ide was their only writer *and* reader, and
  `grep -rn tileset_cols crates/ggo/` returns nothing. *Superseded
  2026-08-25:* cols come from the sidecar
  (`ggo_worldlib::sprites::tileset_meta`), not the db; the rows are dead data.
- **`FieldKind::Asset(_)` commits unvalidated free text (F5.5).**
  `world_panel/src/inspector.rs:113`/`:159` fold `Asset(_)` into the `Str`
  arm, so a `Sprite.stem` can name a `.spr` that does not exist and the value
  lands in the world doc. ggo-ide used a `pick_list` over existing stems, so
  the invalid state was unreachable there. Compounds the dangling-`[[instance]]`
  entry above: two unvalidated-reference paths and one `node["error"]` that
  `inspector.rs` never renders. §4's row is marked `partial` for this.
- **Four behaviours are now single-covered by the fork's own tests (F5.5).**
  `emu_panel/src/drive.rs:455`'s `FrameEvent::Fault` arm and
  `emu_panel/src/ingest.rs`'s populated profile/dprofile case have **no** test
  in any repo; `chart_geom::bins` has three tests but neither of ggo-ide's two
  exact `Histogram.tsx` parity vectors (the only record of the
  `all_int → step.ceil()` rounding contract), and `zoom_domain`'s
  right-to-left drag, `edge_marks`' half-integer rounding, `format_tick`'s
  `-12k` case and `accumulate` at three series deep went with them; and
  `world_panel/src/inspector.rs:158`'s `FieldKind::Fixed` arm is implemented
  and unpinned. Cheap to close (four small tests), listed so that a change to
  any of them is known to be unguarded rather than discovered to be.
