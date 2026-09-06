# Live World View v2 — Phase 3 (per-tab worlds, thin dock) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Several worlds open at once, one per center tab, each owning its document and its viewer emulator; the right-dock panel shows whichever tab is active.

**Architecture:** `WorldPanel` (the existing 15k-line entity) stops being the dock `Panel` and becomes the per-document entity owned strongly by its `WorldCanvasItem`. A new thin `WorldDock` entity is the registered `Panel`: it tracks the active `WorldCanvasItem`, renders that tab's `WorldPanel`, and is the single entry point for opening worlds. Every external caller goes through `WorldDock`.

**Tech Stack:** Rust, GPUI, `workspace::{Panel, Item, Event::ActiveItemChanged}`.

**Spec:** `docs/superpowers/specs/2026-09-06-live-world-view-v2-design.md` ("`ggo_world_panel`: per-tab documents, thin dock").

## Global Constraints

- Branch `live-world-view-v2` in `/home/clay/projects/zed`. Commit per task, no AI trailers.
- Gate before every commit: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`. Task 4 also gates `ggo_emerald_panel`, `ggo_emu_panel`, `ggo_smoke`.
- The dock keeps `persistent_name() == "GGO World"` and `panel_key() == GGO_WORLD_PANEL_KEY` so saved layouts keep working.
- Fork hook rule (CLAUDE.md): `intercept_world_open`, `open_in_panel`'s closure and `WorldDock::open_world` run while the `Workspace` is leased. Reading the workspace or a pane there panics. Loading a world (which reads the workspace for its root) is deferred, as `open_rel_path` already does.
- `WorldPanel::new(workspace, cx)` keeps its signature; every existing `WorldPanel` test still constructs one directly.
- Closing a tab must drop its `WorldPanel`; the panel's existing `on_release` already calls `endpoint.request_stop()`.
- No `unwrap()` outside tests; comments explain why only.

---

### Task 1: `WorldDock` entity

**Files:**
- Create: `crates/ggo/world_panel/src/world_dock.rs`
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`mod world_dock; pub use world_dock::WorldDock;` at the top; `init` ~line 157; `impl Panel for WorldPanel` ~line 6766 and `impl EventEmitter<PanelEvent> for WorldPanel` deleted; `position` field and `DockPosition` import removed from `WorldPanel`; `canvas_mode` and `live_sys_mask` become `pub(crate)`; new `pub(crate) fn mark_loading(&mut self, rel: &str, cx)`)
- Modify: `crates/ggo/world_panel/src/world_canvas_item.rs` (`panel: Entity<WorldPanel>` strong; `pub fn panel(&self) -> &Entity<WorldPanel>`; `test_panel` returns `Some(self.panel.clone())`; `new(panel: Entity<WorldPanel>, cx)`)

**Interfaces:**
- Produces:

```rust
pub struct WorldDock { .. }
impl WorldDock {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self;
    /// The panel behind the active world tab, if the active item is one.
    pub fn active(&self) -> Option<Entity<WorldPanel>>;
    /// Every open world panel (one per `WorldCanvasItem` in any pane).
    pub fn open_panels(&self, cx: &App) -> Vec<Entity<WorldPanel>>;
    /// Activate the tab already showing `rel`, or open a new one. Returns
    /// the tab's panel. Safe to call while the workspace is leased.
    pub fn open_world(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<WorldPanel>>;
}
impl Panel for WorldDock { .. }   // moved from WorldPanel, unchanged values
```

- [ ] **Step 1: Write the failing tests**

In `world_dock.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    /// A workspace with the dock registered and a real fixture root the
    /// panels read through `root_override`.
    async fn dock_workspace(
        cx: &mut TestAppContext,
        root: &std::path::Path,
    ) -> (Entity<Workspace>, Entity<WorldDock>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        crate::tests::write_fixture(root);
        // A second world beside the fixture's `worlds/test.toml`.
        std::fs::copy(root.join("worlds/test.toml"), root.join("worlds/other.toml")).unwrap();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/proj", serde_json::json!({ "worlds": { "test.toml": "", "other.toml": "" } })).await;
        let project = Project::test(fs, ["/proj".as_ref()], cx).await;
        let (multi, cx) = cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |mw, _| mw.workspace().clone());
        let dock = workspace.read_with(cx, |ws, cx| ws.panel::<WorldDock>(cx).expect("registered"));
        dock.update(cx, |dock, _| dock.test_root_override = Some(root.to_path_buf()));
        (workspace, dock, cx)
    }

    #[gpui::test]
    async fn two_worlds_open_as_two_tabs_with_two_panels(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        let first = workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/other.toml", window, cx);
            })
        });
        cx.run_until_parked();

        let panels = dock.read_with(cx, |dock, cx| dock.open_panels(cx));
        assert_eq!(panels.len(), 2, "one panel per tab");
        let rels: Vec<_> = panels.iter().map(|p| p.read_with(cx, |p, _| p.open_rel_path_now().map(str::to_string))).collect();
        assert_eq!(rels, [Some("worlds/test.toml".into()), Some("worlds/other.toml".into())]);
        assert!(panels.iter().all(|p| p.read_with(cx, |p, _| p.test_is_ready())));
        assert!(first, "the first open claimed the dock");

        // The dock follows the active tab: the second world is active now.
        let active = dock.read_with(cx, |dock, _| dock.active().expect("an active world"));
        assert_eq!(active.read_with(cx, |p, _| p.open_rel_path_now().map(str::to_string)), Some("worlds/other.toml".into()));

        // Re-opening the first world activates its tab instead of a third.
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(dock.read_with(cx, |dock, cx| dock.open_panels(cx).len()), 2);
        let active = dock.read_with(cx, |dock, _| dock.active().expect("an active world"));
        assert_eq!(active.read_with(cx, |p, _| p.open_rel_path_now().map(str::to_string)), Some("worlds/test.toml".into()));
    }

    #[gpui::test]
    async fn closing_a_tab_drops_its_panel_and_stops_its_viewer(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        workspace.update_in(cx, |ws, window, cx| {
            ggo_common::open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("open"));
        let endpoint = ggo_common::LinkEndpoint::new();
        panel.update(cx, |panel, _| panel.test_set_live_endpoint(endpoint.clone()));
        let weak = panel.downgrade();
        drop(panel);

        let item = workspace.read_with(cx, |ws, cx| ws.items_of_type::<crate::WorldCanvasItem>(cx).next().expect("tab"));
        let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(item.entity_id(), workspace::SaveIntent::Skip, window, cx)
        })
        .await
        .expect("close");
        cx.run_until_parked();

        assert!(weak.upgrade().is_none(), "the tab owned the panel");
        assert!(endpoint.stop_requested(), "the panel's release stopped its viewer");
        assert!(dock.read_with(cx, |dock, _| dock.active().is_none()));
    }

    #[gpui::test]
    async fn the_dock_renders_the_empty_message_without_a_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, dock, cx) = dock_workspace(cx, dir.path()).await;
        dock.update_in(cx, |dock, window, cx| {
            let _ = dock.render(window, cx);
            assert!(dock.active().is_none());
        });
    }
}
```

`crate::tests::write_fixture` exists in `ggo_world_panel.rs` tests (used by `ready_panel`); make it `pub(crate)`. `test_set_live_endpoint` and `test_root_override` on the dock are new `#[cfg(test)]` helpers (below). If `close_item_by_id` has a different signature in this tree, use whatever `Pane` offers to close one item skipping save.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ggo_world_panel --lib world_dock`
Expected: compile error, module missing.

- [ ] **Step 3: Implement `world_dock.rs`**

```rust
//! The right-dock panel for worlds. Thin on purpose: the document, the
//! editing state and the viewer emulator all live on the `WorldPanel`
//! behind each center-pane `WorldCanvasItem`; this dock only shows the
//! active tab's and is the one place a world gets opened from.

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity, Window,
};
use ui::prelude::*;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::Workspace;

use crate::world_canvas_item::WorldCanvasItem;
use crate::{EMPTY_MESSAGE, GGO_WORLD_PANEL_KEY, ToggleFocus, WorldPanel};

const DEFAULT_WIDTH: Pixels = px(360.);   // move the existing constant here

pub struct WorldDock {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: Option<WeakEntity<WorldPanel>>,
    _active_item: Option<Subscription>,
    #[cfg(test)]
    pub(crate) test_root_override: Option<std::path::PathBuf>,
}

impl WorldDock {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let _active_item = workspace.upgrade().map(|workspace| {
            cx.subscribe(&workspace, |this, _workspace, event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    // Deferred: the event is emitted from inside the
                    // workspace's own update, which `refresh_active` reads.
                    let this = cx.weak_entity();
                    cx.defer(move |cx| {
                        this.update(cx, |this, cx| this.refresh_active(cx)).ok();
                    });
                }
            })
        });
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            active: None,
            _active_item,
            #[cfg(test)]
            test_root_override: None,
        }
    }

    pub fn active(&self) -> Option<Entity<WorldPanel>> {
        self.active.as_ref().and_then(WeakEntity::upgrade)
    }

    pub fn open_panels(&self, cx: &App) -> Vec<Entity<WorldPanel>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        workspace
            .read(cx)
            .items_of_type::<WorldCanvasItem>(cx)
            .map(|item| item.read(cx).panel().clone())
            .collect()
    }

    fn refresh_active(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let active = workspace
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.downcast::<WorldCanvasItem>())
            .map(|item| item.read(cx).panel().downgrade());
        // A non-world tab keeps the last world in the dock: the user is
        // likely editing that world's toml or sprite beside it.
        if active.is_some() || self.active().is_none() {
            self.active = active;
            cx.notify();
        }
    }

    pub fn open_world(
        &mut self,
        rel: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<WorldPanel>> {
        let workspace = self.workspace.upgrade()?;
        let existing = self
            .open_panels(cx)
            .into_iter()
            .find(|panel| panel.read(cx).open_rel_path_now() == Some(rel));
        // The mode and mask are sticky across worlds: a new tab opens the
        // way the last one was showing.
        let (canvas_mode, live_sys_mask) = self
            .active()
            .map(|panel| {
                let panel = panel.read(cx);
                (panel.canvas_mode, panel.live_sys_mask)
            })
            .unwrap_or((crate::live::CanvasMode::Live, 0));
        let panel = match existing {
            Some(panel) => panel,
            None => {
                let weak_workspace = self.workspace.clone();
                let panel = cx.new(|cx| {
                    let mut panel = WorldPanel::new(Some(weak_workspace), cx);
                    panel.canvas_mode = canvas_mode;
                    panel.live_sys_mask = live_sys_mask;
                    #[cfg(test)]
                    {
                        panel.root_override = self.test_root_override.clone();
                    }
                    panel.mark_loading(rel, cx);
                    panel
                });
                panel
            }
        };
        self.active = Some(panel.downgrade());
        let rel = rel.to_string();
        let item_panel = panel.clone();
        // The workspace is leased by whoever called us (the explorer
        // interceptor, a menu entry, the MCP host): tabs and the load
        // wait for the lease to end.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                let existing = workspace
                    .items_of_type::<WorldCanvasItem>(cx)
                    .find(|item| item.read(cx).panel() == &item_panel);
                match existing {
                    Some(item) => workspace.activate_item(&item, true, true, window, cx),
                    None => {
                        let item = cx.new(|cx| WorldCanvasItem::new(item_panel.clone(), cx));
                        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
                    }
                }
            });
            item_panel.update(cx, |panel, cx| {
                if panel.open_rel_path_now() != Some(rel.as_str()) {
                    panel.open_rel_path(&rel, window, cx);
                }
            });
        });
        cx.notify();
        Some(panel)
    }

    #[cfg(test)]
    pub(crate) fn test_root_override(&mut self, root: std::path::PathBuf) {
        self.test_root_override = Some(root);
    }
}

impl Render for WorldDock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.active() {
            Some(panel) => panel.into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new(EMPTY_MESSAGE).color(Color::Muted))
                .into_any_element(),
        };
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(body)
    }
}

impl Focusable for WorldDock {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active() {
            Some(panel) => panel.read(cx).focus_handle.clone(),
            None => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<PanelEvent> for WorldDock {}

impl Panel for WorldDock {
    // Every method and value moved verbatim from `impl Panel for WorldPanel`
    // (persistent_name "GGO World", panel_key GGO_WORLD_PANEL_KEY, position,
    // position_is_valid Left|Right, set_position, default_size DEFAULT_WIDTH,
    // icon IconName::Public, icon_tooltip "GGO World", toggle_action
    // ToggleFocus, activation_priority 8) EXCEPT:
    //
    // `prepare_to_close` -> `Task::ready(true)`: dirt now lives in tabs,
    //   whose own close prompt (`WorldCanvasItem::can_save`) covers it.
    // `set_active(true)` -> the active panel's `refresh_worlds`, deferred
    //   exactly as before, through `self.active()`.
}
```

`WorldPanel` changes:

- Delete `impl Panel for WorldPanel`, `impl EventEmitter<PanelEvent> for WorldPanel`, the `position` field and its initialiser, `DEFAULT_WIDTH` (moved), and `test_root_override` if it was only a `Panel`-side helper (keep it; the dock test above uses the panel's `root_override` field directly, which is `pub(crate)`).
- `canvas_mode`, `live_sys_mask`, `root_override`, `focus_handle` become `pub(crate)`.
- Add:

```rust
    /// Put a fresh panel into `Loading` for `rel` before its deferred
    /// load runs, so a reader that arrives first (the MCP host's
    /// `world_read`) waits instead of seeing an empty panel.
    pub(crate) fn mark_loading(&mut self, rel: &str, cx: &mut Context<Self>) {
        if let Some((_, listing)) = split_world_path(rel) {
            self.state = ViewerState::Loading { stem: listing.stem };
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn test_set_live_endpoint(&mut self, endpoint: Arc<ggo_common::LinkEndpoint>) {
        self.live_endpoint = Some(endpoint);
    }
```

(`live_endpoint` loses its `PathBuf` half in Task 2; write the helper against whichever shape the field has when you get here.)

- `init`: register `WorldDock` instead of `WorldPanel`:

```rust
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return; };
        let weak_workspace = workspace.weak_handle();
        let dock = cx.new(|cx| WorldDock::new(weak_workspace, cx));
        workspace.add_panel(dock, window, cx);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorldDock>(window, cx);
        });
    })
    .detach();
```

`WorldCanvasItem`: field `panel: Entity<WorldPanel>`; `new(panel: Entity<WorldPanel>, cx)` observes it; `pub fn panel(&self) -> &Entity<WorldPanel>`; drop the "dead panel" branches (`upgrade()` → direct use), keep `focus_handle` fallback field out (share the panel's). `test_panel` returns `Some(self.panel.clone())`. Its tests that build a `WorldPanel` then `WorldCanvasItem::new(panel.downgrade(), cx)` pass `panel.clone()`; the "dead panel" test (`doomed`) is deleted — a strong handle cannot be dead.

- [ ] **Step 4: Run the tests**

Run: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`
Expected: the new dock tests PASS; existing tests that referenced `panel::<WorldPanel>` or `DockPosition` on the panel are fixed in the next step. `test_toggle_focus_opens_panel` (~line 6870) becomes: dock registered, `ToggleFocus` opens the right dock and focuses the dock's focus handle (no world open) — assert on `WorldDock` instead.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/world_panel/src
git commit -m "ggo_world_panel: WorldDock hosts one WorldPanel per world tab"
```

---

### Task 2: The explorer and the panel's own paths go through the dock

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`intercept_world_open` ~line 343-480; `contribute_world_menu` + `delete_world` (uses `panel_entry_handler::<WorldPanel>` → `<WorldDock>` then the active panel, or each open panel whose `source_rel == rel`); `enter_live` ~line 4092-4160 (drop the per-project endpoint reuse); `live_endpoint: Option<Arc<LinkEndpoint>>`; `on_release`)

**Interfaces:**
- Consumes: `WorldDock::open_world`, `WorldDock::open_panels`, `WorldCanvasItem::panel()`.

- [ ] **Step 1: Write the failing test**

In `ggo_world_panel.rs` tests, next to the existing interceptor tests (grep `intercept_world_open` in the tests module; there is one that asserts the canvas tab opens on the first click and the toml splits out on the second), add:

```rust
    #[gpui::test]
    async fn the_interceptor_opens_each_world_in_its_own_tab(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = crate::world_dock::tests::dock_workspace(cx, dir.path()).await;
        for rel in ["worlds/test.toml", "worlds/other.toml"] {
            let claimed = workspace.update_in(cx, |ws, window, cx| {
                intercept_world_open(ws, &project_path(ws, rel, cx), window, cx)
            });
            assert!(claimed, "{rel} is a world");
            cx.run_until_parked();
        }
        assert_eq!(dock.read_with(cx, |dock, cx| dock.open_panels(cx).len()), 2);
        workspace.read_with(cx, |ws, cx| {
            assert_eq!(ws.items_of_type::<WorldCanvasItem>(cx).count(), 2, "two tabs");
        });
    }
```

`project_path(ws, rel, cx)` builds a `ProjectPath` for the first worktree; the existing interceptor test has an equivalent — reuse its helper by name. Make `world_dock::tests::dock_workspace` `pub(crate)`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ggo_world_panel --lib the_interceptor_opens_each_world`
Expected: FAIL (second click on a different world reuses the one panel; count is 1).

- [ ] **Step 3: Implement**

`intercept_world_open`: the "second click" detection reads the tab for `rel`:

```rust
    let canvas_for_rel = workspace
        .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
        .find(|item| item.read(cx).panel().read(cx).open_rel_path_now() == Some(rel.as_str()));
    let second_click_canvas = canvas_for_rel.filter(|canvas| {
        workspace
            .active_item(cx)
            .is_some_and(|item| item.item_id() == canvas.entity_id())
    });
```

(the toml-split branch below it is unchanged). The first-click branch becomes:

```rust
    ggo_common::open_in_panel(workspace, window, cx, move |dock: &mut WorldDock, window, cx| {
        dock.open_world(&rel, window, cx);
    })
```

and the block that created the `WorldCanvasItem` here is deleted (the dock does it).

`contribute_world_menu` / `delete_world`: `panel_entry_handler::<WorldDock>` with a closure that runs `delete_world` on every open panel whose `open_rel_path_now() == Some(rel)`, else on the active panel (the prompt only needs *a* panel; `delete_world` refreshes its own root). If no panel is open, build a throwaway `WorldPanel` for the prompt: `cx.new(|cx| WorldPanel::new(Some(workspace), cx))` and run `delete_world` on it.

`enter_live`: delete the `project_key` / reuse block. Every panel boots its own viewer:

```rust
        if let Some(endpoint) = self.live_endpoint.take() {
            endpoint.request_stop();
        }
        // ... existing workspace check ...
        window.defer(cx, move |window, cx| { /* existing boot_viewer call */ });
```

and on success `self.live_endpoint = Some(endpoint)`. Field type `Option<Arc<ggo_common::LinkEndpoint>>`; fix `stop_live_endpoint`, `on_release`, `fall_back_to_design_from` accordingly (they only ever used the endpoint half).

- [ ] **Step 4: Run tests**

Run: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`
Expected: PASS. The Live tests' `live_panel` helper (~line 13003) fetched `workspace.panel::<WorldPanel>()`; rewrite it to open through the dock — `open_in_panel(ws, window, cx, |dock: &mut WorldDock, window, cx| { dock.open_world("worlds/test.toml", window, cx); })`, `cx.run_until_parked()`, then `panel = dock.read(cx).active()` — with `dock.test_root_override(dir)` set before. Delete `a_world_switch_inside_one_project_reuses_the_cart` (~13663): each panel boots its own viewer now. Any other test that used `panel::<WorldPanel>` follows the same rewrite.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/world_panel/src
git commit -m "ggo_world_panel: explorer and menus open worlds through the dock"
```

---

### Task 3: External callers

**Files:**
- Modify: `crates/ggo/emerald_panel/src/ggo_emerald_panel.rs` (~lines 68, 1860, 1892, 4501, 5200, 5274, 5320)
- Modify: `crates/ggo/emu_panel/src/menu.rs` (~line 452)
- Modify: `crates/ggo/emu_panel/src/agent_remote.rs` (~lines 318-360, 837-866)
- Modify: `crates/ggo/smoke/src/ggo_smoke.rs` (~lines 1356-1375, 1633-1650: they read the tab's `test_panel()`, which still works; only `panel::<WorldPanel>` lookups change, grep)

**Interfaces:**
- Consumes: `WorldDock::{active, open_panels, open_world}`; `WorldPanel::{refresh_schemas, save_if_open_and_dirty, remote_list, remote_open, remote_read, remote_screenshot, open_rel_path_now}`.

- [ ] **Step 1: emerald panel**

- `use ggo_world_panel::{WorldDock, WorldPanel};`
- line ~1860 (`GenKind::Component` → `refresh_schemas`): run it on every panel: `for panel in dock.read(cx).open_panels(cx) { panel.update(cx, |p, cx| p.refresh_schemas(cx)); }` where `dock = workspace.read(cx).panel::<WorldDock>(cx)`.
- line ~1892 (`GenKind::World` → open the generated world): `open_in_panel(workspace, window, cx, move |dock: &mut WorldDock, window, cx| { dock.open_world(&rel, window, cx); })`.
- Tests at ~4501, 5200, 5274, 5320: fetch `panel::<WorldDock>(cx)` and then `.read(cx).active().expect("a world tab is open")` before the `open_rel_path_now` assertions; add `cx.run_until_parked()` after the open if not already there (the tab and load are deferred now).

Run: `./script/clippy -p ggo_emerald_panel && cargo test -p ggo_emerald_panel --lib` → PASS.

- [ ] **Step 2: emu panel menu**

`menu.rs` ~452: "save the dirty copy of this world before emulating" over every open panel:

```rust
            Some(workspace) => workspace
                .read(cx)
                .panel::<ggo_world_panel::WorldDock>(cx)
                .is_none_or(|dock| {
                    dock.read(cx)
                        .open_panels(cx)
                        .into_iter()
                        .all(|panel| panel.update(cx, |panel, cx| panel.save_if_open_and_dirty(&rel, cx)))
                }),
```

(`dock.read(cx)` borrow ends before the `update`s: collect `open_panels` into a `Vec` first.)

- [ ] **Step 3: agent remote (MCP `world_*`)**

Replace `world_panel_for` with two helpers:

```rust
/// The dock, leased through its window.
fn world_dock_for(workspace: &Entity<Workspace>, window: AnyWindowHandle, cx: &mut AsyncApp)
    -> Result<Entity<ggo_world_panel::WorldDock>, String>;   // same body, WorldDock

/// The panel showing `world` (stem or rel), opening a tab for it when
/// none does. A different world's unsaved edits are no longer a reason
/// to refuse: every world has its own tab.
fn world_panel_open(
    workspace: &Entity<Workspace>,
    window: AnyWindowHandle,
    world: &str,
    cx: &mut AsyncApp,
) -> Result<(Entity<ggo_world_panel::WorldPanel>, String), String> {
    let dock = world_dock_for(workspace, window, cx)?;
    window
        .update(cx, |_, window, app| {
            // Resolve the name against the project: any panel can (it
            // only needs the root), so use the active one or a probe.
            let resolver = dock.read(app).active().unwrap_or_else(|| {
                let weak = workspace.downgrade();
                app.new(|cx| ggo_world_panel::WorldPanel::new(Some(weak), cx))
            });
            let rel = resolver.update(app, |p, cx| p.remote_resolve(world, cx))?;
            let panel = workspace.update(app, |ws, cx| {
                ggo_common::open_in_panel(ws, window, cx, |dock: &mut ggo_world_panel::WorldDock, window, cx| {
                    dock.open_world(&rel, window, cx);
                });
                dock.read(cx).active()
            });
            panel.map(|panel| (panel, rel)).ok_or_else(|| "no world tab could be opened".to_string())
        })
        .map_err(|e| e.to_string())?
}
```

`remote_resolve` becomes `pub` on `WorldPanel`. `open_in_panel` returns `bool` here; the panel comes from `dock.active()` right after (`open_world` sets it synchronously). Call sites: `world_list` → `world_dock_for` then resolver panel's `remote_list`; `world_open` → `world_panel_open` returning `rel`; `world_read` / `world_screenshot` → `world_panel_open` then `await_world_ready(&panel, ..)` as before. `remote_open`'s "unsaved edits" refusal is dead now (the tab path never reloads an open world); delete `remote_open` if nothing else calls it.

Run: `./script/clippy -p ggo_emu_panel && cargo test -p ggo_emu_panel --lib` → PASS (fix the `agent_remote` tests that asserted the "unsaved edits" refusal: they now expect a second tab).

- [ ] **Step 4: smoke**

Grep `panel::<ggo_world_panel::WorldPanel>` and `WorldPanel::` in `ggo_smoke.rs`; route through `WorldDock` + `active()`. The tab-based helpers (`open_fixture_world`, `open_world_tab`) keep working because `test_panel()` still returns the panel; but with several tabs, `items_of_type::<WorldCanvasItem>().next()` may be the wrong tab — take the one whose panel's `open_rel_path_now()` matches `rel`.

Run: `./script/clippy -p ggo_smoke && cargo test -p ggo_smoke --lib` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo
git commit -m "ggo: callers reach worlds through WorldDock"
```

---

### Task 4: Whole-fork gate and review

- [ ] **Step 1: Gate every GGO crate**

```bash
for c in ggo_common ggo_world_panel ggo_emu_panel ggo_emerald_panel ggo_smoke; do ./script/clippy -p $c && cargo test -p $c --lib || exit 1; done
```

Expected: PASS.

- [ ] **Step 2: Manual check**

Run zed (`cargo run` or the project's run skill), open two worlds from the explorer: two tabs; the dock shows the active one; Design|Live switch per tab; closing a tab in Live stops its viewer (no emulator tab appears at any point).

- [ ] **Step 3: Review**

Fresh opus reviewer over `git diff ggo...live-world-view-v2 -- crates/ggo/world_panel crates/ggo/emerald_panel crates/ggo/emu_panel/src/agent_remote.rs crates/ggo/emu_panel/src/menu.rs crates/ggo/smoke` for practices and for the spec's "per-tab documents, thin dock" section. Fix findings; commit. Merge happens after Phase 4.
