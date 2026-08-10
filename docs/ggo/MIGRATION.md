# ggo-ide → fork: feature disposition audit

Date: 2026-08-08 (end of F3), rows amended 2026-08-09 (end of F4: explorer-driven
panel routing + the read-only tileset viewer; then X4, which removed the last
in-panel file picker — the emulator's `.cart` dropdown), and 2026-08-10 (end of
F5.3: the emerald mutation/reorder ops, the emd version lock, and the four
carried panel deferrals — onion skin, world view controls, arrow nudge, goto
sprite). Fork branch `ggo`.

What this is: a row for **every** user-facing ggo-ide feature, and what — if
anything — answers it in the fork today. It exists so nobody has to guess
whether a thing was ported, replaced, or simply dropped. It is a reference,
not a status report to be proud of; the honest answer for a large part of the
Assets suite is "not covered".

**Sources of truth.** ggo-ide's surface is enumerated from
`ggo/tools/ggo-ide/src/pages/` + `nav.rs` and from
`ggo/docs/ggo-ide-feature-inventory.md` (the redesign brief, which is the
complete list). The intended dispositions come from
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

ggo-ide is still in-tree and still the only place the "dropped" and several
"deferred" rows work. Freezing or removing it is a separate decision.

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
| Typing guard (single-letter hotkeys suppressed while a text field is live) | gpui focus contexts (`not_editing` predicate on the sprite panel; emu pad keys are focus-scoped) | ✓ | Real focus/blur exists here, so the Iced workaround is unnecessary. |
| Theme (22 Iced themes, persisted) | Zed themes | ✓ | |
| `capture_lint` source lint | none | dropped | Guards an Iced `ButtonReleased`/`and_capture` bug class that has no gpui analogue. |

## 2. Projects page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Project library (list, add existing, select, remove) | Zed open-folder / recent projects | dropped | The page is superseded by the editor's own project model; the spec's disposition table already called the library and its persistence dropped. |
| Launcher view (branding screen when nothing is open) | Zed's own welcome/recent-projects screen | dropped | Same. |
| "Create Emerald project" (`emd new <name>`) | `emd: new project scaffold` task | partial | Zed's `tasks.json` has no interactive input variable, so the name comes from `GGO_NEW_NAME` or the task picker's edit-and-spawn flow — not a prompt. |
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
| Onion skin (toggle, back/fwd ghost counts, opacity) | `ggo_sprite_panel` `onion.rs` + a control row under the transport (F5.3/E5) | ✓ | Toggle, back/fwd counts (clamped 0–3), opacity and the `opacity * (1 - (|dist|-1) * 0.3)` falloff are ggo-ide's values; frame selection is worldlib's own `timeline_ops::onion_frames`, not a second copy. Ghosts carry ggo-ide's directional red/blue tint (E5 fast-follow) and paint farthest-first under the live frame. Two deliberate divergences: opacity is a `-`/`+` stepper rather than a slider (`ui` has no slider; same range, same 0.05 step, so the reachable values are identical), and ghosts are suppressed while the transport is playing (ggo-ide stacks them under a moving image, which only smears it). |
| Preview panel (1:1 live frame, background picker, LCD filter) | sprite panel centre preview | partial | Live composed frame with aspect fit shipped; checker/black/white background picker and the LCD scanline filter did not. |
| Hardware budget meter (4 traffic-light rows + tooltips) | sprite panel `hw_meter_line` | partial | Same four value/cap pairs from worldlib `sprites::hw`, condensed into one header line — no per-row dots or tooltips. |
| Per-cell tile assignment from the pool | sprite panel tile picker + preview cell click | ✓ | F5.2 gave the picker a real source: it composes the BOUND TILESET (the sprite's pool is that `.til` byte for byte) as one sheet via worldlib `unpack_til_to_indices` → `compose_tile_grid` → `indices_to_rgba`, the same three calls `ggo_tileset_panel` makes. |
| Tileset editor (`.til`): overview grid, magnified tile canvas, pencil/fill/eyedropper, palette slot edit, append/duplicate/delete-last, cols setting | `ggo_tileset_panel` — read-only overview grid + 16-slot palette swatch row, integer zoom 1x-8x | partial | Viewing shipped (F4): routed off a `.til` click in the project panel, no in-panel picker. The **magnified tile canvas**, pencil/fill/eyedropper, palette slot editing, append/duplicate/delete-last and the cols setting are all still dropped — the fork's zoom magnifies the whole sheet, not one selected tile, so there is no per-tile editing surface at all. This is a viewer, not the editor ggo-ide had; do not read it as more than that. |
| Map editor (`.map`): tileset binding, multi-tile stamp, H/V flip, palSub, brush/rect-fill/eraser/eyedropper, grid, zoom, resize | `ggo_map_panel` (F5.1 M2) — the fork's one editing surface | partial | Maps are **authored here, never imported** (user's call). Ported: tileset binding (`MapOp::BindTileset`, with the bound tileset's tiles as a rect-selectable stamp strip), multi-tile stamp placement, H/V flip and palSub on the stamp, rect-fill, eraser, eyedropper, grid toggle, pan, resize, undo/redo/save with the dirty guard. Opened by clicking a `.map` in the explorer, or created by "New Map…" on an assets directory (blank + **unbound**: cells are pool indices, so a guessed binding is worse than none — you bind from the panel). Not ported: the float zoom slider (integer 1x–8x ladder here, so 16px tiles never resample) and the palSub slider (a stepper over the same 0–15). **The real loss is the per-tileset `cols` setting.** ggo-ide resolves the tile-strip column count through the shared `resolve_and_migrate_cols`, keyed on the `.til` in `~/.ggo/ggo_ide.db`, so its map and tileset editors lay a given tileset out identically and the user can set that width. The fork has no db-backed cols and falls back to 8 columns (clamped) in both `ggo_map_panel` and `ggo_tileset_panel` — they still agree with each other, but not with a tileset authored at some other width. Because `cols` **is** the stamp coordinate system (`build_stamp` indexes `row * cols + col`), a tileset whose metatiles were laid out spatially contiguous at, say, 6 or 12 wide has them scattered across the fork's 8-wide reflow: those metatiles are no longer rect-selectable as one stamp, and have to be placed tile by tile. Defensible while the fork has no settings store, but it is a workflow regression for any non-8-wide tileset, not just a cosmetic relayout. Maps also still *render* in the world panel as backgrounds/tilemaps. |
| PNG import wizard (crop, tileset/sprite/metasprite modes, cell grid, quantized preview, commit, source-PNG cleanup) | `ggo_import_panel` (F5.1 Task I2) — tileset-only | partial | Ported: crop rect + cell-grid overlay, quantized preview, commit to `.til`/`.pal` (assets-root-relative sidecars, overwrite-collision confirm), source-PNG cleanup confirm, "Import as tileset…" on a `.png` in the explorer, opening the result in `ggo_tileset_panel` on commit. **Sprite and Metasprite import modes are NOT ported, by design** — the domain model changed: a sprite is one frame and a metasprite is clips over frames, both assembled from tiles in `ggo_sprite_panel`, not decoded straight from a PNG, so there is no destination for a `.spr` import to land in. `WizardState::sprite_import`/`Mode::{Sprite,Metasprite}` moved into worldlib verbatim (Task I1, dependency-light so splitting the file wasn't worth it) but the panel never calls them. |
| Legacy `.meta.json` sidecar import | none | dropped | One-shot migration aid for a format generation that is already past. |
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
| Inspector: schema-driven typed fields (int/fixed/str/bool/vec2), asset dropdowns, MetaSprite clip dropdown, per-component Remove, "+ Add component…" | world panel `inspector` | ✓ | Commit on Enter **and** on blur — strictly better than ggo-ide's Enter-only. |
| MetaSprite `stem` "→" goto sprite on Assets | Inspector jump button beside the MetaSprite component's Remove, into `ggo_sprite_panel` (F5.3/E5) | ✓ | Resolves `{stem}.spr` under the OPEN DOCUMENT's asset root and re-relativizes into the worktree, since every explorer-driven open takes a worktree-relative path. The button exists only when a destination does: an unauthored stem gets no button rather than a disabled one or a sprite panel parked in an error state. |
| Background merging from instances (priority, first-claimant, drawn at origin) | ✓ (worldlib `backgrounds::MergedBackground`) | ✓ | |
| Undo / redo / dirty / Save (`WorldDocStore` + `write_world`) | ✓ | ✓ | |
| "Emulate" (save, full-system build with this boot world, jump to Emulator) | Project-panel context menu: **Emulate this world (cart)**, contributed by `ggo_emu_panel` (F5.2/S4) | partial | Save-if-dirty → `emd pack-ggo --world <stem>` → run in the emu panel, reusing its existing run path (dock focused). CART mode, not ggo-ide's full-system boot (`emubuild::build_full_system_with_world`, GemOS image, FAT card) — see the Known-deferrals entry below for the three concrete losses (no OS boot, different perf profile, no run-kind column yet). |
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
| Project tab: structured `emerald.toml` viewer | Open `emerald.toml` in the editor | partial | Text, not a structured section/entry view. |
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
| "Build & run project" (full system: OS + FAT image + game) | `emd: run` task (launches the standalone `ggo-emu`, with audio) | partial | The panel runs `.cart` files only. The full-system build+boot path stays external. |
| Run / Stop | ✓ (`ctrl-alt-r` / `ctrl-alt-s`; Run over a running cart restarts cleanly) | ✓ | |
| Cart library (`~/.ggo/ggo-ide/carts`: list, Upload with magic-header validation, Remove) | Zed project panel — clicking a `.cart` there selects it in the emu panel, routed by `ggo_emu_panel::intercept_cart_open` | partial | Browsing instead of a managed library — no upload, no delete, no header validation (a bad cart fails loudly at Run, deliberately). Explorer-driven since F4 X4: the panel's own `.cart` dropdown and its filesystem walk are gone, so this row is now the project panel plus the interceptor. A click **selects** the cart; Run stays an explicit user action, because starting a cart spawns an emulator thread and takes over the keyboard. Clicking a different cart mid-run stops the running one through the normal `finish_run` path, so its perf data still reaches `ggo_ide.db`. |
| Keyboard → gamepad (18-button cart map) | ✓ (18-bit level-triggered mask, 10 of 17 keys pinned key-by-key against the standalone binary) | partial | Only the cart map exists (`emu_panel/src/input.rs`); ggo-ide's second full-system `FS_BTN_*` map has no fork equivalent, since the panel has no full-system mode. Focus-scoped, so pad keys never leak into an editor. |
| Audio + Mute/Unmute + underrun counter | `emd: run` / standalone `ggo-emu` | deferred | Explicitly out of scope for F3 (`constraints.md`); the standalone binary remains the with-audio path. |
| Stats line (fps / drops / step+blit ms) | ✓ | ✓ | |
| Live guest UART console (collapsible, N lines, 2000-line ring) | ✓ (plus cart `log()` output, via ggo PR #75's `Peripherals::log_sink`) | ✓ | |
| End-of-run perf ingest into `ggo_ide.db` (+ status line, + "View in Reports") | ✓, natively — Stop / cart exit / fault / restart all funnel through one `finish_run` | ✓ | Subsumes the spec's CLI-chained `ggo-emu --perf` → `ggo-cli ingest` task. |
| Implicit pause on navigating away, resume on return | none | dropped | Docks have no page crossing; the cart keeps running when the dock is hidden. |
| Reports "Re-run" entry point | none | deferred | Charts panel → emu panel handoff unwired (the reverse direction shipped). |
| Build error surfaced in a scrollable read-only editor | Terminal output of the `emd:` tasks | ✓ | |

## 7. Reports page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Carts → Runs → Detail drill-down with slim back headers | `ggo_charts_panel`: flat run picker (cart · label · started_at) → detail with a Back button | partial | The cart level is collapsed away; there is no per-cart grouping or last-run relative time. |
| Per-cart "Re-run" | Project-panel context menu: **Re-run (perf)** on a `.cart`, contributed by `ggo_emu_panel` (F5.2/S4) | ✓ | Different surface than ggo-ide's (a context-menu entry on the cart file, not a button inside the Reports page), but the capability is complete end to end: re-runs the cart through the panel's own run path and, once perf ingest lands, focuses the charts panel and opens that run — the reverse hop of the "View in Reports" one §1's cross-page-handoffs row already credited. |
| Run detail header + config line (budget, wire-model constants, wire-wait tag) | Run title only | deferred | C2's not-ported list. |
| "Copy for agent" text export | none | deferred | Same list. |
| Failed-asset-loads table, Panics table | none | deferred | Same list. |
| Guest UART console for a stored run | none | deferred | The emu panel shows the *live* console; the ingested one is written to the DB but never read back. |
| Ignored-frames editor (chips, comma/space input, "N of M excluded") | Default frame-0 ignore + a muted `"N frame(s) ignored"` caption | partial | Parity of the *filter* shipped; the editor did not — an ignore set is a UI task, not a port. |
| KPI tile rows (Frames, Over-budget, Avg wire vs budget, IPC, I$/D$ hit rate, max misses, conditional PPU/APU tiles) | none | deferred | C2's not-ported list; the panel has no KPI row at all. |
| Chart set (wire vs budget, wire breakdown, cache misses, syscalls, tile working set, I$ misses + evictions by function, i/d-miss histograms, PPU evictions, tile-load wire, APU fetch wire, instructions) | ✓ — `chart_set::build_charts` mirrors `reports.rs::charts_section`, same order and same gating | ✓ | |
| Historic overlay (up to 5 prior runs, age-faded) | none | deferred | C2's not-ported list. |
| Click-to-inspect frame → per-function misses/evicted table | none | deferred | Same. |
| I$ profile table with sortable header | none | deferred | Same. |
| Reads `~/.ggo/ggo_ide.db` off-thread, stale-response-guarded | ✓ — the same DB file, not a copy; load-generation guards on both the list and the detail | ✓ | |

## 8. Device page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Serial port picker (scan `/dev/serial/by-id`, auto-select a lone port, refresh) | `GGO_TTY` env var read by the tasks | partial | No discovery UI; you set the variable or accept `/dev/ttyUSB0`. |
| Flash `.ggo` via `ggo-diag --provision --launch` (file picker + magic check, Full-run/Skip-PnR toggle, collect seconds, baud) | `ulx3s: flash bitstream (fujprog)` task | partial | The task flashes a **bitstream** with `fujprog`, which is the canonical `scripts/fpga-test` invocation — it is not the `ggo-diag` provision+launch flow with its options. |
| "Run hardware diagnostics" (built-in diagnostic cart, no project needed) | Project-panel context menu: **Run hardware diagnostics**, contributed by `ggo_emu_panel` (F5.2/S4) | partial | `ggo-diag --tty <port> --skip-pnr --launch`, gated to a right-click on a directory (any directory, not tied to an emerald project — the spec's "no project needed"). Needs a GGO repo checkout (`GGO_REPO`) and a board; missing either names exactly what's missing on the status row and spawns nothing. One-shot, not streamed, no cancel, no baud/collect-seconds/port picker — ggo-ide's Device page is the model for all three, unbuilt here. |
| Live run log with stick-to-bottom autoscroll, Cancel, PASS/FAIL/TIMEOUT verdict | Terminal panel output; Ctrl-C | partial | Raw stdout with no verdict parsing and no state machine. |
| History rail (clone `~/.ggo/diag.db`, 50 recent runs, per-run log viewer) | none | deferred | Nothing clones or reads `diag.db` in the fork. |
| UART monitor | `ulx3s: UART monitor` task (mirrors `fpga-test`'s `configure_tty`, documents the FT231X re-enum gap) | ✓ | |

## 9. Shared chart widgets

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Line / StackedArea / Histogram with nice-step gridlines, compact tick labels, axis captions | `chart_geom` (renderer-independent `ChartScene`) + `chart_paint` (gpui) | ✓ | |
| Hover crosshair + flipping tooltip | ✓ | ✓ | |
| Dashed budget line participating in the y-scale | ✓ | ✓ | |
| Click-drag x-zoom + double-click reset | none | deferred | C2's not-ported list. |
| Click-to-select a frame (drives the inspect panel) | none | deferred | Dead without the inspect panel (§7). |
| Historic overlays (grey, age-ramped opacity, contribute to y-scale) | none | deferred | Same list. |
| Cached static layer, cheap hover redraws | Scenes rebuilt on hover | partial | Hover `notify()` rebuilds every chart's scene; fine at real run sizes, a documented ceiling at the 100k-frame cap. |

## 10. Settings page

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Theme | Zed settings | ✓ | |
| Repository path | `GGO_PROJECT_DIR` env (tasks) / the open worktree | partial | No stored setting; the tasks default to `..` and the panels use the worktree root. |
| Serial device (tty) + scanned-port dropdown | `GGO_TTY` env | partial | Free-form env var, no scan, no picker. |
| Baud rate (validated positive integer) | `GGO_BAUD` env | partial | No validation; a bad value fails at `stty`. |
| `emd` binary path (blank = resolve from PATH) | PATH | partial | The override is gone; `emd` must be on PATH. |
| Version/environment row + `emd` re-check | none | deferred | Nothing probes `emd --version` in the fork. |
| Per-key persistence in the settings DB | `.zed/settings.json` (repo-shared) + shell env (per-user) | ✓ | Split exactly as the spec's disposition table proposed. |

## 11. Fork-only additions (no ggo-ide equivalent)

| Feature | Where | Notes |
|---|---|---|
| "GGO World" language: tree-sitter grammar, syntax highlighting, bracket/quote autoclose, outline symbols for `[[entity]]`/`[[instance]]`/`[[background]]` | `ggo_language` + the repo's `.zed/settings.json` `file_types` glob | Native registration (no extension sandbox), grammar via `tree-sitter-toml-ng` since upstream extracted TOML. **Does not activate today**: the `.zed/settings.json` that carries the `file_types` glob lives in the `ggo` repo, which has zero `worlds/*.toml` files; real world files live in game projects (e.g. `~/projects/wilds/worlds/`), which have no `.zed/` directory of their own. The glob has to be copied into each game project's `.zed/settings.json` for the language to apply there — a per-project setup step, not shipped by this fork. This is a config-placement gap, not a code gap; fixing it is copying config into the game repos, not moving anything between `ggo` and this fork. |
| `.zed/tasks.json` task layer (6 tasks) + `scripts/check-zed-config.sh` JSONC validator | ggo repo | Committed, per-user values via env, all six verified against real invocations. |
| Native perf ingest straight from the emulator pane into `ggo_ide.db` | `ggo_emu_panel::ingest` | Replaces the spec's CLI-chained ingest task. |
| **LSP** (diagnostics, completion, hover, code actions on worlds/manifests) | — | The pure-extension spec's central mechanism. **Deferred**: `ggo_language` registers grammar + queries only; there is no `ggo-lsp` binary and no `LspAdapter`. Every capability the old spec assigned to the LSP (world validation, hardware-budget diagnostics, snap/center/offset code actions, import diagnostics) is therefore unbuilt. |
| Legend band on chart widgets | `chart_geom.rs` `legend_layout` (:612), `legend_height` (:642), consumed at :783-811 | ggo-ide's charts have no legends (a deliberate omission); the fork adds one. Documented as a divergence in the module doc (`chart_geom.rs:25-29`). §9's "No legends" row does not apply here — this is new surface, not parity. |

---

## Counts

| Status | Rows |
|---|---|
| ✓ | 44 |
| partial | 34 |
| deferred | 15 |
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

(Counted over §§1-10; §11's fork-only rows are not dispositions of a ggo-ide
feature and are excluded, except that the LSP row there is a deferral in its
own right.)

Read that shape honestly: the panels the fork set out to build (world,
sprite, charts, emulator) are largely ✓, and as of F5.3 so is Emerald ops —
the mutations run in-panel, gated on the version lock, rather than in a
terminal. The remaining *ancillary* pages — Device, Settings, Reports'
non-chart furniture, and Emerald's `emerald.toml` viewer — are still mostly
"partial via a task or a text file"; and the pixel-art half of Assets is a
deliberate drop with no replacement in this repo.

---

## Known deferrals (consolidated ledger)

Open items carried out of F1-F3 that are not feature-disposition rows — bugs,
guards and follow-ups that someone has to pick up:

**worldlib / ggo repo**

- **PaletteSet slot guard** and **last-frame `FrameDelete` guard** — the next
  worldlib doc-op hardening PR (follow-on to ggo #73). Panels re-check both
  before applying, so this is defence in depth, not a live crash.
- **Redo of an add leaves the instance subtree unresolved** — `AddInstance`
  resolves the subtree at add time (fork `ade65c4d`), but a redo of that op
  replays the doc op without the resolve, so the instance renders as a
  placeholder until the world is reopened.
- **`ggo-db` index PR**: `frame(run_id)` and `profile(run_id)` have no index;
  the charts panel's per-run sample query full-scans. Not felt at current run
  counts.
- **`scripts/check-zed-config.sh` trailing-comma stripping** can mangle string
  content containing `,]` or `,}` (fix-forward, low: no such string exists in
  either committed file today).

**fork**

- **Atlas retention in the two document panels.** Only `ggo_emu_panel`
  implements the `Window::drop_image` release contract (double-buffered retire,
  full release on stop, `on_release` at teardown). `ggo_world_panel` rebuilds
  its whole image cache on add-instance and on every world switch, and
  `ggo_sprite_panel` rebuilds every frame + pool-tile `RenderImage` after
  *every* op/undo/redo/save — neither ever calls `drop_image`, so both leak
  atlas tiles at edit frequency. Bounded by session length, not by a loop, so it
  was not an F2/F3 blocker; it is still a real leak.
- **Linear-upscale blur in the emu pane.** The framebuffer is painted with
  `img().w_full().h_full()`, i.e. gpui's default filtering, where ggo-ide
  hard-coded a 2× integer nearest-neighbour scale. Violates the
  "integer-scale pixel rendering" guardrail in the feature inventory.
- **Stuck keys on window deactivation.** `on_focus_out` clears the pad mask,
  but alt-tabbing away with a key held while the panel keeps focus leaves the
  bit set until the key is pressed and released again.
- **`MAX_FRAMES = 100_000` ingest rejection.** Ported verbatim from ggo-ide for
  parity: a run longer than roughly 28 minutes at 60 Hz is rejected outright at
  ingest rather than truncated.
- **Frame-0 ignore-set editor.** The charts panel applies ggo-ide's default
  (drop frame 0) and captions it, but there is no way to change the set.
- **Hover rebuild ceiling.** Every hover move rebuilds all chart scenes;
  O(charts × frames) per mouse-move frame.
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
     the rel path). There is no historic overlay yet to conflate them on one
     chart (`chart_geom.rs`'s own doc: "no historic overlay ... the panel has
     no prior-run picker yet") — the real risk today is in the run **picker
     list** (C1): a cart-mode run and a full-system run of the same game
     share one `cart_name`, so two rows can read as just `"demo"` next to
     each other with nothing marking which is which except an opaque
     free-text `label`, and ggo-ide's own runs leave `label` unset entirely.
     Until a mode column exists, the only discriminator is `label`: `None` =
     ggo-ide, `target/ggo-emulate/*.ggo` = this entry, anything else = a cart
     clicked in the fork's explorer — none of that is surfaced in the picker
     UI today. **F5.3 input**: add an explicit run-kind column (and show it
     in the picker) rather than leaning on that convention; the same column
     would also be what a future historic overlay needs to avoid mixing
     modes once a prior-run picker exists.
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
- **Hardware diagnostics is one-shot, with no streaming, cancel or options
  (F5.2/S4).** The "Run hardware diagnostics" entry spawns
  `ggo-diag --tty <port> --skip-pnr --launch` and shows the transcript when it
  finishes. ggo-ide's Device page streams lines live, can cancel the child,
  offers baud / collect-seconds / port pickers and clones `diag.db` rows back
  into `ggo_ide.db` afterwards; none of that is here. The repo checkout must
  also be pointed at with `GGO_REPO` — ggo-ide lives inside the repo and
  auto-detects from `CARGO_MANIFEST_DIR`, which a fork whose worktree is the
  user's game project cannot do.
