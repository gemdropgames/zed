//! GGO World panel: a dock panel that lists the project's `worlds/**.toml`
//! files, renders the selected world with real pixels (composed sprite/
//! map images via `ggo-worldlib`), and edits it: click select, drag
//! placement (live `WorldOp` moves coalesced per gesture), a schema-driven
//! inspector, undo/redo, and save.
//!
//! Split: `loader` owns everything that runs off the UI thread (world
//! read, instance resolution, asset composition, manifest schemas);
//! `canvas` owns camera math, drag math and painting; `inspector` owns the
//! pure field-target/commit logic; this module owns the panel entity, the
//! picker, the state machine, and all gpui wiring.
//!
//! Editing semantics are ported from ggo-ide's `pages/world/mod.rs`:
//! primary-down hit-tests the CURRENT draw list in world coords and both
//! selects and arms a drag; every drag move applies a live
//! `MoveEntity`/`MoveInstance` with the gesture id (the store coalesces
//! them into one undo entry); snap goes through `drag_ops::snap_to_tile`
//! on the result. One deliberate divergence: empty-space left-drag does
//! NOT pan (W6 chose middle-drag pan; left-click on empty is a pure
//! deselect).

mod canvas;
mod inspector;
mod loader;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Render, RenderImage, ScrollWheelEvent, Styled, Subscription, Task,
    WeakEntity, Window, actions, div, px,
};
use serde_json::Value;
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, Divider, DropdownMenu, ToggleState};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_worldlib::backgrounds::MergedBackground;
use ggo_worldlib::drag_ops::{self, View};
use ggo_worldlib::merge_candidates::merge_candidates;
use ggo_worldlib::render::{
    AssetLoads, DrawItem, Selection, active_camera_origin, build_draw_list, hit_test, world_label,
};
use ggo_worldlib::schemas::{ComponentSchema, defaults_for};
use ggo_worldlib::world_doc::{WorldDocStore, WorldOp};
use ggo_worldlib::world_file::write_world;
use ggo_worldlib::world_files::WorldListing;

actions!(
    ggo_world,
    [
        /// Toggles focus on the GGO world panel.
        ToggleFocus,
        /// Undoes the last edit to the open world.
        Undo,
        /// Redoes the last undone edit to the open world.
        Redo,
        /// Saves the open world to its `worlds/*.toml` file.
        Save,
        /// Commits the focused inspector field (bound to Enter inside the
        /// panel's field editors).
        CommitField,
        /// Deletes the selected entity or instance from the open world.
        DeleteSelected
    ]
);

const GGO_WORLD_PANEL_KEY: &str = "GGOWorldPanel";

/// The panel's key-dispatch context (`.key_context`), which the
/// [`bind_panel_keys`] bindings are scoped to.
const KEY_CONTEXT: &str = "GgoWorldPanel";

/// Fixed default width until the panel grows real settings persistence.
const DEFAULT_WIDTH: Pixels = px(360.);

/// Inspector column width inside the panel.
const INSPECTOR_WIDTH: Pixels = px(220.);

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // `zed::reload_keymaps` CLEARS all key bindings and rebuilds them from
    // keymap files on every keymap/settings change (including once at
    // startup) -- and this fork's rule is that keymap assets are upstream
    // files we don't edit. `KeymapEventChannel` is triggered at the end of
    // every such reload, so re-adding our panel-scoped bindings there
    // keeps them alive without touching any upstream keymap.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| WorldPanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorldPanel>(window, cx);
        });
    })
    .detach();
}

fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-s", Save, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-s", Save, Some(KEY_CONTEXT)),
        // Panel-focused only: an inspector field editor's own (deeper)
        // Editor-context bindings win while typing.
        KeyBinding::new("delete", DeleteSelected, Some(KEY_CONTEXT)),
        KeyBinding::new("backspace", DeleteSelected, Some(KEY_CONTEXT)),
        // Single-line editors don't bind Enter themselves (the default
        // keymap's `enter -> editor::Newline` is `mode == full` only), so
        // this fires while an inspector field editor is focused.
        KeyBinding::new(
            "enter",
            CommitField,
            Some(&format!("{KEY_CONTEXT} > Editor")),
        ),
    ]);
}

// ------------------------------------------------------------- view state

/// Pan/zoom + gesture state shared (via `Rc<RefCell>`) between the panel's
/// event listeners and the canvas element's prepaint/paint closures --
/// prepaint is where the canvas learns its bounds, which both the lazy
/// initial camera centering and cursor-anchored zoom need.
struct ViewShared {
    zoom: f64,
    /// `None` until the first prepaint centers the camera (canvas size is
    /// unknown before layout).
    pan: Option<[f64; 2]>,
    last_bounds: Option<Bounds<Pixels>>,
    drag: Option<Drag>,
}

/// An in-flight middle-mouse pan drag.
struct Drag {
    start_cursor: [f64; 2],
    start_pan: [f64; 2],
}

/// An in-flight left-mouse placement drag on the selected entity/instance
/// -- ggo-ide's `CanvasGesture::Drag` plus its `drag_start_pos`/
/// `drag_start_world` anchors.
#[derive(Clone)]
struct EditDrag {
    gesture_id: String,
    start_pos: [f64; 2],
    start_world: [f64; 2],
}

/// One inspector text input: which field it edits and the single-line
/// editor entity backing it. The subscription commits on blur.
struct InspectorEntry {
    target: inspector::FieldTarget,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

/// A loaded world plus its render-side caches and editor state.
struct OpenWorld {
    listing: WorldListing,
    /// The project root this world was LOADED from. Save writes under
    /// THIS root, not the panel's live `project_root` -- a refresh can
    /// repoint the panel at a different worktree while a world from the
    /// old root is still open, and saving the open doc under the new
    /// root would write a file that was never read (stale-root finding,
    /// task M7).
    root: PathBuf,
    store: WorldDocStore,
    sprite_loads: AssetLoads,
    map_loads: AssetLoads,
    meta_sprite_loads: AssetLoads,
    merged: Vec<MergedBackground>,
    schemas: Vec<ComponentSchema>,
    /// One gpui `RenderImage` (BGRA) per composed worldlib image, built
    /// once at load time -- see `canvas::build_image_cache`.
    images: Arc<HashMap<usize, Arc<RenderImage>>>,
    view: Rc<RefCell<ViewShared>>,
    selected: Option<Selection>,
    snap: bool,
    edit_drag: Option<EditDrag>,
    gesture_counter: u64,
    inspector: Vec<InspectorEntry>,
    save_error: Option<String>,
}

impl OpenWorld {
    fn new(
        listing: WorldListing,
        root: PathBuf,
        loaded: loader::LoadedWorld,
        images: HashMap<usize, Arc<RenderImage>>,
    ) -> Self {
        OpenWorld {
            listing,
            root,
            store: loaded.store,
            sprite_loads: loaded.sprite_loads,
            map_loads: loaded.map_loads,
            meta_sprite_loads: loaded.meta_sprite_loads,
            merged: loaded.merged,
            schemas: loaded.schemas,
            images: Arc::new(images),
            view: Rc::new(RefCell::new(ViewShared {
                zoom: 1.0,
                pan: None,
                last_bounds: None,
                drag: None,
            })),
            selected: None,
            snap: false,
            edit_drag: None,
            gesture_counter: 0,
            inspector: Vec::new(),
            save_error: None,
        }
    }
}

/// The current paint-ordered draw list -- built fresh per use (render,
/// hit test, tests), same per-frame-of-change cadence as ggo-ide.
fn draw_items(open: &OpenWorld) -> Vec<DrawItem> {
    build_draw_list(
        &open.store.state(),
        &open.merged,
        open.selected,
        &open.sprite_loads,
        &open.map_loads,
        &open.meta_sprite_loads,
    )
}

/// The world point at the canvas center -- ggo-ide's `view_center_world`,
/// where a freshly added entity/instance lands. Before the first layout
/// (no bounds/pan yet -- possible when adding right after load, and the
/// normal case in headless tests) it falls back to the panel's default
/// width as a square at identity pan, the same fixed-canvas spirit as
/// ggo-ide's `CANVAS_DEFAULT_W/H`.
fn view_center_world(open: &OpenWorld) -> [f64; 2] {
    let v = open.view.borrow();
    let (w, h) = v
        .last_bounds
        .map_or((f64::from(DEFAULT_WIDTH), f64::from(DEFAULT_WIDTH)), |b| {
            (f64::from(b.size.width), f64::from(b.size.height))
        });
    let pan = v.pan.unwrap_or([0.0, 0.0]);
    let view = View {
        zoom: v.zoom,
        pan_x: pan[0],
        pan_y: pan[1],
        dpr: None,
    };
    drag_ops::screen_to_world(w / 2.0, h / 2.0, &view)
}

enum ViewerState {
    /// Nothing selected yet.
    Empty,
    Loading {
        stem: String,
    },
    Ready(Box<OpenWorld>),
    Error(String),
}

pub struct WorldPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    worlds: Vec<WorldListing>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl WorldPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            worlds: Vec::new(),
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree) and re-enumerate its worlds. Runs on every panel
    /// activation -- the walk only touches `<root>/worlds`, so it's cheap.
    fn refresh_worlds(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        self.worlds = match &self.project_root {
            Some(root) => loader::list_worlds(root),
            None => Vec::new(),
        };
        cx.notify();
    }

    /// Kick off the off-thread load of `self.worlds[ix]`. A stale result
    /// (superseded by a later selection) is dropped by generation check.
    fn select_world(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(listing) = self.worlds.get(ix).cloned() else {
            return;
        };
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            stem: listing.stem.clone(),
        };
        cx.notify();

        let rel = listing.rel_path.clone();
        let loaded_root = root.clone();
        let load = cx.background_spawn(async move {
            loader::load_world(&root, &rel).map(|loaded| {
                // Build the BGRA `RenderImage`s off-thread too -- one per
                // stem, never per frame.
                let images = canvas::build_image_cache(&[
                    &loaded.sprite_loads,
                    &loaded.map_loads,
                    &loaded.meta_sprite_loads,
                ]);
                (loaded, images)
            })
        });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok((loaded, images)) => ViewerState::Ready(Box::new(OpenWorld::new(
                        listing,
                        loaded_root,
                        loaded,
                        images,
                    ))),
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    // ------------------------------------------------------------ editing

    /// Apply one op to the open world's store and repaint. Every editor
    /// mutation funnels through here (or the drag/undo/redo paths, which
    /// notify themselves), so the draw list -- rebuilt per render --
    /// always reflects the store.
    fn apply_op(&mut self, op: WorldOp, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.store.apply(op);
            cx.notify();
        }
    }

    /// Toolbar "add entity": a fresh entity with just a Transform
    /// skeleton -- schema defaults (`defaults_for`, per the M7 brief;
    /// builtins guarantee a Transform schema) with `pos` overridden to
    /// the current view center, ggo-ide's `WorldMsg::AddEntity` -- then
    /// select it.
    fn add_entity_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let center = view_center_world(open);
        let mut transform = open
            .schemas
            .iter()
            .find(|s| s.name == "Transform")
            .map(defaults_for)
            .unwrap_or_default();
        transform.insert("pos".to_string(), serde_json::json!([center[0], center[1]]));
        let mut components = serde_json::Map::new();
        components.insert("Transform".to_string(), Value::Object(transform));
        open.store.apply(WorldOp::AddEntity { components });
        open.selected = Some(Selection::Entity(open.store.state().entities.len() - 1));
        open.edit_drag = None;
        cx.notify();
    }

    /// Worlds safe to `AddInstance` into the open world: the picker's
    /// stems through worldlib's cycle-guarded `merge_candidates` (always
    /// excludes the open world itself, plus any stem the last load
    /// proved cycles somewhere in this doc's resolved instance graph).
    fn instance_candidates(&self) -> Vec<String> {
        let ViewerState::Ready(open) = &self.state else {
            return Vec::new();
        };
        let stems: Vec<String> = self.worlds.iter().map(|w| w.stem.clone()).collect();
        merge_candidates(&stems, &open.listing.stem, &open.store.state().instances)
    }

    /// Add-instance picker pick: re-check the cycle guard at apply time
    /// (a built menu can outlive an undo/redo that changed the instance
    /// graph), then `AddInstance` + a follow-up `MoveInstance` to the
    /// view center (worldlib's op itself lands at its fixed
    /// `DEFAULT_INSTANCE_POS`; the move is a second undo entry -- first
    /// undo returns it to the default spot, second removes it), and
    /// select it.
    fn add_instance_impl(&mut self, stem: String, cx: &mut Context<Self>) {
        if !self.instance_candidates().contains(&stem) {
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let center = view_center_world(open);
        open.store.apply(WorldOp::AddInstance { world: stem });
        let index = open.store.state().instances.len() - 1;
        open.store.apply(WorldOp::MoveInstance {
            index,
            pos: center,
            gesture: None,
        });
        open.selected = Some(Selection::Instance(index));
        open.edit_drag = None;
        cx.notify();
    }

    /// Delete the selected entity or instance (toolbar button and the
    /// `DeleteSelected` action). Bounds-guarded: a selection gone stale
    /// against an undo/redo restructure is a no-op, not a panic.
    fn delete_selected_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        match open.selected {
            Some(Selection::Entity(index)) if index < open.store.state().entities.len() => {
                open.store.apply(WorldOp::RemoveEntity { index });
            }
            Some(Selection::Instance(index)) if index < open.store.state().instances.len() => {
                open.store.apply(WorldOp::RemoveInstance { index });
            }
            _ => return,
        }
        open.selected = None;
        open.edit_drag = None;
        cx.notify();
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.store.undo()
        {
            cx.notify();
        }
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.store.redo()
        {
            cx.notify();
        }
    }

    /// `to_doc()` -> `write_world` -> `mark_saved`. Synchronous by choice:
    /// world files are small TOML, and the async save ggo-ide uses is an
    /// iced task-architecture artifact, not an op-flow semantic (writing
    /// then `mark_saved` in one step also avoids the marked-depth race a
    /// mid-flight edit would cause).
    fn save_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // `open.root`, NOT `self.project_root`: the doc must be written
        // back where it was read from (see the `OpenWorld::root` doc).
        match write_world(&open.root, &open.listing.rel_path, &open.store.to_doc()) {
            Ok(()) => {
                open.store.mark_saved();
                open.save_error = None;
            }
            Err(e) => open.save_error = Some(e.to_string()),
        }
        cx.notify();
    }

    /// The current camera transform, if the canvas has laid out.
    fn canvas_view(&self) -> Option<View> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let v = open.view.borrow();
        let pan = v.pan?;
        Some(View {
            zoom: v.zoom,
            pan_x: pan[0],
            pan_y: pan[1],
            dpr: None,
        })
    }

    /// Left-mouse down at canvas-relative `local` px: hit-test in world
    /// coords, update the selection, and arm a placement drag on a hit --
    /// ggo-ide's `PrimaryDown` arm (minus its empty-space pan; panning is
    /// middle-drag here).
    fn canvas_primary_down(&mut self, local: [f64; 2], cx: &mut Context<Self>) {
        let Some(view) = self.canvas_view() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let world = drag_ops::screen_to_world(local[0], local[1], &view);
        let items = draw_items(open);
        let hit = hit_test(&items, world[0], world[1]);
        open.selected = hit;
        let start_pos = match hit {
            Some(Selection::Entity(i)) => inspector::entity_pos(&open.store.state(), i),
            Some(Selection::Instance(i)) => {
                open.store.state().instances.get(i).map(|inst| inst.pos)
            }
            None => None,
        };
        open.edit_drag = match start_pos {
            Some(start_pos) => {
                open.gesture_counter += 1;
                Some(EditDrag {
                    gesture_id: format!("drag-{}", open.gesture_counter),
                    start_pos,
                    start_world: world,
                })
            }
            None => None,
        };
        cx.notify();
    }

    /// Continue the in-flight placement drag to canvas-relative `local`
    /// px: live `MoveEntity`/`MoveInstance` applies sharing the drag's
    /// gesture id (the store coalesces them into ONE undo entry) --
    /// ggo-ide's `Moved` arm, snap included.
    fn canvas_drag_to(&mut self, local: [f64; 2], cx: &mut Context<Self>) {
        let Some(view) = self.canvas_view() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(drag) = open.edit_drag.clone() else {
            return;
        };
        let world = drag_ops::screen_to_world(local[0], local[1], &view);
        let pos = canvas::dragged_pos(drag.start_pos, drag.start_world, world, open.snap);
        match open.selected {
            Some(Selection::Entity(entity)) => open.store.apply(WorldOp::MoveEntity {
                entity,
                pos,
                gesture: Some(drag.gesture_id),
            }),
            Some(Selection::Instance(index)) => open.store.apply(WorldOp::MoveInstance {
                index,
                pos,
                gesture: Some(drag.gesture_id),
            }),
            None => {}
        }
        cx.notify();
    }

    /// Middle-mouse pan handling for a move event. Returns true if the
    /// event belonged to an in-flight pan (handled or cancelled).
    fn handle_pan_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &self.state else {
            return false;
        };
        let mut v = open.view.borrow_mut();
        let Some(drag) = &v.drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Middle) {
            v.drag = None;
            return true;
        }
        let dx = f64::from(event.position.x) - drag.start_cursor[0];
        let dy = f64::from(event.position.y) - drag.start_cursor[1];
        v.pan = Some([drag.start_pan[0] + dx, drag.start_pan[1] + dy]);
        drop(v);
        cx.notify();
        true
    }

    /// Canvas-relative position for an in-flight placement drag's move
    /// event; cancels the drag when the left button is no longer held.
    fn edit_drag_local(&mut self, event: &MouseMoveEvent) -> Option<[f64; 2]> {
        let ViewerState::Ready(open) = &mut self.state else {
            return None;
        };
        open.edit_drag.as_ref()?;
        if event.pressed_button != Some(MouseButton::Left) {
            open.edit_drag = None;
            return None;
        }
        let v = open.view.borrow();
        let bounds = v.last_bounds?;
        Some([
            f64::from(event.position.x - bounds.origin.x),
            f64::from(event.position.y - bounds.origin.y),
        ])
    }

    // -------------------------------------------------- inspector editors

    /// Keep the inspector's editor entities in sync with the selection:
    /// rebuild when the set of editable fields changed (selection change,
    /// component add/remove, undo/redo restructure), otherwise refresh
    /// unfocused editors' text from the doc (a focused editor keeps the
    /// user's in-progress buffer, ggo-ide's `field_edit` rule).
    fn ensure_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        let specs = inspector::selection_field_specs(open.selected, &state, &open.schemas);
        let same_targets = open.inspector.len() == specs.len()
            && open
                .inspector
                .iter()
                .zip(&specs)
                .all(|(entry, spec)| entry.target == spec.target);

        if same_targets {
            for entry in &open.inspector {
                if entry.editor.focus_handle(cx).is_focused(window) {
                    continue;
                }
                let text = inspector::display_text(&entry.target, &state, &open.schemas);
                if entry.editor.read(cx).text(cx) != text {
                    entry
                        .editor
                        .update(cx, |editor, cx| editor.set_text(text, window, cx));
                }
            }
            return;
        }

        let mut entries = Vec::with_capacity(specs.len());
        for spec in &specs {
            let text = inspector::display_text(&spec.target, &state, &open.schemas);
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(text, window, cx);
                editor
            });
            let subscription = cx.subscribe(&editor, Self::handle_editor_event);
            entries.push(InspectorEntry {
                target: spec.target.clone(),
                editor,
                _subscription: subscription,
            });
        }
        open.inspector = entries;
    }

    /// Blur commits the field, matching the brief's enter/blur rule (and
    /// ggo-ide's cross-field commit-on-input). An unchanged or unparsable
    /// buffer is a no-op in the store, so committing every blur is safe.
    fn handle_editor_event(
        &mut self,
        editor: Entity<Editor>,
        event: &EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, EditorEvent::Blurred) {
            self.commit_editor(editor.entity_id(), cx);
        }
    }

    fn commit_editor(&mut self, editor_id: EntityId, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some((target, text)) = open
            .inspector
            .iter()
            .find(|e| e.editor.entity_id() == editor_id)
            .map(|e| (e.target.clone(), e.editor.read(cx).text(cx)))
        else {
            return;
        };
        let state = open.store.state();
        if let Some(op) = inspector::commit_field(&target, &text, &state, &open.schemas) {
            open.store.apply(op);
            cx.notify();
        }
    }

    fn on_commit_field(&mut self, _: &CommitField, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let focused = open
            .inspector
            .iter()
            .find(|e| e.editor.focus_handle(cx).is_focused(window))
            .map(|e| e.editor.entity_id());
        if let Some(id) = focused {
            self.commit_editor(id, cx);
        }
    }

    // ------------------------------------------------------------- render

    fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_stem = match &self.state {
            ViewerState::Ready(open) => Some(open.listing.stem.clone()),
            ViewerState::Loading { stem } => Some(stem.clone()),
            _ => None,
        };
        h_flex()
            .flex_wrap()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when(self.worlds.is_empty(), |this| {
                let message = if self.project_root.is_some() {
                    "No worlds found"
                } else {
                    "No project open"
                };
                this.child(Label::new(message).color(Color::Muted))
            })
            .children(self.worlds.iter().enumerate().map(|(ix, listing)| {
                let selected = selected_stem.as_deref() == Some(listing.stem.as_str());
                Button::new(("ggo-world", ix), SharedString::from(listing.stem.clone()))
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_world(ix, cx)))
            }))
    }

    fn render_message(&self, message: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }

    /// Add-entity / add-instance / delete / save / undo / redo / snap
    /// row, with the dirty dot on the world's title.
    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_toolbar is only called in the Ready state");
        };
        let dirty = open.store.state().dirty;
        let snap = open.snap;
        let has_selection = open.selected.is_some();
        let candidates = self.instance_candidates();
        let weak = cx.weak_entity();

        // Add-instance picker over the cycle-guarded candidate stems.
        let instance_menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for stem in candidates {
                let weak = weak.clone();
                let label = world_label(&stem).to_string();
                menu = menu.entry(SharedString::from(label), None, move |_window, cx| {
                    let stem = stem.clone();
                    weak.update(cx, |this, cx| this.add_instance_impl(stem, cx))
                        .ok();
                });
            }
            menu
        });

        let weak = cx.weak_entity();
        let title = format!(
            "{}{}",
            world_label(&open.listing.stem),
            if dirty { " ●" } else { "" }
        );
        h_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(title).size(LabelSize::Small).color(if dirty {
                Color::Modified
            } else {
                Color::Muted
            }))
            .child(div().flex_1())
            .child(
                IconButton::new("ggo-world-add-entity", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Add entity"))
                    .on_click(cx.listener(|this, _, _, cx| this.add_entity_impl(cx))),
            )
            .child(DropdownMenu::new(
                "ggo-world-add-instance",
                "+ Instance",
                instance_menu,
            ))
            .child(
                IconButton::new("ggo-world-delete", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Delete selected"))
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected_impl(cx))),
            )
            .child(
                Checkbox::new("ggo-world-snap", ToggleState::from(snap))
                    .label("Snap")
                    .on_click(move |toggle, _window, cx| {
                        let on = matches!(toggle, ToggleState::Selected);
                        weak.update(cx, |this, cx| {
                            if let ViewerState::Ready(open) = &mut this.state {
                                open.snap = on;
                                cx.notify();
                            }
                        })
                        .ok();
                    }),
            )
            .child(
                IconButton::new("ggo-world-undo", IconName::Undo)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Undo"))
                    .on_click(cx.listener(|this, _, _, cx| this.undo_impl(cx))),
            )
            .child(
                IconButton::new("ggo-world-redo", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Redo"))
                    .on_click(cx.listener(|this, _, _, cx| this.redo_impl(cx))),
            )
            .child(
                Button::new("ggo-world-save", "Save")
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, _, cx| this.save_impl(cx))),
            )
            .children(open.save_error.as_ref().map(|e| {
                Label::new(format!("save failed: {e}"))
                    .size(LabelSize::Small)
                    .color(Color::Error)
            }))
            .into_any_element()
    }

    fn render_canvas(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_canvas is only called in the Ready state");
        };

        // Build the paint-ordered draw list from current state + loads --
        // per render (i.e. per notify), matching ggo-ide's
        // per-frame-of-change rebuild; images inside are `Arc` clones.
        // Selection is threaded through so `build_draw_list` emits the
        // `SelectionOutline` overlay.
        let items = draw_items(open);
        let screen_origin = active_camera_origin(&open.store.state());
        let world_center = canvas::camera_center(screen_origin);
        let images = open.images.clone();
        let view = open.view.clone();
        let background = cx.theme().colors().editor_background;
        let text_color = cx.theme().colors().text;

        let element = gpui::canvas(
            move |canvas_bounds, _window, _cx| {
                let mut v = view.borrow_mut();
                v.last_bounds = Some(canvas_bounds);
                if v.pan.is_none() {
                    // First layout: center the active camera's view.
                    v.pan = Some(canvas::centering_pan(
                        f64::from(canvas_bounds.size.width),
                        f64::from(canvas_bounds.size.height),
                        v.zoom,
                        world_center,
                    ));
                }
                canvas::Scene {
                    items,
                    images,
                    zoom: v.zoom,
                    pan: v.pan.expect("initialized above"),
                    screen_origin,
                    background,
                    text_color,
                }
            },
            move |canvas_bounds, scene, window, cx| {
                canvas::paint_scene(&scene, canvas_bounds, window, cx)
            },
        )
        .size_full();

        div()
            .id("ggo-world-canvas")
            .size_full()
            .overflow_hidden()
            .child(element)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    // Take focus so the panel's Undo/Redo/Save bindings
                    // apply (and any in-progress field edit blur-commits).
                    window.focus(&this.focus_handle, cx);
                    let local = {
                        let ViewerState::Ready(open) = &this.state else {
                            return;
                        };
                        let v = open.view.borrow();
                        let Some(bounds) = v.last_bounds else {
                            return;
                        };
                        [
                            f64::from(event.position.x - bounds.origin.x),
                            f64::from(event.position.y - bounds.origin.y),
                        ]
                    };
                    this.canvas_primary_down(local, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    if let ViewerState::Ready(open) = &mut this.state {
                        open.edit_drag = None;
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    let ViewerState::Ready(open) = &this.state else {
                        return;
                    };
                    let mut v = open.view.borrow_mut();
                    if let Some(pan) = v.pan {
                        v.drag = Some(Drag {
                            start_cursor: [
                                f64::from(event.position.x),
                                f64::from(event.position.y),
                            ],
                            start_pan: pan,
                        });
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.handle_pan_move(event, cx) {
                    return;
                }
                let Some(local) = this.edit_drag_local(event) else {
                    return;
                };
                this.canvas_drag_to(local, cx);
            }))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    if let ViewerState::Ready(open) = &this.state {
                        open.view.borrow_mut().drag = None;
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                let ViewerState::Ready(open) = &this.state else {
                    return;
                };
                let mut v = open.view.borrow_mut();
                let (Some(pan), Some(canvas_bounds)) = (v.pan, v.last_bounds) else {
                    return;
                };
                let dy = f32::from(event.delta.pixel_delta(px(20.)).y);
                if dy == 0.0 {
                    return;
                }
                let dir = if dy > 0.0 { 1 } else { -1 };
                let new_zoom = canvas::zoom_step(v.zoom, dir);
                if new_zoom == v.zoom {
                    return;
                }
                let cursor = [
                    f64::from(event.position.x - canvas_bounds.origin.x),
                    f64::from(event.position.y - canvas_bounds.origin.y),
                ];
                v.pan = Some(canvas::zoom_at(pan, v.zoom, cursor, new_zoom));
                v.zoom = new_zoom;
                drop(v);
                cx.notify();
            }))
            .into_any_element()
    }

    /// One inspector text input, wrapped in a minimal bordered box (the
    /// brief's "primitive gpui/ui components" rule -- no widget framework).
    fn editor_input(editor: &Entity<Editor>, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .flex_1()
            .min_w_0()
            .px_1()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .bg(cx.theme().colors().editor_background)
            .child(editor.clone())
            .into_any_element()
    }

    fn field_label(label: &str) -> gpui::AnyElement {
        div()
            .w(px(72.))
            .flex_none()
            .child(
                Label::new(SharedString::from(label.to_string()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_entity_inspector(
        &self,
        entity_ix: usize,
        entity: &ggo_worldlib::world_doc::WorldEntity,
        editors: &HashMap<inspector::FieldTarget, Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("inspector renders only in the Ready state");
        };
        let schemas = &open.schemas;
        let mut col = v_flex()
            .gap_1()
            .child(Label::new(format!("Entity #{entity_ix}")).size(LabelSize::Small));

        for (component, value) in &entity.components {
            let name = component.clone();
            let mut panel = v_flex().gap_1().child(
                h_flex()
                    .justify_between()
                    .child(Label::new(SharedString::from(component.clone())))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("ggo-remove-{component}")),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::XSmall)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Direct undoable removal (ggo-ide's
                            // Transform-with-visual confirm modal is not
                            // ported; undo covers it).
                            this.apply_op(
                                WorldOp::RemoveComponent {
                                    entity: entity_ix,
                                    name: name.clone(),
                                },
                                cx,
                            );
                        })),
                    ),
            );

            match value.as_object() {
                Some(fields) => {
                    for (field, field_value) in fields {
                        match inspector::field_kind(schemas, component, field) {
                            Some(ggo_worldlib::schemas::FieldKind::Bool) => {
                                let checked = field_value.as_bool().unwrap_or(false);
                                let weak = cx.weak_entity();
                                let component = component.clone();
                                let field_name = field.clone();
                                panel = panel.child(
                                    Checkbox::new(
                                        SharedString::from(format!(
                                            "ggo-field-{component}-{field}"
                                        )),
                                        ToggleState::from(checked),
                                    )
                                    .label(SharedString::from(field.clone()))
                                    .on_click(
                                        move |toggle, _window, cx| {
                                            let value = matches!(toggle, ToggleState::Selected);
                                            weak.update(cx, |this, cx| {
                                                this.apply_op(
                                                    WorldOp::SetField {
                                                        entity: entity_ix,
                                                        component: component.clone(),
                                                        field: field_name.clone(),
                                                        value: Value::Bool(value),
                                                    },
                                                    cx,
                                                );
                                            })
                                            .ok();
                                        },
                                    ),
                                );
                            }
                            Some(ggo_worldlib::schemas::FieldKind::Vec2) => {
                                let axis_editor = |axis: usize| {
                                    editors.get(&inspector::FieldTarget::EntityVec2Axis {
                                        entity: entity_ix,
                                        component: component.clone(),
                                        field: field.clone(),
                                        axis,
                                    })
                                };
                                let mut row =
                                    h_flex().gap_1().child(Self::field_label(field.as_str()));
                                for axis in 0..2 {
                                    if let Some(editor) = axis_editor(axis) {
                                        row = row.child(Self::editor_input(editor, cx));
                                    }
                                }
                                panel = panel.child(row);
                            }
                            _ => {
                                let target = inspector::FieldTarget::EntityField {
                                    entity: entity_ix,
                                    component: component.clone(),
                                    field: field.clone(),
                                };
                                if let Some(editor) = editors.get(&target) {
                                    panel = panel.child(
                                        h_flex()
                                            .gap_1()
                                            .child(Self::field_label(field.as_str()))
                                            .child(Self::editor_input(editor, cx)),
                                    );
                                }
                            }
                        }
                    }
                }
                // A component in a non-object state (marker bool, string,
                // array) still shows its raw value with a working Remove
                // button -- ggo-ide dogfood round 2.
                None => {
                    panel = panel.child(
                        Label::new(format!("(non-object value: {value})"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
            }

            col = col.child(panel).child(Divider::horizontal());
        }

        // Add-component picker over every schema not already present,
        // seeded by `defaults_for` -- ggo-ide's `pick_list` flow.
        let addable: Vec<ComponentSchema> = schemas
            .iter()
            .filter(|s| !entity.components.contains_key(&s.name))
            .cloned()
            .collect();
        if !addable.is_empty() {
            let weak = cx.weak_entity();
            let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
                for schema in addable {
                    let weak = weak.clone();
                    let name = schema.name.clone();
                    menu = menu.entry(
                        SharedString::from(name.clone()),
                        None,
                        move |_window, cx| {
                            let defaults = defaults_for(&schema);
                            let name = name.clone();
                            weak.update(cx, |this, cx| {
                                this.apply_op(
                                    WorldOp::AddComponent {
                                        entity: entity_ix,
                                        name,
                                        defaults,
                                    },
                                    cx,
                                );
                            })
                            .ok();
                        },
                    );
                }
                menu
            });
            col = col.child(DropdownMenu::new(
                "ggo-add-component",
                "Add component…",
                menu,
            ));
        }

        col.into_any_element()
    }

    fn render_instance_inspector(
        &self,
        index: usize,
        editors: &HashMap<inspector::FieldTarget, Entity<Editor>>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("inspector renders only in the Ready state");
        };
        let state = open.store.state();
        let Some(instance) = state.instances.get(index) else {
            return Label::new("Nothing selected.")
                .color(Color::Muted)
                .into_any_element();
        };
        let mut col = v_flex()
            .gap_1()
            .child(Label::new(format!("Instance #{index}")).size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_1()
                    .child(Self::field_label("world"))
                    .child(Label::new(SharedString::from(
                        world_label(&instance.world).to_string(),
                    ))),
            );
        let mut row = h_flex().gap_1().child(Self::field_label("pos"));
        for axis in 0..2 {
            if let Some(editor) =
                editors.get(&inspector::FieldTarget::InstancePosAxis { index, axis })
            {
                row = row.child(Self::editor_input(editor, cx));
            }
        }
        col = col.child(row);
        if let Some(error) = &instance.error {
            if !error.is_null() {
                col = col.child(
                    Label::new(error.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                );
            }
        }
        col.into_any_element()
    }

    /// The right-side inspector column -- rendered only with a selection.
    fn render_inspector(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let selection = open.selected?;
        let editors: HashMap<inspector::FieldTarget, Entity<Editor>> = open
            .inspector
            .iter()
            .map(|e| (e.target.clone(), e.editor.clone()))
            .collect();
        let state = open.store.state();
        let body = match selection {
            Selection::Entity(i) => match state.entities.get(i) {
                Some(entity) => {
                    let entity = entity.clone();
                    self.render_entity_inspector(i, &entity, &editors, window, cx)
                }
                None => Label::new("Nothing selected.")
                    .color(Color::Muted)
                    .into_any_element(),
            },
            Selection::Instance(i) => self.render_instance_inspector(i, &editors, cx),
        };
        Some(
            div()
                .id("ggo-world-inspector")
                .w(INSPECTOR_WIDTH)
                .h_full()
                .flex_none()
                .border_l_1()
                .border_color(cx.theme().colors().border)
                .overflow_y_scroll()
                .child(v_flex().p_1().gap_1().child(body))
                .into_any_element(),
        )
    }

    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let inspector = self.render_inspector(window, cx);
        let toolbar = self.render_toolbar(window, cx);
        v_flex()
            .size_full()
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_canvas(cx)),
                    )
                    .children(inspector),
            )
            .into_any_element()
    }
}

impl Render for WorldPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_inspector(window, cx);
        let body = match &self.state {
            ViewerState::Empty => self.render_message("Select a world".to_string(), cx),
            ViewerState::Loading { stem } => self.render_message(format!("Loading {stem}…"), cx),
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(window, cx),
        };
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Undo, _window, cx| this.undo_impl(cx)))
            .on_action(cx.listener(|this, _: &Redo, _window, cx| this.redo_impl(cx)))
            .on_action(cx.listener(|this, _: &Save, _window, cx| this.save_impl(cx)))
            .on_action(
                cx.listener(|this, _: &DeleteSelected, _window, cx| this.delete_selected_impl(cx)),
            )
            .on_action(cx.listener(Self::on_commit_field))
            .bg(cx.theme().colors().panel_background)
            .child(self.render_picker(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for WorldPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorldPanel {}

impl Panel for WorldPanel {
    fn persistent_name() -> &'static str {
        "GGO World"
    }

    fn panel_key() -> &'static str {
        GGO_WORLD_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // No settings persistence yet (see task-5-brief.md); Bottom isn't a
        // sensible spot for a world/map editor sidebar, so only Left/Right.
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        DEFAULT_WIDTH
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Public)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO World")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_worlds` needs to READ the
            // workspace to find the project root -- reading it re-entrantly
            // panics.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_worlds(cx)).ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::render::DrawKind;
    use ggo_worldlib::world_file::{
        WorldEntity, WorldFile, WorldInstance, read_world, write_world,
    };
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use workspace::dock::DockPosition;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock and focuses the panel.
    /// Goes through `MultiWorkspace::test_new` rather than a bare
    /// `Workspace::test_new`, because `register_action` handlers (like
    /// `ToggleFocus`) are only mounted into the dispatch tree once something
    /// renders `Workspace::actions`, which in production is
    /// `MultiWorkspace`'s render (same lesson as ggo_hello's F0 test).
    #[gpui::test]
    async fn test_toggle_focus_opens_panel(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.panel::<WorldPanel>(cx).is_some(),
                "WorldPanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<WorldPanel>(cx)
                .expect("WorldPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    fn entity(components: serde_json::Value) -> WorldEntity {
        WorldEntity {
            components: components.as_object().unwrap().clone(),
        }
    }

    /// Fixture note: the world is built from RectFill/Text/Camera entities
    /// plus a nested `[[instance]]` -- deliberately NO image assets
    /// (authoring a valid `.spr`/`.til`/`.pal`/`.map` set through
    /// `sprites::io` save fns would dwarf the test; the brief allows this
    /// choice). Real-image composition is worldlib-tested; the BGRA bridge
    /// has its own unit test in `canvas`.
    fn write_fixture(root: &std::path::Path) {
        let sub = WorldFile {
            entities: vec![entity(json!({
                "Transform": { "pos": [0.0, 0.0], "z": 1.0 },
                "RectFill": { "w": 8.0, "h": 8.0, "color": 2016.0 }
            }))],
            instances: vec![],
            backgrounds: vec![],
        };
        write_world(root, "worlds/sub.toml", &sub).unwrap();

        let main = WorldFile {
            entities: vec![
                entity(json!({
                    "Transform": { "pos": [4.0, 4.0], "z": 0.0 },
                    "RectFill": { "w": 16.0, "h": 12.0, "color": 63488.0 }
                })),
                entity(json!({
                    "Transform": { "pos": [40.0, 8.0], "z": 2.0 },
                    "Text": { "content": "hello", "max_width": 40.0, "max_height": 12.0 }
                })),
                entity(json!({
                    "Transform": { "pos": [0.0, 0.0], "z": 0.0 },
                    "Camera": { "is_active": true }
                })),
            ],
            instances: vec![WorldInstance {
                world: "worlds/sub".to_string(),
                pos: [32.0, 16.0],
                background_priority: false,
            }],
            backgrounds: vec![],
        };
        write_world(root, "worlds/test.toml", &main).unwrap();
    }

    /// Load `worlds/test` into a fresh panel and return it Ready, with the
    /// camera at identity (pan `[0, 0]`, zoom 1) so canvas-local px ==
    /// world px in the editor tests.
    async fn ready_panel(
        cx: &mut TestAppContext,
        root: &std::path::Path,
    ) -> gpui::Entity<WorldPanel> {
        write_fixture(root);
        let root = root.to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            let ix = panel
                .worlds
                .iter()
                .position(|w| w.stem == "worlds/test")
                .unwrap();
            panel.select_world(ix, cx);
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready state after load");
            };
            open.view.borrow_mut().pan = Some([0.0, 0.0]);
        });
        panel
    }

    /// End-to-end viewer load against a real-fs temp project: picker
    /// enumerates both worlds, selecting `worlds/test` runs the off-thread
    /// loader (including recursive instance resolution of `worlds/sub`),
    /// and the panel reaches Ready with a non-empty draw list containing
    /// both the top-level and the instance-resolved RectFill.
    #[gpui::test]
    async fn test_select_world_reaches_ready_with_draw_items(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });

        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            let stems: Vec<&str> = panel.worlds.iter().map(|w| w.stem.as_str()).collect();
            assert_eq!(stems, ["worlds/sub", "worlds/test"]);
            let ix = panel
                .worlds
                .iter()
                .position(|w| w.stem == "worlds/test")
                .unwrap();
            panel.select_world(ix, cx);
            assert!(matches!(panel.state, ViewerState::Loading { .. }));
        });

        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready state after load");
            };
            let items = draw_items(open);
            assert!(!items.is_empty(), "draw list should not be empty");

            let rects = items
                .iter()
                .filter(|i| matches!(i.kind, DrawKind::Rect { .. }))
                .count();
            assert_eq!(
                rects, 2,
                "one top-level RectFill + one from the resolved worlds/sub instance"
            );
            assert!(
                items
                    .iter()
                    .any(|i| matches!(&i.kind, DrawKind::Text { content } if content == "hello")),
                "Text entity should be in the draw list"
            );
            assert!(
                items
                    .iter()
                    .any(|i| matches!(i.kind, DrawKind::InstanceOrigin)),
                "instance origin gizmo should be in the draw list"
            );
        });
    }

    /// Click select: a primary-down over the RectFill (world [4,4]..[20,16]
    /// at identity view) selects Entity(0) and the rebuilt draw list gains
    /// a SelectionOutline; a primary-down over empty space deselects.
    #[gpui::test]
    async fn test_click_selects_entity_and_empty_click_deselects(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down([10.0, 10.0], cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected, Some(Selection::Entity(0)));
            assert!(
                open.edit_drag.is_some(),
                "a hit should also arm a placement drag"
            );
            let items = draw_items(open);
            assert!(
                items
                    .iter()
                    .any(|i| matches!(i.kind, DrawKind::SelectionOutline)),
                "selection outline should be emitted for the selected entity"
            );

            panel.canvas_primary_down([300.0, 300.0], cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected, None, "empty-space click deselects");
            assert!(open.edit_drag.is_none());
            let items = draw_items(open);
            assert!(
                !items
                    .iter()
                    .any(|i| matches!(i.kind, DrawKind::SelectionOutline)),
                "no outline without a selection"
            );
        });
    }

    /// Drag placement: live gesture moves coalesce into ONE undo entry;
    /// snap lands the result on the tile grid; undo/redo round-trips the
    /// position and the dirty flag.
    #[gpui::test]
    async fn test_drag_moves_entity_with_gesture_coalescing_snap_and_undo_redo(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down([10.0, 10.0], cx);
            panel.canvas_drag_to([26.0, 13.0], cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            // start [4,4] + world delta [16,3].
            assert_eq!(
                inspector::entity_pos(&open.store.state(), 0),
                Some([20.0, 7.0])
            );
            assert!(open.store.state().dirty);
        });

        panel.update(cx, |panel, cx| {
            // Snap mid-gesture: result snaps to the grid, not the delta.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.snap = true;
            }
            panel.canvas_drag_to([27.0, 10.0], cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            // [4,4] + [17,0] = [21,4] -> snapped [16,0].
            assert_eq!(
                inspector::entity_pos(&open.store.state(), 0),
                Some([16.0, 0.0])
            );
        });

        panel.update(cx, |panel, cx| {
            // One undo unwinds the WHOLE drag (gesture coalescing), and
            // clears dirty back to the load state.
            panel.undo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                inspector::entity_pos(&open.store.state(), 0),
                Some([4.0, 4.0])
            );
            assert!(!open.store.state().dirty, "undo back to saved => clean");

            panel.redo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                inspector::entity_pos(&open.store.state(), 0),
                Some([16.0, 0.0])
            );
            assert!(open.store.state().dirty, "redo re-dirties");
        });
    }

    /// Save: `to_doc()` -> `write_world` -> `mark_saved`; the written file
    /// `read_world`-round-trips equal to `to_doc()` and the dirty flag
    /// clears.
    #[gpui::test]
    async fn test_save_writes_file_that_round_trips_and_clears_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [50.0, 60.0],
                    gesture: None,
                },
                cx,
            );
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(open.store.state().dirty);

            panel.save_impl(cx);

            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(open.save_error.is_none(), "save should succeed");
            assert!(!open.store.state().dirty, "mark_saved clears dirty");
            let on_disk = read_world(dir.path(), "worlds/test.toml").unwrap();
            let doc = open.store.to_doc();
            assert!(
                world_files_equal(&on_disk, &doc),
                "written file must read back equal to to_doc(): {on_disk:?} vs {doc:?}"
            );
        });
    }

    /// `WorldFile` equality under JS-number semantics (`50` == `50.0`) --
    /// the TOML writer canonicalizes whole floats to integers, so derived
    /// `PartialEq` (which distinguishes `serde_json` int/float reprs) is
    /// stricter than the actual round-trip contract; worldlib's own
    /// `world_doc::values_equal` makes the same call.
    fn world_files_equal(a: &WorldFile, b: &WorldFile) -> bool {
        fn values_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
            use serde_json::Value;
            match (a, b) {
                (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
                (Value::Array(x), Value::Array(y)) => {
                    x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_equal(a, b))
                }
                (Value::Object(x), Value::Object(y)) => {
                    x.len() == y.len()
                        && x.iter()
                            .all(|(k, v)| y.get(k).is_some_and(|v2| values_equal(v, v2)))
                }
                _ => a == b,
            }
        }
        a.entities.len() == b.entities.len()
            && a.entities.iter().zip(&b.entities).all(|(x, y)| {
                values_equal(
                    &serde_json::Value::Object(x.components.clone()),
                    &serde_json::Value::Object(y.components.clone()),
                )
            })
            && a.instances == b.instances
            && a.backgrounds == b.backgrounds
    }

    /// M7 add/delete: the toolbar add drops a Transform-only entity at
    /// the view center (schema defaults, `pos` overridden), selected;
    /// it renders as a `Marker` in the draw list; `DeleteSelected`
    /// removes it (and clears the selection); undo restores it and a
    /// second undo returns to the loaded, clean state.
    #[gpui::test]
    async fn test_add_entity_appears_in_draw_list_delete_and_undo_restores(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let markers_before = {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                draw_items(open)
                    .iter()
                    .filter(|i| matches!(i.kind, DrawKind::Marker))
                    .count()
            };

            panel.add_entity_impl(cx);
            let center = {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                let center = view_center_world(open);
                let state = open.store.state();
                assert_eq!(state.entities.len(), 4);
                assert_eq!(open.selected, Some(Selection::Entity(3)));
                assert!(state.dirty, "add dirties the doc");
                let t = state.entities[3].components["Transform"]
                    .as_object()
                    .expect("Transform skeleton");
                assert_eq!(t["pos"], json!([center[0], center[1]]));
                assert_eq!(t["z"], json!(0), "defaults_for seeds the z field");
                let markers = draw_items(open)
                    .iter()
                    .filter(|i| matches!(i.kind, DrawKind::Marker))
                    .count();
                assert_eq!(
                    markers,
                    markers_before + 1,
                    "the Transform-only entity draws as a Marker"
                );
                center
            };

            panel.delete_selected_impl(cx);
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                assert_eq!(open.store.state().entities.len(), 3);
                assert_eq!(open.selected, None, "delete clears the selection");
            }

            // Stale-selection guard: nothing selected => no-op.
            panel.delete_selected_impl(cx);
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                assert_eq!(open.store.state().entities.len(), 3);
            }

            panel.undo_impl(cx);
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                let state = open.store.state();
                assert_eq!(state.entities.len(), 4, "undo restores the deleted entity");
                assert_eq!(
                    state.entities[3].components["Transform"]["pos"],
                    json!([center[0], center[1]])
                );
            }
            panel.undo_impl(cx);
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                assert_eq!(open.store.state().entities.len(), 3);
                assert!(!open.store.state().dirty, "back to the loaded state");
            }
        });
    }

    /// M7 add-instance: candidates exclude the open world itself and any
    /// stem the load proved cycles in the resolved instance graph (a <->
    /// b fixture); a guarded pick is dropped without an op; a legal pick
    /// adds the instance at the view center, selected.
    #[gpui::test]
    async fn test_add_instance_honors_cycle_guard(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Mutually-recursive pair on top of the standard fixture:
        // worlds/a instances worlds/b, worlds/b instances worlds/a.
        let cyclic = |other: &str| WorldFile {
            entities: vec![],
            instances: vec![WorldInstance {
                world: format!("worlds/{other}"),
                pos: [0.0, 0.0],
                background_priority: false,
            }],
            backgrounds: vec![],
        };
        write_world(root, "worlds/a.toml", &cyclic("b")).unwrap();
        write_world(root, "worlds/b.toml", &cyclic("a")).unwrap();

        let panel = ready_panel(cx, root).await;
        panel.update(cx, |panel, cx| {
            let ix = panel
                .worlds
                .iter()
                .position(|w| w.stem == "worlds/a")
                .unwrap();
            panel.select_world(ix, cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            let candidates = panel.instance_candidates();
            assert!(
                !candidates.contains(&"worlds/a".to_string()),
                "the open world is never a candidate"
            );
            assert!(
                !candidates.contains(&"worlds/b".to_string()),
                "a stem the load proved cyclic is excluded"
            );
            assert!(
                candidates.contains(&"worlds/sub".to_string()),
                "unrelated worlds stay offered: {candidates:?}"
            );

            // A guarded pick (self or proven-cyclic) is dropped.
            panel.add_instance_impl("worlds/a".to_string(), cx);
            panel.add_instance_impl("worlds/b".to_string(), cx);
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                assert_eq!(
                    open.store.state().instances.len(),
                    1,
                    "guarded picks must not add instances"
                );
                assert!(!open.store.state().dirty);
            }

            // A legal pick lands at the view center, selected.
            panel.add_instance_impl("worlds/sub".to_string(), cx);
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                let center = view_center_world(open);
                let state = open.store.state();
                assert_eq!(state.instances.len(), 2);
                assert_eq!(state.instances[1].world, "worlds/sub");
                assert_eq!(state.instances[1].pos, center);
                assert_eq!(open.selected, Some(Selection::Instance(1)));
                assert!(state.dirty);
            }
        });
    }

    /// M7 stale-root regression: repointing the panel's project root
    /// (workspace/worktree change + refresh) while a world is open must
    /// not redirect Save -- the doc writes back under the root it was
    /// LOADED from, and nothing appears under the new root.
    #[gpui::test]
    async fn test_save_after_root_repoint_writes_under_captured_root(cx: &mut TestAppContext) {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir1.path()).await;

        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    // Non-integral on purpose: the TOML writer
                    // canonicalizes whole floats to ints, which would
                    // trip the exact json! comparison below.
                    pos: [77.5, 88.25],
                    gesture: None,
                },
                cx,
            );

            // Repoint the panel at a different (empty) root, as a
            // worktree change + activation refresh would.
            panel.root_override = Some(dir2.path().to_path_buf());
            panel.refresh_worlds(cx);
            assert_eq!(panel.project_root.as_deref(), Some(dir2.path()));

            panel.save_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(open.save_error.is_none(), "save should succeed");
            assert!(!open.store.state().dirty);
        });

        let saved = read_world(dir1.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            saved.entities[0].components["Transform"]["pos"],
            json!([77.5, 88.25]),
            "the edit was saved under the LOAD root"
        );
        assert!(
            !dir2.path().join("worlds").exists(),
            "nothing may be written under the repointed root"
        );
    }

    /// Field commits map inspector text to ops against the LIVE panel
    /// store: an int field edit applies SetField; undo restores it.
    #[gpui::test]
    async fn test_commit_field_applies_set_field_through_the_store(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected = Some(Selection::Entity(0));
            let state = open.store.state();
            let target = inspector::FieldTarget::EntityField {
                entity: 0,
                component: "Transform".to_string(),
                field: "z".to_string(),
            };
            let op = inspector::commit_field(&target, "7", &state, &open.schemas).unwrap();
            open.store.apply(op);
            assert_eq!(
                open.store.state().entities[0].components["Transform"]["z"],
                json!(7)
            );
            panel.undo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            // The TOML reader canonicalizes the fixture's `0.0` to the
            // integer `0`; undo restores that exact stored value.
            assert_eq!(
                open.store.state().entities[0].components["Transform"]["z"],
                json!(0)
            );
        });
    }
}
