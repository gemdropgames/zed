//! GGO World panel: a dock panel that renders the open `worlds/**.toml`
//! with real pixels (composed sprite/map images via `ggo-worldlib`) and
//! edits it: click select, drag placement (live `WorldOp` moves coalesced
//! per gesture), a schema-driven inspector, undo/redo, and save. Which
//! world is open is driven ENTIRELY by the file explorer (F4 X1): clicking
//! a `**/worlds/**/*.toml` there routes here through
//! [`intercept_world_open`]; the panel has no picker of its own. The ASSET
//! ROOT the document (and every stem inside it) resolves against is derived
//! from that clicked path -- see [`split_world_path`].
//!
//! Split: `loader` owns everything that runs off the UI thread (world
//! read, instance resolution, asset composition, manifest schemas);
//! `canvas` owns camera math, drag math and painting; `inspector` owns the
//! pure field-target/commit logic; this module owns the panel entity, the
//! state machine, and all gpui wiring.
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
use ggo_worldlib::world_files::{self, WorldListing};
use project::ProjectPath;

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

    // Explorer-driven routing: clicking a `**/worlds/**/*.toml` in the project
    // panel loads it HERE instead of opening a TOML editor tab. This is the
    // panel's only way in -- there is no in-panel world picker.
    workspace::register_path_open_interceptor(cx, intercept_world_open);

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

/// Empty-state text. The panel has no picker of its own by design (F4 X1):
/// worlds arrive by clicking a `**/worlds/**/*.toml` in the project panel.
const EMPTY_MESSAGE: &str = "Open a world file from the project panel";

/// Byte index of the LAST `worlds/` path COMPONENT in `rel`, or `None` when
/// `rel` has none. Component-anchored: `myworlds/x.toml` does not match,
/// only a segment that IS `worlds`.
fn last_worlds_dir_index(rel: &str) -> Option<usize> {
    rel.rmatch_indices(world_files::WORLDS_DIR)
        .map(|(i, _)| i)
        .find(|i| *i == 0 || rel.as_bytes()[i - 1] == b'/')
}

/// Split a worktree-relative path into its ASSET ROOT (worktree-relative,
/// `""` for the worktree root itself) and the asset-root-relative world
/// listing -- or `None` when `rel` is not a world file.
///
/// Worlds do NOT live at `<project>/worlds` in a real project: they live
/// under an asset root, `<project>/assets/worlds/*.toml`, and that asset
/// root is what EVERY stem in the engine's world format resolves against --
/// `emerald.toml`'s `default_world = "worlds/boot"`, each
/// `[[instance]] world = "worlds/arena"`, each `[[background]]` map path and
/// each `Sprite`/`MetaSprite`/`Tilemap` asset stem. So the root can't be
/// hardcoded; it is DERIVED here from the clicked path by splitting at the
/// last `worlds/` component:
///
/// | clicked rel                     | asset root      | world rel         |
/// |---------------------------------|-----------------|-------------------|
/// | `assets/worlds/main.toml`       | `assets`        | `worlds/main.toml`|
/// | `worlds/main.toml`              | `` (worktree)   | `worlds/main.toml`|
/// | `assets/worlds/sub/worlds/x.toml` | `assets/worlds/sub` | `worlds/x.toml` |
///
/// The tail is validated by worldlib's own `world_files` rule, so the
/// "what counts as a world file" half stays single-sourced. The result
/// accepts exactly what the syntax-highlighting glob a GGO project's
/// `.zed/settings.json` uses accepts (`**/worlds/**/*.toml`, see
/// `ggo_language::PROJECT_FILE_TYPE_GLOB`); the two were divergent until F4
/// and the glob was the one that was right. A bare `foo.toml`, a `.toml`
/// outside any `worlds/` directory, or a FILE named `worlds.toml` is NOT a
/// world and must not hijack this panel.
fn split_world_path(rel: &str) -> Option<(String, WorldListing)> {
    let cut = last_worlds_dir_index(rel)?;
    let listing = world_files::world_files(std::slice::from_ref(&rel[cut..].to_string()))
        .into_iter()
        .next()?;
    Some((rel[..cut].trim_end_matches('/').to_string(), listing))
}

/// `workspace::PathOpenInterceptor` for `**/worlds/**/*.toml`: claim the
/// path, open the panel, and load it. Declines (so the normal open path
/// runs) for any other file, for a path outside the primary worktree, and
/// when no panel is docked.
fn intercept_world_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    if split_world_path(&rel).is_none() {
        return false;
    }
    ggo_common::open_in_panel(
        workspace,
        window,
        cx,
        move |panel: &mut WorldPanel, window, cx| panel.open_rel_path(&rel, window, cx),
    )
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
    /// The world's listing RELATIVE TO [`Self::root`] (e.g. stem
    /// `worlds/main`), which is the same frame every engine-side stem
    /// resolves in -- not the worktree-relative path that was clicked.
    listing: WorldListing,
    /// The worktree-relative path as CLICKED (e.g.
    /// `assets/worlds/main.toml`). Kept alongside the listing because it,
    /// not the asset-root-relative rel, is what identifies the file to the
    /// explorer and to the user: it answers "is this click the world that
    /// is already open?" and it is what the unsaved-edits prompt names.
    source_rel: String,
    /// The ASSET ROOT this world was LOADED from -- `<worktree>/assets`
    /// for `assets/worlds/main.toml`, the worktree root for
    /// `worlds/main.toml` (see [`split_world_path`]). Save writes under
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
        source_rel: String,
        root: PathBuf,
        loaded: loader::LoadedWorld,
        images: HashMap<usize, Arc<RenderImage>>,
    ) -> Self {
        OpenWorld {
            listing,
            source_rel,
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
    /// worktree) and re-enumerate the worlds under the ACTIVE asset root.
    /// Runs on every panel activation -- the walk only touches
    /// `<asset root>/worlds`, so it's cheap.
    ///
    /// The listing is no longer a picker feed (F4 X1 removed the picker);
    /// it survives because `AddInstance` needs the set of OTHER worlds this
    /// one may instance -- see [`Self::instance_candidates`]. That set has
    /// to come from the OPEN document's asset root, not the worktree root:
    /// an `[[instance]]` stem resolves against the same root its parent
    /// world did, so enumerating `<worktree>/worlds` while
    /// `<worktree>/assets/worlds/main.toml` is open would offer stems that
    /// resolve to nothing.
    ///
    /// MUST NOT run while the workspace itself is mid-update (it reads the
    /// workspace entity) -- see the deferral in `set_active` and in
    /// [`Self::open_rel_path`].
    fn refresh_worlds(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        self.worlds = match self.asset_root() {
            Some(root) => loader::list_worlds(&root),
            None => Vec::new(),
        };
        cx.notify();
    }

    /// The root world stems are enumerated and resolved against: the open
    /// document's derived asset root while one is loaded, else the
    /// worktree root (nothing better is known before the first open).
    fn asset_root(&self) -> Option<PathBuf> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.root.clone()),
            _ => self.project_root.clone(),
        }
    }

    /// Load the project-relative world path `rel`, prompting FIRST if the
    /// open world has unsaved edits -- Cancel leaves the current document
    /// loaded and dirty and abandons the open. This is the panel's entry
    /// point from the file explorer ([`intercept_world_open`]); there is no
    /// in-panel picker.
    ///
    /// Everything after the guard runs on a spawned task, deliberately: the
    /// interceptor calls this from INSIDE the workspace's own update, and
    /// [`Self::refresh_worlds`] has to read that same workspace entity.
    pub fn open_rel_path(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Clicking the file that is ALREADY open is how you bring the panel
        // back into focus, and upstream's semantics for that click on a tab
        // are "activate the existing item", not "reload it". The interceptor
        // has already revealed and focused the dock by the time we get here,
        // so there is nothing left to do -- and doing anything would either
        // prompt (offering a "Don't Save" the user never asked for) or drop
        // the undo stack, selection and camera on the floor.
        if let ViewerState::Ready(open) = &self.state
            && open.source_rel == rel
        {
            return;
        }
        let rel = rel.to_string();
        let proceed = ggo_common::prepare_to_close_dirty(
            self.dirty_world_name(),
            window,
            cx,
            Self::save_for_close,
        );
        cx.spawn(async move |this, cx| {
            if !proceed.await {
                return;
            }
            this.update(cx, |this, cx| {
                this.refresh_worlds(cx);
                this.load_rel_path(&rel, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Kick off the off-thread load of the worktree-relative path `rel`,
    /// against the asset root DERIVED from it ([`split_world_path`]). A
    /// stale result (superseded by a later open) is dropped by generation
    /// check.
    fn load_rel_path(&mut self, rel: &str, cx: &mut Context<Self>) {
        let Some((asset_root_rel, listing)) = split_world_path(rel) else {
            return;
        };
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let root = if asset_root_rel.is_empty() {
            project_root
        } else {
            project_root.join(&asset_root_rel)
        };
        let source_rel = rel.to_string();
        // Re-enumerate under the root THIS document resolves against, so
        // `+ Instance` offers stems that actually resolve. `refresh_worlds`
        // ran before this against the previously-open root.
        self.worlds = loader::list_worlds(&root);
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
                        source_rel,
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
    /// undo returns it to the default spot, second removes it), select
    /// it, and resolve its subtree so it renders immediately.
    fn add_instance_impl(&mut self, stem: String, cx: &mut Context<Self>) {
        if !self.instance_candidates().contains(&stem) {
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let center = view_center_world(open);
        open.store.apply(WorldOp::AddInstance {
            world: stem.clone(),
        });
        let index = open.store.state().instances.len() - 1;
        open.store.apply(WorldOp::MoveInstance {
            index,
            pos: center,
            gesture: None,
        });
        open.selected = Some(Selection::Instance(index));
        open.edit_drag = None;

        // Resolve the new stem's subtree and stamp it NOW -- ggo-ide
        // re-resolves after every message (`dispatch_new_asset_loads`),
        // so parity means the added instance's entities/assets render
        // without a reload, not just its origin marker (M7 review, fix
        // round 1). Synchronous by choice: small TOML IO at world-add
        // frequency, the same trade as `save_impl`. A failed resolve
        // stamps the instance's error badge; it never fails the add.
        let result = loader::resolve_instance(&open.root, &stem);
        open.store.set_instances_resolved(&stem, &result, true);
        // Compose whatever load targets the subtree introduced and fold
        // them into the RenderImage cache (existing entries keep their
        // images -- `fill_missing_asset_loads` never recomposes).
        let state = open.store.state();
        loader::fill_missing_asset_loads(
            &open.root,
            &state,
            &mut open.sprite_loads,
            &mut open.map_loads,
            &mut open.meta_sprite_loads,
        );
        open.images = Arc::new(canvas::build_image_cache(&[
            &open.sprite_loads,
            &open.map_loads,
            &open.meta_sprite_loads,
        ]));
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

    /// Save on behalf of a "Save" answer to the unsaved-edits prompt,
    /// reporting whether the write actually landed (a failed write must not
    /// let the caller discard the document). Shared by
    /// [`Panel::prepare_to_close`] and [`Self::open_rel_path`].
    fn save_for_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_impl(cx);
        match &self.state {
            ViewerState::Ready(open) => open.save_error.is_none(),
            // The panel can't have gone un-Ready between the prompt and
            // here, but if it somehow did there is nothing left to lose.
            _ => true,
        }
    }

    /// The open world's display path when it has unsaved edits, else
    /// `None`. Drives both the close guard and (indirectly) the title's
    /// dirty dot.
    fn dirty_world_name(&self) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        // The CLICKED path, not the asset-root-relative one: the prompt has
        // to name the file the way the user sees it in the explorer.
        open.store.state().dirty.then(|| open.source_rel.clone())
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
            ViewerState::Empty => self.render_message(EMPTY_MESSAGE.to_string(), cx),
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

    /// The open world lives in panel state, not in a workspace `Item`, so
    /// nothing else in the close flow knows it can be dirty. Prompt with
    /// the same Save/Don't-Save/Cancel warning a dirty buffer gets; a
    /// failed write cancels the close rather than dropping the edits.
    fn prepare_to_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        ggo_common::prepare_to_close_dirty(
            self.dirty_world_name(),
            window,
            cx,
            Self::save_for_close,
        )
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
    use project::{FakeFs, Project, WorktreeId};
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
    /// `MultiWorkspace`'s render (the lesson the F0 fork-wiring smoke test
    /// taught, carried into every GGO panel's registration test since).
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
            panel.load_rel_path("worlds/test.toml", cx);
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
            panel.load_rel_path("worlds/test.toml", cx);
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

    /// F4 regression (user-reported): real projects keep their worlds under
    /// an ASSET ROOT -- `<project>/assets/worlds/*.toml` -- because that is
    /// the root `emerald.toml`'s `default_world = "worlds/boot"` stem (and
    /// every `[[instance]] world = "worlds/…"` stem, and every sprite/map
    /// asset stem) resolves against. Clicking `assets/worlds/test.toml` must
    /// load, with the asset root DERIVED from the clicked path.
    ///
    /// The rect count is the load-bearing assertion: it only reaches 2 if
    /// the derived root was threaded into instance resolution, not merely
    /// used to read the top-level file (a wrong root resolves the
    /// `worlds/sub` instance to nothing and silently renders a placeholder).
    #[gpui::test]
    async fn test_asset_root_world_loads_and_resolves_its_instance_subtree(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // The whole fixture lives under `<root>/assets`, exactly the
        // `~/projects/wilds` layout.
        write_fixture(&dir.path().join("assets"));

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });

        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            panel.load_rel_path("assets/worlds/test.toml", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready after loading a world under an asset root");
            };
            assert_eq!(
                open.root,
                dir.path().join("assets"),
                "the asset root must be derived from the clicked path"
            );
            assert_eq!(
                open.listing.stem, "worlds/test",
                "the listing is asset-root-relative, matching engine-side stems"
            );
            let rects = draw_items(open)
                .iter()
                .filter(|i| matches!(i.kind, DrawKind::Rect { .. }))
                .count();
            assert_eq!(
                rects, 2,
                "top-level RectFill + the worlds/sub instance's, which only \
                 resolves if the derived root reached instance resolution"
            );
            assert_eq!(
                panel.instance_candidates(),
                vec!["worlds/sub".to_string()],
                "+ Instance must enumerate from the SAME derived root"
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
            panel.load_rel_path("worlds/a.toml", cx);
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

    /// M7 fix round 1: a freshly added instance's subtree must render
    /// WITHOUT a reload -- add resolves the stem and composes its
    /// assets immediately (ggo-ide re-resolves after every message).
    /// `worlds/sub` carries a RectFill child, so the draw list's rect
    /// count must grow right after the add.
    #[gpui::test]
    async fn test_add_instance_resolves_subtree_without_reload(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let rect_count = |open: &OpenWorld| {
                draw_items(open)
                    .iter()
                    .filter(|i| matches!(i.kind, DrawKind::Rect { .. }))
                    .count()
            };
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                assert_eq!(
                    rect_count(open),
                    2,
                    "fixture baseline: top-level RectFill + the existing sub instance's"
                );
            }

            panel.add_instance_impl("worlds/sub".to_string(), cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let state = open.store.state();
            assert_eq!(state.instances.len(), 2);
            assert!(
                state.instances[1].resolved.is_some(),
                "the subtree is resolved at add time"
            );
            assert!(state.instances[1].error.is_none());
            assert_eq!(
                rect_count(open),
                3,
                "the new instance's RectFill child renders without a reload"
            );
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

    // ------------------------------------------- unsaved-document guard

    /// Dirty the open world so the close guard has something to protect,
    /// without going through the canvas.
    fn dirty_the_world(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [50.0, 60.0],
                    gesture: None,
                },
                cx,
            );
            assert!(
                panel.dirty_world_name().is_some(),
                "op should dirty the doc"
            );
        });
    }

    /// A clean panel must be invisible to the close flow: no prompt, and
    /// the guard resolves `true` immediately.
    #[gpui::test]
    async fn test_close_guard_lets_a_clean_panel_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        assert!(
            !cx.has_pending_prompt(),
            "a clean world must not prompt on close"
        );
        assert!(close.await, "a clean panel must not block the close");
    }

    /// Cancel aborts the close and leaves the document dirty and unwritten
    /// -- the data-loss guard proper.
    #[gpui::test]
    async fn test_close_guard_cancel_aborts_the_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_world(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some("worlds/test.toml contains unsaved edits. Do you want to save it?".to_string()),
        );
        cx.simulate_prompt_answer("Cancel");
        assert!(!close.await, "Cancel must veto the close");

        panel.update(cx, |panel, _cx| {
            assert!(
                panel.dirty_world_name().is_some(),
                "Cancel must leave the edits in place"
            );
        });
        let on_disk = read_world(dir.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([4, 4]),
            "Cancel must not have written the file"
        );
    }

    /// Save writes through the panel's own save path and then allows the
    /// close.
    #[gpui::test]
    async fn test_close_guard_save_writes_then_allows_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_world(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        cx.simulate_prompt_answer("Save");
        assert!(close.await, "a successful save must allow the close");

        panel.update(cx, |panel, _cx| {
            assert!(panel.dirty_world_name().is_none(), "save clears dirty");
        });
        let on_disk = read_world(dir.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([50, 60]),
            "Save must have written the edit"
        );
    }

    /// "Don't Save" closes and deliberately drops the edits -- the file on
    /// disk keeps its loaded contents.
    #[gpui::test]
    async fn test_close_guard_discard_allows_close_without_writing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_world(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        cx.simulate_prompt_answer("Don't Save");
        assert!(close.await, "Don't Save must allow the close");

        let on_disk = read_world(dir.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([4, 4]),
            "Don't Save must not write the file"
        );
    }

    /// The wiring test: a dirty panel docked in a REAL workspace makes
    /// `Workspace::prepare_to_close` (the single funnel for window close,
    /// quit and restart) prompt and, on Cancel, report `false` -- which is
    /// what `MultiWorkspace::close_window` and `zed::quit` honour.
    #[gpui::test]
    async fn test_dirty_panel_vetoes_workspace_prepare_to_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldPanel>(cx)
                .expect("init() adds the panel")
        });

        // Point the docked panel at the real temp project and open a world.
        panel.update(cx, |panel, cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.refresh_worlds(cx);
            panel.load_rel_path("worlds/test.toml", cx);
        });
        cx.run_until_parked();
        dirty_the_world(&panel, cx);

        let close = workspace.update_in(cx, |workspace, window, cx| {
            workspace.prepare_to_close(workspace::CloseIntent::CloseWindow, window, cx)
        });
        cx.run_until_parked();
        assert!(
            cx.has_pending_prompt(),
            "the docked dirty panel must be polled by Workspace::prepare_to_close"
        );
        cx.simulate_prompt_answer("Cancel");
        assert!(
            !close.await.unwrap(),
            "Cancel in the panel guard must cancel the whole close"
        );
    }

    // ------------------------------------------ explorer-driven routing

    /// The predicate that decides what the panel claims from the file
    /// explorer: a `worlds/` directory ANYWHERE in the path with a `.toml`
    /// leaf, NOT a bare `.toml` test -- a stray `Cargo.toml` click must
    /// still open an editor.
    ///
    /// This is exactly `ggo_language::PROJECT_FILE_TYPE_GLOB`
    /// (`**/worlds/**/*.toml`, which `ggo_language::tests` pins as matching
    /// `assets/worlds/deep/nested/arena.toml`). The two used to diverge --
    /// routing accepted only a TOP-LEVEL `worlds/` -- and the glob was the
    /// one that was right: real projects keep worlds under an asset root,
    /// so the narrow predicate silently declined every real world file.
    #[gpui::test]
    fn test_world_predicate_matches_only_world_files(_cx: &mut gpui::App) {
        let stem = |rel: &str| split_world_path(rel).map(|(_, l)| l.stem);
        assert_eq!(stem("worlds/test.toml"), Some("worlds/test".to_string()));
        assert_eq!(
            stem("worlds/nested/arena.toml"),
            Some("worlds/nested/arena".to_string()),
            "nested worlds under a worlds/ dir count"
        );
        assert_eq!(
            stem("assets/worlds/main.toml"),
            Some("worlds/main".to_string()),
            "the real-project layout: worlds under an asset root"
        );
        assert_eq!(
            stem("deep/nested/worlds/x.toml"),
            Some("worlds/x".to_string()),
            "a worlds/ dir at ANY depth routes"
        );

        assert!(
            split_world_path("Cargo.toml").is_none(),
            "a bare .toml is not a world"
        );
        assert!(
            split_world_path("assets/worlds.toml").is_none(),
            "a FILE named worlds is not a worlds/ DIRECTORY"
        );
        assert!(
            split_world_path("worlds/readme.md").is_none(),
            "only .toml leaves count"
        );
        assert!(
            split_world_path("myworlds/x.toml").is_none(),
            "the worlds/ match is anchored to a whole path component"
        );
    }

    /// Root derivation: the asset root is everything BEFORE the last
    /// `worlds/` component, and the listing is asset-root-relative -- which
    /// is the frame `[[instance]]`/sprite/map stems resolve in.
    #[gpui::test]
    fn test_split_world_path_derives_the_asset_root(_cx: &mut gpui::App) {
        let split = |rel: &str| {
            let (root, listing) = split_world_path(rel).expect("a world path");
            (root, listing.rel_path, listing.stem)
        };
        assert_eq!(
            split("assets/worlds/main.toml"),
            (
                "assets".to_string(),
                "worlds/main.toml".to_string(),
                "worlds/main".to_string()
            )
        );
        assert_eq!(
            split("worlds/main.toml"),
            (
                // Empty root == the worktree root itself: the pre-F4
                // behaviour, preserved exactly.
                String::new(),
                "worlds/main.toml".to_string(),
                "worlds/main".to_string()
            )
        );
        assert_eq!(
            split("deep/nested/worlds/x.toml"),
            (
                "deep/nested".to_string(),
                "worlds/x.toml".to_string(),
                "worlds/x".to_string()
            )
        );
        assert_eq!(
            split("assets/worlds/sub/worlds/x.toml"),
            (
                // LAST worlds/ wins: the inner one is the world directory,
                // everything left of it is the root.
                "assets/worlds/sub".to_string(),
                "worlds/x.toml".to_string(),
                "worlds/x".to_string()
            ),
        );
        assert_eq!(
            split("assets/worlds/nested/arena.toml"),
            (
                // A nested dir INSIDE worlds/ stays part of the stem.
                "assets".to_string(),
                "worlds/nested/arena.toml".to_string(),
                "worlds/nested/arena".to_string()
            )
        );
    }

    /// A fake-fs project with one visible worktree holding the same file
    /// names the real-fs `root` fixture does: the interceptor only needs a
    /// worktree id and a rel path, while the panel reads the actual world
    /// TOML through `std::fs` from `root` (`root_override`).
    async fn routed_project(
        cx: &mut TestAppContext,
        root: &std::path::Path,
        run_init: bool,
    ) -> Entity<Project> {
        write_fixture(root);
        cx.update(|cx| {
            AppState::test(cx);
            if run_init {
                init(cx);
            }
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            json!({
                "worlds": { "test.toml": "", "sub.toml": "" },
                "Cargo.toml": "",
            }),
        )
        .await;
        Project::test(fs, ["/proj".as_ref()], cx).await
    }

    fn project_path(worktree_id: WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    fn worktree_id(project: &Entity<Project>, cx: &mut gpui::VisualTestContext) -> WorktreeId {
        project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("one visible worktree")
                .read(cx)
                .id()
        })
    }

    /// With nothing registered, `intercept_path_open` claims nothing --
    /// i.e. the upstream open path is completely unchanged, world files
    /// included. `init` is deliberately NOT run here.
    #[gpui::test]
    async fn test_empty_interceptor_registry_claims_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), false).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "worlds/test.toml"),
                window,
                cx,
            )
        });
        assert!(
            !claimed,
            "an empty registry must never claim a path -- upstream behaviour byte for byte"
        );
    }

    /// The registered world predicate claims `**/worlds/**/*.toml` (so the
    /// project panel opens NO pane item for it), opens the dock, and loads
    /// the world -- while a root-level `Cargo.toml` is declined.
    #[gpui::test]
    async fn test_world_click_routes_into_the_panel_and_is_claimed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldPanel>(cx)
                .expect("init() adds the panel")
        });
        let root = dir.path().to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "worlds/test.toml"),
                window,
                cx,
            )
        });
        assert!(
            claimed,
            "a world file must be claimed, suppressing the pane item"
        );
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready after routing");
            };
            assert_eq!(open.listing.rel_path, "worlds/test.toml");
        });
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "routing must open the panel's dock even if it was closed"
            );
        });

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "Cargo.toml"), window, cx)
        });
        assert!(
            !claimed,
            "a .toml outside worlds/ must still open in the editor"
        );
    }

    /// A clean panel switches worlds without a prompt.
    #[gpui::test]
    async fn test_open_rel_path_switches_a_clean_panel_directly(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("worlds/sub.toml", window, cx)
            })
        });
        assert!(
            !cx.has_pending_prompt(),
            "a clean panel must switch without asking"
        );
        cx.run_until_parked();
        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready after the switch");
            };
            assert_eq!(open.listing.rel_path, "worlds/sub.toml");
        });
    }

    /// Clicking the file that is ALREADY open must be a pure focus/reveal:
    /// no prompt (a dirty doc would otherwise be offered a "Don't Save" the
    /// user never asked for) and no reload (which would silently drop the
    /// undo stack, selection and camera). The undo assertion is the
    /// load-bearing one -- a reload would leave the entity at its on-disk
    /// `[4,4]` with nothing to undo.
    #[gpui::test]
    async fn test_open_rel_path_on_the_open_world_does_not_reload(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_world(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("worlds/test.toml", window, cx)
            })
        });
        assert!(
            !cx.has_pending_prompt(),
            "re-opening the open world must not prompt"
        );
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                inspector::entity_pos(&open.store.state(), 0),
                Some([50.0, 60.0]),
                "the in-memory edit must survive an already-open click"
            );
            assert!(open.store.state().dirty, "and the doc must still be dirty");

            panel.undo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                inspector::entity_pos(&open.store.state(), 0),
                Some([4.0, 4.0]),
                "the undo stack must have survived too"
            );
        });
    }

    /// The data-loss guard: a file-tree click while the open world has
    /// unsaved edits must PROMPT, and Cancel must abort the open -- the
    /// previously loaded document stays loaded, stays dirty, and stays
    /// unwritten.
    #[gpui::test]
    async fn test_open_rel_path_cancel_keeps_the_dirty_document(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_world(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("worlds/sub.toml", window, cx)
            })
        });
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some("worlds/test.toml contains unsaved edits. Do you want to save it?".to_string()),
            "switching away from a dirty world must prompt first"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("Cancel must leave the panel Ready");
            };
            assert_eq!(
                open.listing.rel_path, "worlds/test.toml",
                "Cancel must abort the open and leave the current world loaded"
            );
            assert!(open.store.state().dirty, "and leave its edits in place");
        });
        let on_disk = read_world(dir.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([4, 4]),
            "Cancel must not have written the file"
        );
    }
}
