# World-Hosted Map Editing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Retire the standalone `.map` editor tab and move all map creation/editing into the world editor's canvas, painting linked `.map` files in place with entities and other layers visible.

**Architecture:** `ggo-worldlib` gains an undoable `WorldOp::SetBackground`. `ggo_map_panel` is stripped into a paint library exporting `PaintSession` (document store + tileset cache + tool state machine). `ggo_world_panel` hosts paint mode: a layers rail edits `[[background]]` slots, painting routes through per-stem `PaintSession`s, and save flushes dirty sessions alongside the world TOML. No format changes anywhere.

**Tech Stack:** Rust, GPUI. Two repos: `/home/clay/projects/zed` (panels, branch `ggo`) and `/home/clay/projects/ggo` (worldlib/formats — zed pulls both by path, so no version bumps).

**Spec:** `docs/superpowers/specs/2026-08-29-world-hosted-map-editing-design.md`

## Global Constraints

- Model split: implementation subagents run on Opus (`model: "opus"` on Agent calls).
- Never run `install-zedgg`; clay installs.
- Zed-repo verification gates every commit: `./script/clippy -p <crate> && cargo test -p <crate> --lib && git commit ...` (chained on exit codes, never echo-after-check).
- ggo-repo verification: `cd /home/clay/projects/ggo && cargo clippy -p ggo-worldlib --all-targets -- -D warnings && cargo test -p ggo-worldlib && git commit ...`.
- Worldlib changes commit in `/home/clay/projects/ggo`; panel/smoke changes commit in `/home/clay/projects/zed` on branch `ggo`. When a zed task depends on a worldlib change, the worldlib commit lands first.
- Fork hook rule (repo CLAUDE.md): interceptor/context-menu/drop hooks run while a `Pane`/`ProjectPanel` entity is leased — decide synchronously from path inspection; push pane-touching work into `cx.defer_in(window, ..)`.
- GPUI test timers: `cx.background_executor().timer(..)`, never `smol::Timer::after`.
- No `unwrap()` outside tests; no `let _ =` on fallible ops.
- Comments explain "why" only; match surrounding density and idiom (both panels are heavily doc-commented — keep that standard for new public items, skip narration comments).

---

### Task 1: `WorldOp::SetBackground` in ggo-worldlib

**Files:**
- Modify: `/home/clay/projects/ggo/tools/ggo-worldlib/src/world_doc.rs` (op enums ~line 165/220, `apply` ~437, `undo`/`redo` ~909/921; tests at bottom)
- Modify: `/home/clay/projects/ggo/tools/ggo-worldlib/src/sprites/io.rs` (add `save_new_bound_map` near `save_new_map` ~line 967)

**Interfaces:**
- Consumes: existing `Background` (`world_file.rs:72`, fields `layer: u8`, `map: String`), `BACKGROUND_LAYER_COUNT = 4`, `InverseOp` machinery (`push_undo`, `update_dirty`, stacks).
- Produces (later tasks rely on these exact shapes):
  - `WorldOp::SetBackground { layer: u8, map: Option<String> }`
  - `pub fn io::save_new_bound_map(project_dir: &Path, rel_path: &str, w: u16, h: u16, til_rel: &str) -> Result<(), IoError>`
  - Store behavior: no-op apply (same value) records nothing; slot list stays sorted by layer; `state().backgrounds` and `to_doc().backgrounds` reflect edits; undo/redo restore prior slot value; dirty tracking via the existing watermark.

- [ ] **Step 1: Write the failing tests** in `world_doc.rs`'s existing `#[cfg(test)] mod tests` (follow the local helper style — the tests there build `WorldDocWire` literals):

```rust
#[test]
fn set_background_sets_replaces_clears_and_round_trips_undo() {
    let mut store = WorldDocStore::new(WorldDocWire {
        entities: Vec::new(),
        instances: Vec::new(),
        backgrounds: vec![Background { layer: 2, map: "maps/old.bg2.map".into() }],
    });

    store.apply(WorldOp::SetBackground { layer: 0, map: Some("maps/a.bg0.map".into()) });
    let st = store.state();
    assert_eq!(
        st.backgrounds,
        vec![
            Background { layer: 0, map: "maps/a.bg0.map".into() },
            Background { layer: 2, map: "maps/old.bg2.map".into() },
        ],
        "slots stay sorted by layer"
    );
    assert!(st.dirty);

    store.apply(WorldOp::SetBackground { layer: 2, map: Some("maps/new.bg2.map".into()) });
    assert_eq!(store.state().backgrounds[1].map, "maps/new.bg2.map");

    store.apply(WorldOp::SetBackground { layer: 2, map: None });
    assert_eq!(store.state().backgrounds.len(), 1, "clear removes the slot");

    assert!(store.undo(), "undo the clear");
    assert_eq!(store.state().backgrounds[1].map, "maps/new.bg2.map");
    assert!(store.undo(), "undo the replace");
    assert_eq!(store.state().backgrounds[1].map, "maps/old.bg2.map");
    assert!(store.undo(), "undo the add");
    assert_eq!(store.state().backgrounds.len(), 1);
    assert!(!store.state().dirty, "back at the saved depth");
    assert!(store.redo());
    assert_eq!(store.state().backgrounds[0].map, "maps/a.bg0.map");
}

#[test]
fn set_background_noop_and_out_of_range_record_nothing() {
    let mut store = WorldDocStore::new(WorldDocWire {
        entities: Vec::new(),
        instances: Vec::new(),
        backgrounds: vec![Background { layer: 1, map: "maps/a.map".into() }],
    });
    store.apply(WorldOp::SetBackground { layer: 1, map: Some("maps/b.map".into()) });
    assert!(store.state().dirty);

    // Same value again: must not push undo or clear the redo stack.
    assert!(store.undo());
    store.apply(WorldOp::SetBackground { layer: 1, map: Some("maps/a.map".into()) });
    assert!(store.redo(), "a no-op apply must not have cleared redo");
    assert_eq!(store.state().backgrounds[0].map, "maps/b.map");

    // Out-of-range layer: op is refused entirely.
    store.apply(WorldOp::SetBackground { layer: 4, map: Some("maps/x.map".into()) });
    assert_eq!(store.state().backgrounds.len(), 1);
    assert!(!store.undo() || store.state().backgrounds[0].map != "maps/x.map");
}

#[test]
fn to_doc_carries_background_edits() {
    let mut store = WorldDocStore::new(WorldDocWire {
        entities: Vec::new(),
        instances: Vec::new(),
        backgrounds: Vec::new(),
    });
    store.apply(WorldOp::SetBackground { layer: 3, map: Some("maps/w.bg3.map".into()) });
    let doc = store.to_doc();
    assert_eq!(doc.backgrounds, vec![Background { layer: 3, map: "maps/w.bg3.map".into() }]);
}
```

Also in `io.rs`'s test module:

```rust
#[test]
fn save_new_bound_map_writes_a_blank_map_bound_to_the_tileset_pair() {
    let dir = tempfile::tempdir().unwrap();
    // reuse the module's existing tileset-writing test helper to create tiles/bg.til + .pal
    // (see the save_tileset-based helpers already in this test module)
    let opened_err = open_map(dir.path(), "maps/w.bg0.map");
    assert!(opened_err.is_err(), "not written yet");
    save_new_bound_map(dir.path(), "maps/w.bg0.map", 16, 16, "tiles/bg.til").unwrap();
    let data = open_map(dir.path(), "maps/w.bg0.map").unwrap();
    assert_eq!((data.w, data.h), (16, 16));
    assert_eq!(data.til_path, "tiles/bg.til");
    assert_eq!(data.pal_path, "tiles/bg.pal", "pal derived by the single-sourced pairing rule");
    assert!(data.cells.iter().all(|&c| c == crate::sprites::map_doc::CELL_BLANK));
}
```

- [ ] **Step 2: Run the tests, verify they fail to compile** (missing variant/function):
`cd /home/clay/projects/ggo && cargo test -p ggo-worldlib set_background` — expect compile errors naming `SetBackground` / `save_new_bound_map`.

- [ ] **Step 3: Implement.** In `world_doc.rs`:

Add to `WorldOp` (after `MoveInstance`):
```rust
    /// Set (`Some`), replace, or clear (`None`) one hardware background
    /// slot. Never touches the `.map` file itself -- unlink only, since
    /// other worlds/instances may share the map and unlink is undoable
    /// while deletion is not.
    SetBackground {
        layer: u8,
        map: Option<String>,
    },
```

Add to `InverseOp`:
```rust
    SetBackground {
        layer: u8,
        old_map: Option<String>,
        new_map: Option<String>,
    },
```

Private helpers on `WorldDocStore` (near `set_transform_pos`):
```rust
    fn background_map(&self, layer: u8) -> Option<String> {
        self.backgrounds
            .iter()
            .find(|b| b.layer == layer)
            .map(|b| b.map.clone())
    }

    fn set_background_slot(&mut self, layer: u8, map: Option<String>) {
        self.backgrounds.retain(|b| b.layer != layer);
        if let Some(map) = map {
            self.backgrounds.push(Background { layer, map });
            self.backgrounds.sort_by_key(|b| b.layer);
        }
    }
```

`apply` arm (the existing `!matches!(.. Move ..)` guard at the top of `apply` already seals gestures for any new op — no change needed there):
```rust
            WorldOp::SetBackground { layer, map } => {
                if (layer as usize) >= crate::world_file::BACKGROUND_LAYER_COUNT {
                    return;
                }
                let old_map = self.background_map(layer);
                if old_map == map {
                    return;
                }
                self.set_background_slot(layer, map.clone());
                self.push_undo(InverseOp::SetBackground {
                    layer,
                    old_map,
                    new_map: map,
                });
            }
```

In `undo()`/`redo()`, follow the existing arm pattern exactly (pop entry, apply the appropriate side, move entry to the other stack): undo applies `old_map` via `set_background_slot`, redo applies `new_map`. Also update the store's module doc — the "`backgrounds` … read-only (no editing UI exists for it yet)" paragraph (~line 74-80) is now false; rewrite it to say `SetBackground` edits slots and the panel's layers rail is the UI.

In `io.rs`, next to `save_new_map` (:967), using the module's own `tileset_pal_path` (:617):
```rust
/// A blank `w`x`h` map already bound to `til_rel` (palette derived by the
/// single-sourced pairing rule) -- what the world panel's add-background
/// flow writes, so a fresh layer is paintable without a follow-up bind.
pub fn save_new_bound_map(
    project_dir: &Path,
    rel_path: &str,
    w: u16,
    h: u16,
    til_rel: &str,
) -> Result<(), IoError> {
    let state = MapState {
        w,
        h,
        cells: vec![CELL_BLANK; w as usize * h as usize],
        til_path: til_rel.to_string(),
        pal_path: tileset_pal_path(til_rel),
        dirty: false,
    };
    save_map(project_dir, rel_path, &state)
}
```
(Match `save_new_map`'s actual construction — if it builds `MapState` differently or `tileset_pal_path` has a different signature, mirror what's there; the contract is the test in Step 1.)

- [ ] **Step 4: Run tests, verify pass:** `cd /home/clay/projects/ggo && cargo test -p ggo-worldlib`
- [ ] **Step 5: Verify zed side still builds against the changed lib:** `cd /home/clay/projects/zed && cargo check -p ggo_world_panel -p ggo_map_panel`
- [ ] **Step 6: Commit (ggo repo):**
```bash
cd /home/clay/projects/ggo && cargo clippy -p ggo-worldlib --all-targets -- -D warnings && cargo test -p ggo-worldlib && git add -A tools/ggo-worldlib && git commit -m "worldlib: WorldOp::SetBackground + io::save_new_bound_map

Backgrounds stop being a read-only pass-through: the world editor's
layers rail needs an undoable set/replace/clear per hardware slot, and
its add-layer flow writes a blank map already bound to a tileset."
```

---

### Task 2: Extract `PaintSession` inside `ggo_map_panel`

Pure refactor — the standalone panel keeps working and its full test suite stays green. This carves out exactly the code that survives the panel's later deletion.

**Files:**
- Create: `crates/ggo/map_panel/src/paint_session.rs`
- Modify: `crates/ggo/map_panel/src/ggo_map_panel.rs` (move logic out of `OpenMap`/`MapPanel`; `OpenMap` keeps only view state and embeds a session)
- Modify: `crates/ggo/map_panel/Cargo.toml` only if a `[features] test-support` entry is missing (Task 4's world-panel tests will construct sessions; check first — the crate may already have one)

**Interfaces:**
- Consumes: `MapDocStore`, `MapOp`, `loader::{Tileset, LoadedMap, load_map, load_tileset, compose_strip, compose_live_image, compose_live_rgba, tileset_meta, tileset_meta_rel}`, `terrain::resolve`, `geom`.
- Produces — the library surface later tasks build on (all `pub`, exported from `ggo_map_panel`'s lib root alongside `pub mod loader`):

```rust
// paint_session.rs
pub use crate::MapTool; // make the existing enum + its ALL/icon/label/id pub

pub struct PaintSession {
    /// Asset-root-relative `.map` path -- the frame `io::save_map` writes in.
    pub rel_path: String,
    /// Asset root captured at load ("save where it was read from").
    pub root: PathBuf,
    pub store: MapDocStore,
    pub tileset: Option<loader::Tileset>,
    pub tileset_error: Option<String>,
    pub strip: Option<Arc<RenderImage>>,
    pub tool: MapTool,
    pub hflip: bool,
    pub vflip: bool,
    pub pal_sub: u16,
    pub pal_anchor: (i32, i32),
    pub pal_far: (i32, i32),
    pub rect_pending: Option<(i32, i32, i32, i32)>,
    pub sel_pending: Option<(i32, i32, i32, i32)>,
    pub selection: Option<(i32, i32, i32, i32)>,
    pub paint_erase: bool,
    pub terrains: Vec<Terrain>,
    pub terrain: Option<usize>,
    pub til_meta_rel: Option<String>,
    pub terrain_error: Option<String>,
    pub save_error: Option<String>,
}

impl PaintSession {
    pub fn new(rel_path: String, root: PathBuf, loaded: loader::LoadedMap) -> Self;
    /// Off-thread-friendly load (wraps loader::load_map).
    pub fn load(root: &Path, rel: &str, project_root: &Path) -> Result<Self, String>;

    pub fn current_stamp(&self) -> Stamp;      // moved from OpenMap
    pub fn fill_cell(&self) -> u16;            // moved from OpenMap
    pub fn dirty(&self) -> bool;               // store.dirty()

    /// One tool application at `cell`. Returns true when the DOCUMENT
    /// changed (host must recompose); Select/Eyedropper/RectFill-pending
    /// mutate only session view state and return false.
    pub fn paint_at(&mut self, cell: (i32, i32)) -> bool;

    /// RectFill mouse-up: commit `rect_pending` as one MapOp::RectFill.
    pub fn commit_rect(&mut self) -> bool;
    /// Select mouse-up: settle `sel_pending` into `selection`.
    pub fn commit_selection(&mut self);

    pub fn apply(&mut self, op: MapOp);        // store.apply, nothing else
    pub fn eyedrop(&mut self, x: i32, y: i32); // moved from MapPanel::eyedrop

    /// Undo/redo with the bind-resync (moved from MapPanel::step_history).
    pub fn step_history(
        &mut self,
        step: fn(&mut MapDocStore) -> bool,
        project_root: Option<&Path>,
    ) -> bool;

    /// Moved from MapPanel::bind_tileset (resolve-first-then-apply).
    pub fn bind_tileset(&mut self, til_rel: String, project_root: Option<&Path>);

    /// Live compose of the current document, for the host's image cache.
    pub fn live_rgba(&self) -> Option<(Vec<u8>, u32, u32)>;
    pub fn live_image(&self) -> Option<Arc<RenderImage>>;

    /// io::save_map to `self.root`/`self.rel_path`, then mark_saved.
    /// Failure lands in `self.save_error` AND returns Err.
    pub fn save(&mut self) -> Result<(), String>;
}
```

- [ ] **Step 1: Write the pinning tests first** in `paint_session.rs`'s own `#[cfg(test)] mod tests` (no gpui needed — this is pure logic; model the fixtures on `loader.rs`'s `write_tileset` test helper):

```rust
#[test]
fn brush_paints_the_default_stamp_and_select_tracks_pending() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 2-tile tileset + a 4x4 bound blank map, via worldlib io (same shape
    // as loader.rs's the_live_compose_tracks_an_unsaved_edit fixture).
    write_tileset(root, "fx", 2);
    let state = MapState {
        w: 4, h: 4,
        cells: vec![CELL_BLANK; 16],
        til_path: "tiles/fx.til".into(),
        pal_path: "tiles/fx.pal".into(),
        dirty: false,
    };
    io::save_map(root, "maps/m.map", &state).unwrap();

    let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
    assert!(!session.dirty());

    assert!(session.paint_at((1, 1)), "brush changes the document");
    let st = session.store.state();
    assert_eq!(st.cells[1 * 4 + 1], pack_cell(0, 0, false, false));
    assert!(session.dirty());

    session.tool = MapTool::Select;
    assert!(!session.paint_at((0, 0)), "select never touches the document");
    assert!(session.paint_at((2, 1)) == false);
    session.commit_selection();
    assert_eq!(session.selection, Some((0, 0, 2, 1)));

    assert!(session.step_history(MapDocStore::undo, None));
    assert!(!session.dirty(), "one undo reverts the single brush");
}

#[test]
fn save_writes_where_it_was_read_from_and_clears_dirty() {
    // fixture as above; paint one cell, session.save().unwrap(),
    // io::open_map(root, "maps/m.map") shows the painted cell,
    // session.dirty() is false.
}

#[test]
fn painting_an_unbound_map_is_inert() {
    // io::save_new_map fixture; PaintSession::load succeeds with
    // tileset_error set; paint_at returns false and cells stay CELL_BLANK.
}
```

- [ ] **Step 2: Run, verify compile failure:** `cargo test -p ggo_map_panel --lib paint_session` (from `/home/clay/projects/zed`).

- [ ] **Step 3: Implement by moving, not rewriting.** Mechanical inventory (source lines in `ggo_map_panel.rs` as of `6813a8b0e5`):
  - `MapTool` (:412-472) → `paint_session.rs`, made `pub` (keep `icon`/`label`/`id` — the world panel's tool rail reuses them; `IconName` import moves too).
  - From `OpenMap` (:491): fields listed in the Interfaces block move to `PaintSession`; `OpenMap` keeps `source_rel`, `tilesets`, `show_grid`, `zoom`, `pan`, `canvas_bounds`, `strip_bounds`, `pan_drag`, `pal_dragging`, `hover_cell`, `mask_draft`, `terrain_name`, `painting`, `resize`, plus a new `session: PaintSession` field. `image: Option<Arc<RenderImage>>` stays on `OpenMap` (it's the standalone canvas's display cache; the world panel uses its own image cache instead).
  - `OpenMap::current_stamp`/`fill_cell` (:608-636) → `PaintSession`.
  - `MapPanel::eyedrop` (:1149), `step_history` (:959) + `set_tileset` (:990) + `resolve_tileset` (:1109) + `adopt_tileset_meta` (:1131), `bind_tileset` (:1081), the document half of `paint_at` (:1176) and the rect/select commit halves of the mouse-up handler → `PaintSession` methods per the Interfaces block. `resolve_tileset` loses its `&OpenMap` parameter (it only used `open.root` — pass `&self.root`).
  - `save_impl`'s body (:1016-1030) → `PaintSession::save`; `MapPanel::save_impl` becomes a thin wrapper that calls it and `cx.notify()`.
  - `MapPanel::apply_op`/`rebuild_image` (:908-926): `rebuild_image` becomes `open.image = open.session.live_image()`.
  - Every `open.<moved field>` reference in the render/mouse code becomes `open.session.<field>` — mechanical, compiler-driven.
- [ ] **Step 4: Full crate green:** `cargo test -p ggo_map_panel --lib` — the existing panel tests are the refactor's net; all must pass unmodified (test code may need `open.session.` path updates only).
- [ ] **Step 5: Export the surface** from the lib root: `pub mod paint_session; pub use paint_session::{PaintSession, MapTool};` and confirm `pub mod loader;` (world panel will call `loader::compose_strip` etc. through the session, but `Tileset` is in its signature).
- [ ] **Step 6: Commit:**
```bash
cd /home/clay/projects/zed && ./script/clippy -p ggo_map_panel && cargo test -p ggo_map_panel --lib && git add -A crates/ggo/map_panel && git commit -m "ggo map_panel: carve PaintSession out of the panel

Document store + tileset cache + tool state machine become a reusable
library surface; the standalone panel now delegates to it unchanged.
First step of world-hosted map editing (spec 2026-08-29): the session
is exactly the code that survives the panel's retirement."
```

---

### Task 3: Layers rail — add/remove background slots in the world panel

No painting yet: the rail displays the four slots, adds a layer (tileset pick → `save_new_bound_map` → `SetBackground`), and clears one. Merged backgrounds and the canvas refresh on every background change, including via undo/redo.

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs`
- Modify: `crates/ggo/world_panel/src/loader.rs` (retain instance backgrounds for re-merge)
- Modify: `crates/ggo/world_panel/Cargo.toml` (add `ggo_map_panel` dependency now — Task 4 needs it; workspace-path dep like the crate's existing `ggo_common` entry, plus `ggo_map_panel = { workspace = true, features = ["test-support"] }` under dev-dependencies if its test hooks are feature-gated)

**Interfaces:**
- Consumes: `WorldOp::SetBackground`, `io::save_new_bound_map` (Task 1), `merge_backgrounds`, `io::list_tilesets`, `io::compose_map_rgba`, `canvas::build_image_cache_reusing`, `canvas::image_key`.
- Produces:
  - `fn background_map_rel(world_stem: &str, layer: u8) -> String` (free fn, unit-tested)
  - `WorldPanel::add_background_impl(&mut self, layer: u8, til_rel: String, cx: &mut Context<Self>)`
  - `WorldPanel::clear_background_impl(&mut self, layer: u8, cx: &mut Context<Self>)`
  - `WorldPanel::refresh_backgrounds(&mut self, cx: &mut Context<Self>)` — recompute `open.merged`, compose any missing bg stem into `open.map_loads`, rebuild `open.images` with `build_image_cache_reusing`
  - `LoadedWorld.instance_backgrounds: HashMap<String, Vec<Background>>` and a matching `OpenWorld.instance_backgrounds` field
  - test-support: `WorldPanel::test_backgrounds(&self) -> Vec<Background>`

- [ ] **Step 1: Failing unit tests** in `ggo_world_panel.rs`'s test module (use the crate's existing `ready_panel_in_window` fixture pattern; write the tileset fixture with `io::save_tileset` like `ggo_map_panel::loader`'s tests):

```rust
#[test]
fn background_map_rel_strips_worlds_prefix_and_keeps_nesting() {
    assert_eq!(background_map_rel("worlds/main", 0), "maps/main.bg0.map");
    assert_eq!(background_map_rel("worlds/nested/arena", 3), "maps/nested/arena.bg3.map");
    assert_eq!(background_map_rel("main", 1), "maps/main.bg1.map");
}

#[gpui::test]
async fn test_add_background_writes_bound_map_links_slot_and_undo_unlinks_only(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, cx) = crate::tests::ready_panel_in_window(cx, dir.path()).await;
    write_test_tileset(dir.path(), "tiles/bg.til"); // save_tileset fixture helper

    panel.update(cx, |panel, cx| {
        panel.add_background_impl(1, "tiles/bg.til".into(), cx);
        assert_eq!(
            panel.test_backgrounds(),
            vec![Background { layer: 1, map: "maps/test.bg1.map".into() }]
        );
        assert!(panel.test_is_dirty());
    });
    // The map exists on disk, blank, bound to the picked tileset.
    let data = io::open_map(dir.path(), "maps/test.bg1.map").unwrap();
    assert_eq!(data.til_path, "tiles/bg.til");
    assert_eq!(data.pal_path, "tiles/bg.pal");

    panel.update(cx, |panel, cx| {
        panel.undo_impl(cx);
        assert!(panel.test_backgrounds().is_empty(), "undo unlinks the slot");
    });
    assert!(
        io::open_map(dir.path(), "maps/test.bg1.map").is_ok(),
        "undo never deletes the file (accepted orphan)"
    );
    panel.update(cx, |panel, cx| {
        panel.redo_impl(cx);
        assert_eq!(panel.test_backgrounds().len(), 1, "redo relinks the EXISTING file");
    });
}

#[gpui::test]
async fn test_add_background_links_an_existing_map_without_overwriting(
    cx: &mut TestAppContext,
) {
    // Pre-create maps/test.bg0.map with one painted cell; add_background_impl
    // for layer 0 must link it and leave the painted cell intact on disk.
}

#[gpui::test]
async fn test_clear_background_unlinks_and_save_persists_the_toml(cx: &mut TestAppContext) {
    // add layer 2, save_impl, read_world: [[background]] present.
    // clear_background_impl(2), save_impl, read_world: backgrounds empty;
    // the .map file still exists on disk.
}
```

- [ ] **Step 2: Run, verify failure:** `cargo test -p ggo_world_panel --lib background`

- [ ] **Step 3: Implement.**

Naming helper (free fn near `world_stem` :285):
```rust
/// Where the add-layer flow puts a world's generated background map. The
/// leading `worlds/` is dropped but nesting is kept, so two worlds with
/// the same basename in different subdirectories cannot collide.
fn background_map_rel(world_stem: &str, layer: u8) -> String {
    let stem = world_stem.strip_prefix("worlds/").unwrap_or(world_stem);
    format!("maps/{stem}.bg{layer}.map")
}
```

`add_background_impl` — write file only when absent (redo-after-undo and deliberate re-adds link, never clobber), then link, then refresh:
```rust
fn add_background_impl(&mut self, layer: u8, til_rel: String, cx: &mut Context<Self>) {
    const NEW_BG_DIM: u16 = 16; // matches ggo_map_panel::geom::NEW_MAP_DIM
    let ViewerState::Ready(open) = &mut self.state else { return };
    let map_rel = background_map_rel(&open.listing.stem, layer);
    if io::open_map(&open.root, &map_rel).is_err() {
        if let Err(e) = io::save_new_bound_map(&open.root, &map_rel, NEW_BG_DIM, NEW_BG_DIM, &til_rel) {
            open.save_error = Some(e.to_string());
            cx.notify();
            return;
        }
    }
    self.apply_op(WorldOp::SetBackground { layer, map: Some(map_rel) }, cx);
    self.refresh_backgrounds(cx);
}

fn clear_background_impl(&mut self, layer: u8, cx: &mut Context<Self>) {
    self.apply_op(WorldOp::SetBackground { layer, map: None }, cx);
    self.refresh_backgrounds(cx);
}
```

`refresh_backgrounds`: recompute `open.merged = merge_backgrounds(&state.backgrounds, &instances, &open.instance_backgrounds)` (the same call `loader::load_world` makes at :119 — lift the `instances` tuple-building into a small helper or repeat the three lines); for each merged stem missing from `open.map_loads`, compose from disk (`io::compose_map_rgba` — the file exists, add-layer just wrote it) and insert; rebuild `open.images` via `build_image_cache_reusing(&[&open.sprite_loads, &open.map_loads, &open.meta_sprite_loads], &open.images)` (match the argument list `load_rel_path` currently uses to build the cache — copy its exact call). Loader change: `load_world` keeps `loaded_bgs` (:107) in the returned `LoadedWorld` as `instance_backgrounds`, and `OpenWorld::new` stores it.

Undo/redo integration: at the end of `undo_impl` (:1721) and `redo_impl` (:1731), detect a background change cheaply — snapshot `state().backgrounds` before the step, compare after, call `refresh_backgrounds` on difference.

Rail UI: in the toolbar/side region of `render_canvas`'s surrounding chrome (follow the panel's existing toolbar composition around :3298), render four rows `bg0..bg3`: linked → stem label + a clear button (`IconButton::new(("ggo-world-bg-clear", layer as usize), IconName::Trash)` calling `clear_background_impl`); empty → a `PopoverMenu` listing `io::list_tilesets(&open.root)` (the map panel's bind-picker pattern) whose pick calls `add_background_impl`. Keep ids stable (`ggo-world-bg-slot-{n}`) — smoke will click them.

test-support accessor:
```rust
#[cfg(feature = "test-support")]
pub fn test_backgrounds(&self) -> Vec<Background> {
    match &self.state {
        ViewerState::Ready(open) => open.store.state().backgrounds,
        _ => Vec::new(),
    }
}
```

- [ ] **Step 4: Run tests, verify pass:** `cargo test -p ggo_world_panel --lib`
- [ ] **Step 5: Commit:**
```bash
cd /home/clay/projects/zed && ./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib && git add -A crates/ggo/world_panel && git commit -m "ggo world_panel: layers rail edits [[background]] slots

Add-layer picks a tileset, writes maps/<stem>.bg<N>.map already bound
(save_new_bound_map), and links the slot via the new undoable
SetBackground; clear unlinks without deleting. Undo/redo refresh the
merged set and the canvas. First background editing UI -- until now the
only way to edit [[background]] was the raw-TOML split pane."
```

---

### Task 4: Paint mode — enter/exit, brush/eraser, live canvas

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs`
- Modify: `crates/ggo/world_panel/src/canvas.rs` (screen→cell helper + dimming)

**Interfaces:**
- Consumes: `PaintSession` (Task 2), `refresh_backgrounds`-style image swapping (Task 3), the canvas's existing screen→world transform (the helpers `dragged_pos`/marquee hit-testing already use — reuse the same math, do not derive a second transform).
- Produces:
```rust
enum PaintTarget {
    BgSlot(u8),
    /// Entity index (same frame as Selection::Entity).
    TilemapEntity(usize),
}
enum EditMode { Entities, Paint(PaintTarget) }
// OpenWorld gains: mode: EditMode, sessions: HashMap<String, PaintSession>,
//                  session_loading: Option<Task<()>>
impl WorldPanel {
    fn enter_paint_mode(&mut self, target: PaintTarget, cx: &mut Context<Self>);
    fn exit_paint_mode(&mut self, cx: &mut Context<Self>);
    /// (map rel, world-space pixel anchor) for a target, or None when the
    /// target no longer resolves (slot cleared, entity gone).
    fn paint_target_rel(&self, target: &PaintTarget) -> Option<(String, [f64; 2])>;
    /// Recompose the session's live image into map_loads + images.
    fn refresh_paint_image(&mut self, rel: &str, cx: &mut Context<Self>);
}
// canvas.rs:
pub fn paint_cell_at(world: [f64; 2], anchor: [f64; 2]) -> (i32, i32);
// test-support:
pub fn test_enter_paint_bg(&mut self, layer: u8, cx: &mut Context<Self>) -> bool;
pub fn test_paint_mode_rel(&self) -> Option<String>;
pub fn test_paint_session(&self, rel: &str) -> Option<&PaintSession>; // or a closure-based with_session
```

- [ ] **Step 1: Failing tests:**

```rust
// canvas.rs
#[test]
fn paint_cell_at_floors_into_the_anchored_tile_grid() {
    assert_eq!(paint_cell_at([0.0, 0.0], [0.0, 0.0]), (0, 0));
    assert_eq!(paint_cell_at([31.9, 16.0], [0.0, 0.0]), (1, 1));
    assert_eq!(paint_cell_at([-0.1, 0.0], [0.0, 0.0]), (-1, 0));
    assert_eq!(paint_cell_at([40.0, 40.0], [32.0, 32.0]), (0, 0));
}

// ggo_world_panel.rs
#[gpui::test]
async fn test_paint_mode_loads_a_session_and_brush_edits_the_map_doc(
    cx: &mut TestAppContext,
) {
    // Fixture: world + tileset + add_background_impl(0, ..) as in Task 3.
    // enter via test_enter_paint_bg(0); run_until_parked (session loads
    // off-thread); assert test_paint_mode_rel() == Some("maps/test.bg0.map").
    // Drive one brush application through the panel's mouse path (or a
    // test_paint_at hook mirroring the smoke world journeys' event style),
    // then assert the session's store has the painted cell and
    // panel dirty state reflects the session (Task 7 tightens this).
    // Escape exits: mode back to Entities; session SURVIVES in the map
    // (undo history preserved) -- re-enter and undo still works.
}

#[gpui::test]
async fn test_paint_mode_on_a_missing_map_is_an_error_state_not_a_crash(
    cx: &mut TestAppContext,
) {
    // SetBackground to a rel that doesn't exist on disk (apply_op directly),
    // test_enter_paint_bg -> session load fails -> mode stays Entities,
    // open.save_error (or a dedicated paint_error field) carries the reason.
}
```

- [ ] **Step 2: Run, verify failure.**
- [ ] **Step 3: Implement.**
  - `enter_paint_mode`: resolve `paint_target_rel`; if `sessions` lacks the rel, `cx.background_spawn` `PaintSession::load(&open.root, &rel, &project_root)` (project_root from the panel, same value `load_rel_path` uses), then on the foreground insert the session, set `open.mode = EditMode::Paint(target)`, `refresh_paint_image(&rel, cx)`, `cx.notify()`. Guard with the panel's load-generation idiom so a stale load can't install over a newer world. On load error: stay in `Entities`, surface the message.
  - `paint_target_rel`: `BgSlot(n)` → `background_map(n)` from store state, anchor `[0.0, 0.0]`. `TilemapEntity(i)` → entity `i`'s `Tilemap.stem + ".map"` and anchor `Transform.pos + [col, row] * TILE_PX` (mirror `render.rs::push_tilemap_item`'s arithmetic exactly — read it, copy the field access).
  - `paint_cell_at` in canvas.rs: `((world[0] - anchor[0]) / TILE_PX as f64).floor() as i32` per axis, `TILE_PX` imported from worldlib.
  - Mouse routing: in the canvas mouse-down/moved handlers, when `open.mode` is `Paint`, translate cursor → world (existing transform) → `paint_cell_at` → `session.paint_at(cell)`; on `true`, `refresh_paint_image`. Mouse-up: `session.commit_rect()` / `commit_selection()` (rect commit also refreshes). Entity hit-testing/marquee/drag handlers all no-op in paint mode.
  - `refresh_paint_image`: `session.live_rgba()` → `render::RgbaImage { rgba: rgba.into(), w, h }` → `open.map_loads.insert(stem.to_string(), Loadable::Ready(img))` (stem = rel minus `.map`, reuse `ggo_map_panel::loader::map_stem`) → rebuild `open.images` with `build_image_cache_reusing` — one image recomposes, every other cache entry is reused by buffer address (`image_key`). Same-stem-drawn-twice updates for free.
  - Escape: in the existing escape handler, paint mode branch first — selection pending/set on the session → clear it; else `exit_paint_mode`. Undo/redo: `undo_impl`/`redo_impl` branch on mode → `session.step_history(MapDocStore::undo|redo, project_root)` + `refresh_paint_image`; entity mode unchanged.
  - Dimming: thread `Option<&str>` (the active paint stem, `None` in entity mode) into `canvas::paint_scene`; non-matching draw items paint at reduced opacity (use gpui's image opacity/tint if `paint_scene` has one available; otherwise wash with a translucent theme-background quad over non-target items — pick whichever the existing paint code supports with the smallest diff, and leave a `ponytail:`-free honest comment only if the approach is non-obvious).
- [ ] **Step 4: Run tests:** `cargo test -p ggo_world_panel --lib`
- [ ] **Step 5: Commit** (same chained gate, message: `ggo world_panel: paint mode -- brush/eraser edits background maps in place`, body noting sessions cache per stem and undo is mode-scoped).

---

### Task 5: Full tool surface in paint mode

Strip picker, tool rail (all 7 tools), palSub/flip controls, resize, terrain painting, select/copy/paste/delete, bind-tileset prompt for unbound maps.

**Files:**
- Modify: `crates/ggo/map_panel/src/ggo_map_panel.rs` → extract the strip-picker and terrain-editor render fns into a new `crates/ggo/map_panel/src/paint_ui.rs` (pub render helpers taking `&mut PaintSession` + the bounds `Rc<RefCell<Option<Bounds<Pixels>>>>` + an event callback, so both hosts can mount them); the standalone panel switches to calling these.
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (mount rail + strip + controls when `mode` is `Paint`)

**Interfaces:**
- Consumes: `MapTool::{ALL, icon, label, id}`, `PaintSession` fields/methods, `geom::{PAL_SUB_MIN, PAL_SUB_MAX, MIN_MAP_DIM, MAX_MAP_DIM}`, `MapOp::{Resize, Brush, RectFill, SetCells}`, clipboard `Stamp`.
- Produces: `paint_ui::render_tool_rail(...)`, `paint_ui::render_strip(...)`, `paint_ui::render_paint_controls(...)` — exact signatures settled during extraction (they must serve the standalone panel unchanged in this task; that's the compile-time proof of reusability). World panel holds the clipboard `Option<Stamp>` at panel level (survives switching targets), mirroring `MapPanel::clipboard`.

- [ ] **Step 1: Failing tests** — session-level (no gpui): terrain paint resolves and applies as one `SetCells` (port the shape of the panel's existing terrain test if one exists; otherwise: 2-tile terrain fixture, `session.tool = Terrain`, `session.terrain = Some(0)`, `paint_at` produces the 47-mask resolve worldlib's `terrain::resolve` returns); copy/paste round-trip through a panel-level clipboard; `MapOp::Resize` via a `resize(w, h)` session helper clamps to `MIN_MAP_DIM..=MAX_MAP_DIM`. Panel-level (gpui): in paint mode the tool rail's Select button id resolves and a marquee drag sets `session.selection`; delete with a selection blanks exactly those cells as ONE undo step (mirror the standalone panel's existing `smoke_map_select_delete_and_escape_branches` assertions at unit level).
- [ ] **Step 2: Run, verify failure.**
- [ ] **Step 3: Implement:** extract `paint_ui.rs` (strip render + strip mouse math move mostly verbatim; the strip's `pal_dragging` flag moves into `PaintSession` if the shared widget needs it — it does, move it); world panel mounts rail/strip/controls in a side column of the canvas element when painting (slot ids `ggo-world-paint-tool-*` reusing `MapTool::id` with a `world-` prefix is unnecessary — reuse the ids as-is, they're unique per window since the standalone panel is retiring). Unbound map: paint pane shows the bind picker (same `io::list_tilesets` menu as Task 3) instead of the strip; pick → `session.bind_tileset(til, project_root)` → refresh image.
- [ ] **Step 4: Both crates green:** `cargo test -p ggo_map_panel --lib && cargo test -p ggo_world_panel --lib`
- [ ] **Step 5: Commit** (`ggo map_panel, world_panel: full paint tool surface in world paint mode`).

---

### Task 6: Tilemap-entity paint targets

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs`

**Interfaces:**
- Consumes: `PaintTarget::TilemapEntity` + `paint_target_rel` (Task 4 built both; this task wires entry and covers it).
- Produces: context-menu entry "Paint tiles" on an entity with a `Tilemap` component (the panel's existing entity context-menu builder); double-click on such an entity enters paint mode too. test-support: `test_enter_paint_entity(&mut self, index: usize, cx) -> bool`.

- [ ] **Step 1: Failing test:** fixture world whose entity has `Transform { pos: [32, 16] }` + `Tilemap { stem: "maps/deco", col: 1, row: 0 }` and a real bound `maps/deco.map` on disk; `test_enter_paint_entity(0)` loads the session for `maps/deco.map`; painting the world-space point over the entity's cell (0,0) — i.e. world `[32 + 1*16, 16 + 0*16]` — edits doc cell (0,0), proving the anchor offset `Transform.pos + (col,row)*TILE_PX` is applied. An entity without `Tilemap` refuses entry (returns false).
- [ ] **Step 2: Run, verify failure.**
- [ ] **Step 3: Implement** entry points; `paint_target_rel` already resolves the anchor — verify its arithmetic against `render.rs::push_tilemap_item` (:461) while writing the test, not from memory.
- [ ] **Step 4: Run tests.**
- [ ] **Step 5: Commit** (`ggo world_panel: paint Tilemap-entity maps in place`).

---

### Task 7: Dirty, save, reload, and the Emulate invariant

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`save_impl` :1767, `dirty_world_name` :1908, `reload_from_disk` path, `save_if_open_and_dirty` :1810)
- Modify: `crates/ggo/world_panel/src/world_canvas_item.rs` (nothing structural — its `is_dirty`/`save` already route through the panel fns this task extends; extend its tests)

**Interfaces:**
- Consumes: `PaintSession::{dirty, save}`, sessions map (Task 4).
- Produces: `dirty_world_name` returns the world's name when the world store OR any session is dirty (so the tab dot, close prompt, and `save_if_open_and_dirty` all see paint edits); `save_impl` writes the world TOML then every dirty session, aggregating the first error into `open.save_error`; `reload_from_disk` clears `open.sessions` (discard means discard).

- [ ] **Step 1: Failing tests:**

```rust
#[gpui::test]
async fn test_paint_dirt_reaches_the_tab_the_save_and_the_emulate_gate(
    cx: &mut TestAppContext,
) {
    // add background + enter paint + one brush stroke; world store CLEAN.
    // dirty_world_name() is Some -- paint dirt alone marks the document.
    // save_impl: session clean, io::open_map shows the painted cell,
    // dirty_world_name() None.
    // Paint again; save_if_open_and_dirty("worlds/test.toml") returns true
    // AND flushed the map to disk (byte check) -- the emd pack-ggo
    // reads-from-disk invariant now covers paint edits.
}

#[gpui::test]
async fn test_reload_discards_paint_sessions(cx: &mut TestAppContext) {
    // paint, then reload_from_disk: sessions empty, disk map unchanged,
    // dirty_world_name() None.
}

#[gpui::test]
async fn test_failed_map_write_keeps_the_document_dirty(cx: &mut TestAppContext) {
    // Repoint session.root at a file (the world_canvas_item blocker trick),
    // save_impl: open.save_error Some, dirty_world_name still Some.
}
```

- [ ] **Step 2: Run, verify failure.**
- [ ] **Step 3: Implement.** `dirty_world_name`: `store.dirty() || open.sessions.values().any(|s| s.dirty())`. `save_impl`: world write as today; then for each dirty session `session.save()`, first `Err` → `open.save_error` (world write success + map write failure must still read as a failed save — `save_for_close` already keys off `save_error`). `reload_from_disk`/the load path: fresh `OpenWorld` naturally drops sessions — verify, don't assume, since sessions live on `OpenWorld`.
- [ ] **Step 4: Run tests** (`ggo_world_panel` + `cargo test -p ggo_emu_panel --lib` — its RED DRILL tests exercise `save_if_open_and_dirty`).
- [ ] **Step 5: Commit** (`ggo world_panel: save/dirty/reload cover paint sessions`).

---

### Task 8: Delete the standalone map editor

**Files:**
- Delete: `crates/ggo/map_panel/src/map_item.rs`
- Modify: `crates/ggo/map_panel/src/ggo_map_panel.rs` (remove `MapPanel`, `OpenMap`, `ViewerState`, `intercept_map_open` :196, `open_map_item` :214, `contribute_map_menu` :247, `create_blank_map` :158, all standalone render/mouse/zoom code and the `actions!` block; `init` shrinks to nothing → delete it)
- Modify: `crates/zed/src/main.rs:783` (remove `ggo_map_panel::init(cx); // GGO`)
- Modify: `crates/ggo/smoke/src/ggo_smoke.rs` (delete the standalone map journeys :1546-1858 and `smoke_new_map_opens_and_round_trips` :739 — Task 9 replaces them; the file must compile at this commit)
- Check: `grep -rn "ggo_map_panel" crates/ Cargo.toml` — expected surviving consumers: `ggo_world_panel` (library), `smoke` (Task 9's new journeys), workspace `Cargo.toml`. Anything else found (emu panel? import panel drop-to-import?) is handled in this task: re-point to the library surface or delete the dead reference — do not leave a dangling consumer.

**Interfaces:**
- Consumes: nothing new. Produces: `ggo_map_panel` is a pure library (`paint_session`, `paint_ui`, `loader`, `geom`); clicking a `.map` in the project panel falls through to default handling (accepted).

- [ ] **Step 1: Delete** per the inventory. Move any remaining shared helper the world panel needs (e.g. `split_map_path`'s asset-root walk, if `PaintSession::load` callers still use it) into `paint_session.rs` before deleting its old home.
- [ ] **Step 2: Prove the tab is gone at the funnel:** add one world-panel (or smoke, Task 9) assertion that `workspace.intercept_path_open` on `assets/maps/x.map` returns unclaimed. Keep the library's own tests (paint_session/paint_ui/loader/geom) green; delete only tests of deleted code.
- [ ] **Step 3: Full-workspace proof:** `cargo check --workspace` (catches the main.rs edit and any missed consumer), then `cargo test -p ggo_map_panel --lib && cargo test -p ggo_world_panel --lib && cargo test -p ggo_smoke --lib`.
- [ ] **Step 4: Commit:**
```bash
./script/clippy -p ggo_map_panel -p ggo_world_panel && cargo check --workspace && cargo test -p ggo_map_panel --lib -p ggo_world_panel --lib && git add -A && git commit -m "ggo map_panel: retire the standalone .map editor

MapEditorItem, the .map interceptor, New Map…, and the panel's own
canvas are gone; the crate is now the paint library the world panel
hosts. .map files are no longer directly openable (spec 2026-08-29,
accepted limitation)."
```
(If `./script/clippy` rejects multiple `-p` flags, run it once per crate, chained with `&&`.)

---

### Task 9: World-hosted smoke journeys

**Files:**
- Modify: `crates/ggo/smoke/src/ggo_smoke.rs` (new section where :1546-1858 lived)

**Interfaces:**
- Consumes: the existing world-journey plumbing (fixture writers, `intercept_path_open` funnel, `WorldCanvasItem::test_panel`, real-keymap key dispatch, the mouse-event helpers the deleted map journeys used), Task 3/4 test-support hooks, `background_map_rel` naming.

- [ ] **Step 1: Port the fixture:** `write_map_fixture`'s tileset half survives (2-tile `assets/art/mapfx.til`); the world fixture gains `[[background]] layer = 0, map = "maps/edit.bg0.map"` with that map written bound via worldlib, mirroring the old fixture's asset-root-relative `til_path` pinning.
- [ ] **Step 2: `smoke_world_paint_undo_and_save_round_trip`** — open the world through the real interceptor funnel; enter paint mode on slot 0 (click the rail slot by resolved element id, with the same wrong-button guard trick the old select-tool test used); click a canvas cell; assert the doc cell equals `pack_cell(0,0,false,false)`, doc dirty, world store clean; `ctrl-z` one step reverts; `ctrl-shift-z` replays; `ctrl-s` cleans BOTH stores; reopen the `.map` with `io::open_map` — byte-exact cells, `til_path` still asset-root-relative.
- [ ] **Step 3: `smoke_world_paint_select_delete_and_escape_branches`** — paint two cells, Select tool via rail button, marquee, `escape` clears selection (still in paint mode), second `escape` exits to entity mode, re-enter + re-select + `delete` blanks exactly the selection as one undo step.
- [ ] **Step 4: `smoke_world_add_background_layer_journey`** — world with no backgrounds; click empty slot 1's add button; pick the fixture tileset from the menu; assert `maps/<stem>.bg1.map` exists on disk bound to it, `[[background]]` present in the store; paint; `ctrl-s`; `read_world` shows the link and the map bytes show the paint.
- [ ] **Step 5: Run:** `cargo test -p ggo_smoke --lib` (plus `-p ggo_world_panel --lib` for any test-support additions).
- [ ] **Step 6: Commit** (`ggo smoke: world-hosted paint journeys replace the map-tab journeys`).

---

## Post-plan notes for the executor

- After Task 9, run the full gate across every touched crate before declaring done: `./script/clippy -p ggo_map_panel -p ggo_world_panel -p ggo_smoke && cargo test -p ggo_map_panel --lib -p ggo_world_panel --lib -p ggo_smoke --lib && cargo check --workspace`, plus the ggo-repo gate from Task 1 if worldlib was touched again.
- Do not run `install-zedgg` at any point; clay installs and eyeballs the UI (dimming opacity, rail placement) — flag both as "needs visual check" in the final report.
- PR (if asked): title `ggo: World-hosted map editing`, body ends with a `Release Notes:` section (`- N/A` — fork-only), and a "Suggested .rules additions" heading only if a genuinely new trap emerged (the pane-lease rule already exists; don't restate it).
