# ggo-ide → fork: feature disposition audit

Date: 2026-08-08 (end of F3), rows amended 2026-08-09 (end of F4: explorer-driven
panel routing + the read-only tileset viewer; then X4, which removed the last
in-panel file picker — the emulator's `.cart` dropdown). Fork branch `ggo`.

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
| Cross-page handoffs (Reports Re-run, World Emulate, inspector "→" sprite, Emulator View in Reports) | emu → charts "View in Reports" only | partial | Only that one hop shipped; the other three are listed separately below. |
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
| New tileset / new map | none | deferred | Same — no CLI, no panel. |
| Files tree (create folder, delete file/dir, extension badges) | Zed project panel | partial | Create/delete/rename exist; the GGO extension badges do not. |
| **Pixel editor**: 14 tools, brush sizes, mirror, pixel-perfect, shapes, marquee/lasso/wand, floating-selection move/flip/rotate, eyedropper, zoom/pan, per-editor undo | none | dropped | Explicit spec scope call. Pixel authoring is an external editor plus an import path. |
| **Palette panel**: RGB565 16-slot editor, draft picker (5/6/5 sliders, 565 + 888 hex, quantized preview), swap / sort / ramp, shared-palette warnings | none | dropped | Part of the pixel-editor scope not taken. |
| **Tile-pool panel** + dedup preview/apply | none | dropped | Same. Dedup still happens implicitly on metasprite save (worldlib's fold-back). |
| Animation timeline: transport, clip lanes (name/from/to/loop/delete/add), frame strip (select, add, duplicate, delete, move, per-frame ms), playback honouring durations + clip range + loop | `ggo_sprite_panel` | ✓ | |
| Onion skin (toggle, back/fwd ghost counts, opacity) | none | deferred | The timeline was ported without ghost compositing; no blocker beyond effort. |
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
| "+ New world" (snake_case validated, overwrite confirm) | none | deferred | Carried on the F2 gap list. worldlib can `write_world`; the panel has no create affordance. |
| Delete world (confirm + rescan) | Project-panel context menu: **Delete World**, contributed by `ggo_world_panel` (F5.0) | ✓ | Confirms (naming the world by stem AND file, and saying so when the open document has unsaved edits), unlinks, clears the panel if that world was open, and re-enumerates so `+ Instance` stops offering it. One thing ggo-ide didn't do either: `[[instance]]` references to the deleted world are not chased down, so they now dangle — see the Known-deferrals note about `loader.rs`'s unrendered `node["error"]`. |
| Canvas rendering: sprites, metasprites (per-clip), tilemaps, text, rect fills, transform markers, instance gizmos, merged backgrounds, error placeholders, selection outline | world panel `canvas` + `loader` (worldlib compose / `build_draw_list`) | ✓ | |
| Click-select + drag-move with one undo entry per gesture | ✓ | ✓ | One deliberate divergence: empty-space left-drag deselects instead of panning; pan is middle-drag. |
| Wheel zoom / pan | ✓ (cursor-anchored zoom, middle-drag pan) | ✓ | Zoom is cursor-anchored here, which ggo-ide's is not. |
| Snap-to-tile checkbox | ✓ | ✓ | |
| Grid checkbox, "Reset" view, preview-size stepper (`- Preview Nx +`) | none | deferred | F2 final-review gap list; the canvas chrome row shipped with Snap only. |
| Sidebar entity/instance lists | none (selection is canvas-driven; inspector shows the selection) | partial | No list-based selection or navigation; a hard-to-click entity has no list fallback. |
| "+ Entity" (default Transform at view centre) | Add-entity toolbar button | ✓ | |
| "+ Merge" fuzzy world search → add instance | "+ Instance" dropdown over cycle-guarded `merge_candidates` | partial | Instance add shipped, including the cycle guard and immediate subtree resolve; the fuzzy-search picker UI became a plain dropdown. |
| Delete entity / remove instance | `Delete selected` button + `delete`/`backspace` | partial | ggo-ide confirms before removing an instance; the fork does not confirm anything. |
| Inspector: schema-driven typed fields (int/fixed/str/bool/vec2), asset dropdowns, MetaSprite clip dropdown, per-component Remove, "+ Add component…" | world panel `inspector` | ✓ | Commit on Enter **and** on blur — strictly better than ggo-ide's Enter-only. |
| MetaSprite `stem` "→" goto sprite on Assets | none | deferred | Cross-panel navigation (world → sprite panel) is unwired; on the F2 gap list. |
| Background merging from instances (priority, first-claimant, drawn at origin) | ✓ (worldlib `backgrounds::MergedBackground`) | ✓ | |
| Undo / redo / dirty / Save (`WorldDocStore` + `write_world`) | ✓ | ✓ | |
| "Emulate" (save, full-system build with this boot world, jump to Emulator) | `emd: run` task, or the emu panel on an already-packed `.cart` | deferred | On the F2 gap list. The panel cannot bake a boot world, and no task takes one. |
| Arrow-key nudge (1 px / 16 px with Shift) | none | deferred | Not ported; drag + snap is the only placement path. |
| Confirm dialogs (delete world, overwrite, remove instance, remove Transform from a visual entity) | none | dropped | See §1 — no confirm system in the fork. |

## 5. Emerald page (ECS manifests)

The pure-extension spec expected "full for ops; no dashboard view". The fork
landed neither the dashboard nor the ops surface: manifests are text files you
edit and `emd` is a command you run.

| ggo-ide feature | Fork-era answer | Status | Rationale (non-✓) |
|---|---|---|---|
| Project tab: structured `emerald.toml` viewer | Open `emerald.toml` in the editor | partial | Text, not a structured section/entry view. |
| Components / Systems / Schedules browsing (module groups, list → detail) | Open `manifests/*.toml` in the editor | partial | Text only. No list/detail dashboard, no field counts, no "used by schedules". |
| Create component/system/schedule (validated forms → `emd generate …`) | `emd` in the terminal | deferred | T1 shipped build/pack/run/flash/monitor/new tasks only; the generate/rm/field/schedule verbs have no task and no panel. |
| Remove component/system/schedule, add/remove field (with cascade-aware and compiler-check confirms) | `emd` in the terminal | deferred | Same. The "Reverted" compiler-rollback surfacing is lost with it. |
| Schedule ordered run-list editor (reorder, cadence, add/remove, optimistic commit) | `emd schedule set` by hand | deferred | Richest interaction in ggo-ide; nothing ported. |
| emd version-lock banner + gating + mtime poll + mid-run drift check | none | deferred | Nothing gates `emd` invocations in the fork; a version-skewed `emd` fails at the terminal instead of being pre-empted. |
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
| Per-cart "Re-run" | none | deferred | Needs the charts → emu handoff (see §6). |
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
| "Run hardware diagnostics" (built-in diagnostic cart, no project needed) | none | deferred | No task wraps `ggo-diag`; T1 stayed inside verbs it could verify. |
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
| ✓ | 33 |
| partial | 31 |
| deferred | 28 |
| dropped | 11 |
| **total** | **103** |

(F4 wrap: the `.til` tileset-editor row moved dropped → partial now that
`ggo_tileset_panel` ships a read-only viewer. Before F4: ✓ 31 / partial 28 /
deferred 31 / dropped 13. The sprite/world-picker rows also changed *text*
(explorer-driven instead of in-panel pickers) but not *status* — both were
already ✓/partial and stay there.)

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

(Counted over §§1-10; §11's fork-only rows are not dispositions of a ggo-ide
feature and are excluded, except that the LSP row there is a deferral in its
own right.)

Read that shape honestly: the panels the fork set out to build (world,
sprite, charts, emulator) are largely ✓; the *ancillary* pages — Emerald
ops, Device, Settings, Reports' non-chart furniture — are mostly "partial via a
task or a text file"; and the pixel-art half of Assets is a deliberate drop
with no replacement in this repo.

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
