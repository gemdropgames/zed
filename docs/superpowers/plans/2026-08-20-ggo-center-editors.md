# GGO Center-Pane Editors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `.spr` files open as one center-pane tab per file; a world open puts the canvas in a center tab (dock keeps the inspector) plus the `.toml` as an adjacent tab.

**Architecture:** New workspace `Item` wrappers around the existing panel entities. `SpriteEditorItem` owns a private `Entity<SpritePanel>` per file (dock registration removed). `WorldCanvasItem` holds `WeakEntity<WorldPanel>`; the dock panel stays the document owner and renders inspector-only, while the item renders the canvas.

**Tech Stack:** Rust, gpui, `workspace::{Item, ItemHandle}`, existing ggo panel crates.

**Spec:** `docs/superpowers/specs/2026-08-20-ggo-center-editors-design.md`

## Global Constraints

- Do NOT commit: repo rule is commit only when clay explicitly asks. Skip every commit step; leave the tree dirty.
- `./script/clippy -p <crate>` must pass after each task.
- No `mod.rs`; new files are `src/<name>.rs` with `mod <name>;` in the crate root file.
- Comments explain "why" only; follow the crates' existing doc-comment style.
- GPUI test idiom: `#[gpui::test] async fn(cx: &mut TestAppContext)`, `MultiWorkspace::test_new` for workspace tests (see `test_spr_click_routes_into_the_panel_and_is_claimed` in `ggo_sprite_panel.rs` for the exact setup shape).

---

### Task 1: SpriteEditorItem

**Files:**
- Create: `crates/ggo/sprite_panel/src/sprite_item.rs`
- Modify: `crates/ggo/sprite_panel/src/ggo_sprite_panel.rs` (add `mod sprite_item;`; make `SpritePanel::new`, `open_rel_path`, `save_impl`, `ViewerState`, and the `store` accessors reachable — `pub(crate)` where currently private)
- Test: in `sprite_item.rs` `#[cfg(test)] mod tests` (reuse fixture helpers via `pub(crate)` on `write_sprite_fixture`… if test helpers are not reachable, move `write_sprite_fixture`/`save_fixture` into a `pub(crate) mod test_fixtures` in `ggo_sprite_panel.rs` guarded by `#[cfg(test)]`)

**Interfaces:**
- Consumes: `SpritePanel::new(Option<WeakEntity<Workspace>>, cx)`, `SpritePanel::open_rel_path(&str, &mut Window, &mut Context<SpritePanel>)`, `SpritePanel::save_impl(cx)`, `ViewerState::Ready(open)` with `open.store.dirty()`, `open.save_error`.
- Produces: `SpriteEditorItem::new(rel: String, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<SpriteEditorItem>) -> Self`, `SpriteEditorItem::rel(&self) -> &str`, `pub enum SpriteItemEvent { UpdateTab }`.

- [ ] **Step 1: Write the failing test** (in `sprite_item.rs`)

```rust
#[gpui::test]
async fn test_item_wraps_a_panel_and_mirrors_dirty(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    crate::test_fixtures::write_sprite_fixture(dir.path()); // the Task 1 "Files" note's shared #[cfg(test)] fixture module
    let root = dir.path().to_path_buf();
    let (item, cx) = cx.add_window_view(|window, cx| {
        // No real workspace: root_override drives root resolution, as in
        // the panel's own tests.
        let mut item = SpriteEditorItem::new_for_test("sprites/hero.spr".into(), root, window, cx);
        item
    });
    cx.run_until_parked();
    item.read_with(cx, |item, cx| {
        assert_eq!(item.rel(), "sprites/hero.spr");
        assert_eq!(item.tab_content_text(0, cx), "hero");
        assert!(!item.is_dirty(cx), "freshly opened is clean");
    });
    item.update(cx, |item, cx| {
        item.panel().update(cx, |panel, cx| panel.step_size(1, 0, cx));
    });
    item.read_with(cx, |item, cx| assert!(item.is_dirty(cx), "panel edits surface as item dirt"));
}
```

`new_for_test` = `new` but setting `panel.root_override` instead of a workspace handle; `panel()` is a `#[cfg(test)] pub(crate)` accessor.

- [ ] **Step 2: Run `cargo test -p ggo_sprite_panel test_item_wraps` — expect compile failure (type missing)**

- [ ] **Step 3: Implement `sprite_item.rs`**

```rust
//! One center-pane tab per `.spr`: a workspace `Item` wrapping its own
//! `SpritePanel` entity, so every open sprite keeps independent
//! state/undo. Replaces the dock registration (spec 2026-08-20).

use gpui::{AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity, Window};
use ui::prelude::*;
use workspace::item::{Item, ItemEvent, TabContentParams};
use workspace::{Workspace, SaveOptions};
use project::Project;

use crate::{SpritePanel, ViewerState};

pub enum SpriteItemEvent { UpdateTab }

pub struct SpriteEditorItem {
    panel: Entity<SpritePanel>,
    rel: String,
}

impl SpriteEditorItem {
    pub fn new(rel: String, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let panel = cx.new(|cx| SpritePanel::new(Some(workspace), cx));
        panel.update(cx, |panel, cx| panel.open_rel_path(&rel, window, cx));
        // Re-render the tab (dirty dot) whenever the inner panel changes.
        cx.observe(&panel, |_, _, cx| cx.emit(SpriteItemEvent::UpdateTab)).detach();
        Self { panel, rel }
    }

    pub fn rel(&self) -> &str { &self.rel }
}

impl EventEmitter<SpriteItemEvent> for SpriteEditorItem {}

impl Focusable for SpriteEditorItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle { self.panel.focus_handle(cx) }
}

impl Render for SpriteEditorItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for SpriteEditorItem {
    type Event = SpriteItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event { SpriteItemEvent::UpdateTab => f(ItemEvent::UpdateTab) }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        // File stem: "sprites/hero.spr" -> "hero".
        std::path::Path::new(&self.rel)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.rel.clone())
            .into()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        matches!(&self.panel.read(cx).state, ViewerState::Ready(open) if open.store.dirty())
    }

    fn can_save(&self, cx: &App) -> bool { self.is_dirty(cx) }

    fn save(&mut self, _options: SaveOptions, _project: Entity<Project>, _window: &mut Window, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let result = self.panel.update(cx, |panel, cx| {
            panel.save_impl(cx);
            match &panel.state {
                ViewerState::Ready(open) => match &open.save_error {
                    Some(e) => Err(anyhow::anyhow!(e.clone())),
                    None => Ok(()),
                },
                _ => Ok(()),
            }
        });
        Task::ready(result)
    }
}
```

Adjust field/method visibility in `ggo_sprite_panel.rs` (`pub(crate) state`, `pub(crate) fn save_impl`, `pub(crate) enum ViewerState`, `pub(crate) struct OpenSprite` fields used) until this compiles. Exact trait-method names/signatures: mirror `crates/image_viewer/src/image_viewer.rs`'s `impl Item for ImageView` where they differ.

- [ ] **Step 4: Run `cargo test -p ggo_sprite_panel test_item_wraps` — expect PASS; then the full crate suite — all green**

- [ ] **Step 5: `./script/clippy -p ggo_sprite_panel` — clean**

---

### Task 2: Route `.spr` opens to items; retire the sprite dock

**Files:**
- Modify: `crates/ggo/sprite_panel/src/ggo_sprite_panel.rs` (`init`, `intercept_sprite_open`, tests)

**Interfaces:**
- Consumes: `SpriteEditorItem::{new, rel}` (Task 1), `Workspace::{items_of_type, activate_item, add_item_to_active_pane}` (`crates/workspace/src/workspace.rs:4096/5267/4732`).
- Produces: `.spr` interception behavior relied on by user flows; no new API.

- [ ] **Step 1: Write the failing tests** (replace `test_spr_click_routes_into_the_panel_and_is_claimed` and add a second)

```rust
#[gpui::test]
async fn test_spr_click_opens_one_item_per_file_and_refocuses(cx: &mut TestAppContext) {
    // Same MultiWorkspace::test_new + real-fs temp project setup as the
    // old routing test, with TWO fixtures: sprites/hero.spr, sprites/foe.spr.
    // 1. intercept_path_open(hero) -> claimed; exactly one
    //    SpriteEditorItem in the workspace, rel == "sprites/hero.spr".
    // 2. intercept_path_open(foe) -> two items.
    // 3. intercept_path_open(hero) again -> still two items; the active
    //    pane's active item is the hero item (activate, not duplicate).
    // Item count via workspace.items_of_type::<SpriteEditorItem>(cx).
}
```

- [ ] **Step 2: Run — expect FAIL (interceptor still routes to the dock panel)**

- [ ] **Step 3: Rewrite `intercept_sprite_open` and `init`**

```rust
fn intercept_sprite_open(workspace: &mut Workspace, path: &ProjectPath, window: &mut Window, cx: &mut Context<Workspace>) -> bool {
    if !is_sprite_path(path) { return false; }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else { return false; };
    if let Some(existing) = workspace
        .items_of_type::<SpriteEditorItem>(cx)
        .find(|item| item.read(cx).rel() == rel)
    {
        workspace.activate_item(&existing, true, true, window, cx);
        return true;
    }
    let weak = workspace.weak_handle();
    let item = cx.new(|cx| SpriteEditorItem::new(rel, weak, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
    true
}
```

In `init`: delete the `cx.observe_new(...)` block that creates/adds the dock panel and registers `ToggleFocus` (lines around `workspace.add_panel(panel, window, cx)`); delete the `ToggleFocus` action from the `actions!` list and its keybinding in `bind_panel_keys`; keep the interceptor + context-menu registrations. Remove the now-dead `impl Panel for SpritePanel` block and `test_toggle_focus_opens_panel`.

- [ ] **Step 4: Fix the fallout in the crate's tests** — any test that grabbed the panel via `workspace.panel::<SpritePanel>(cx)` builds a bare panel entity instead (the `ready_panel` helper already does; most tests never touch the dock). Delete tests that exist purely to prove dock registration.

- [ ] **Step 5: Run `cargo test -p ggo_sprite_panel` — all green; `./script/clippy -p ggo_sprite_panel` — clean**

---

### Task 3: WorldPanel: canvas callable from outside, inspector-full-width dock

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`render_canvas` → `pub(crate)`, dock render drops the canvas column)

**Interfaces:**
- Consumes: existing `WorldPanel::render_canvas(&self, cx: &mut Context<WorldPanel>) -> gpui::AnyElement` (line ~2349).
- Produces: `pub(crate) fn render_canvas` unchanged in behavior, callable by the item; the dock's Ready render shows transport/inspector only.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn test_dock_render_has_no_canvas_but_render_canvas_still_composes(cx: &mut TestAppContext) {
    // Load the fixture world (reuse the crate's existing ready-panel test
    // helper). Assert the panel's canvas-bounds cell (`open.view.last_bounds`
    // idiom) stays None after a dock render, and that calling
    // panel.render_canvas(cx) directly returns an element (smoke: no panic).
}
```

- [ ] **Step 2: Run — expect FAIL (dock render still paints the canvas and records bounds)**

- [ ] **Step 3: Make `render_canvas` `pub(crate)`; remove the canvas child from the dock's Ready layout so the inspector takes the full width. Keep all canvas code compiled (the item uses it next task).**

- [ ] **Step 4: Run the crate suite; rewire any dock tests that asserted canvas presence to call `render_canvas` directly. All green; clippy clean.**

---

### Task 4: WorldCanvasItem

**Files:**
- Create: `crates/ggo/world_panel/src/world_canvas_item.rs`
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (add `mod world_canvas_item;`, visibility for `state`/`save_impl`/`dirty` accessors)

**Interfaces:**
- Consumes: `WeakEntity<WorldPanel>`, `WorldPanel::render_canvas` (Task 3), panel dirty/save (`save_impl`, `dirty_world_name`), `open_rel_path_now()` for the tab title.
- Produces: `WorldCanvasItem::new(panel: WeakEntity<WorldPanel>, cx) -> Self`, `pub enum WorldCanvasEvent { UpdateTab }`.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn test_canvas_item_renders_panel_canvas_and_mirrors_dirty(cx: &mut TestAppContext) {
    // Build a Ready WorldPanel entity (existing helper), wrap:
    // let item = cx.new(|cx| WorldCanvasItem::new(panel.downgrade(), cx));
    // - tab_content_text contains the world stem;
    // - is_dirty false; dirty_the_world(&panel, cx); is_dirty true;
    // - render in a window (add_window_view) parks; the panel's
    //   view.last_bounds is now Some (the canvas painted from the item).
}
```

- [ ] **Step 2: Run — expect compile failure (type missing)**

- [ ] **Step 3: Implement** — same skeleton as `SpriteEditorItem` (Task 1 Step 3) with these differences: holds `WeakEntity<WorldPanel>`; `render` upgrades and calls `panel.update(cx, |p, cx| p.render_canvas(cx))`, falling back to a `Label::new("No world open")` element when the upgrade fails; `is_dirty` = `panel.read(cx)` dirty check via the same state the dock's dirty dot uses; `save` routes `save_impl`; `tab_content_text` = `"World: <stem>"` from `open_rel_path_now()`, `"World"` when none; `Focusable` delegates to the panel's focus handle (upgrade-or-own-handle: keep one `FocusHandle` field for the dead-panel case).

- [ ] **Step 4: Run the new test — PASS; crate suite green; clippy clean.**

---

### Task 5: World routing — dock load + canvas tab + toml tab

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`intercept_world_open`)

**Interfaces:**
- Consumes: `WorldCanvasItem` (Task 4), `Workspace::{items_of_type, activate_item, add_item_to_active_pane, open_path}` — `open_path(path: ProjectPath, …)` at `workspace.rs:4831`.
- Produces: final routing behavior; no new API.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn test_world_click_opens_canvas_item_and_toml_tab(cx: &mut TestAppContext) {
    // MultiWorkspace::test_new over a temp project with assets/worlds/w1.toml
    // (reuse the crate's existing routing-test fixture).
    // intercept_path_open(w1.toml) ->
    //  - claimed == true
    //  - dock WorldPanel is Ready on w1
    //  - exactly one WorldCanvasItem exists
    //  - the pane also holds an editor item for w1.toml (items_len == 2)
    //  - the pane's ACTIVE item is the canvas item
    // A second intercept re-activates, item count still 2.
}
```

- [ ] **Step 2: Run — expect FAIL (current interceptor only loads the dock panel)**

- [ ] **Step 3: Extend `intercept_world_open`** — keep the existing dock-load body, then: find-or-create the single `WorldCanvasItem` (find via `items_of_type`, create with the dock panel's downgraded handle from `workspace.panel::<WorldPanel>(cx)`); `workspace.open_path(path.clone(), None, false, window, cx)` for the toml (returns a `Task` — `detach_and_log_err`, do NOT focus it); finally `activate_item(&canvas_item, true, true, …)`. Order matters: open the toml before the final activate so the canvas tab ends active.

- [ ] **Step 4: Run — PASS; crate suite green; clippy clean.**

---

### Task 6: Full verification sweep

**Files:** none new.

- [ ] **Step 1: `cargo test -p ggo_sprite_panel -p ggo_world_panel` — all green**
- [ ] **Step 2: `./script/clippy -p ggo_sprite_panel -p ggo_world_panel` — clean**
- [ ] **Step 3: `cargo build -p zed` compiles (panel deregistration can strand imports in `zed`'s panel setup — fix any `SpritePanel::ToggleFocus` references in `crates/zed`)**
- [ ] **Step 4: Report: what moved, what tests cover it, known gaps (no session restore for items; world toml/panel save race unchanged — both per spec)**
