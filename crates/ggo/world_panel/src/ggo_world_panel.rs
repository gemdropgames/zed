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
use std::path::{Path, PathBuf};
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
use ggo_worldlib::schemas::{ComponentSchema, FieldKind, defaults_for};
use ggo_worldlib::sprites::palette565;
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
        /// Moves focus to the next inspector field (bound to Tab inside
        /// the panel's field editors); the blur commits the current one.
        FocusNextField,
        /// Moves focus to the previous inspector field (bound to Shift-Tab
        /// inside the panel's field editors).
        FocusPrevField,
        /// Deletes the selected entity or instance from the open world.
        DeleteSelected,
        /// Resets the canvas camera to the default framing.
        ResetView,
        /// Nudges the selection one pixel left.
        NudgeLeft,
        /// Nudges the selection one pixel right.
        NudgeRight,
        /// Nudges the selection one pixel up.
        NudgeUp,
        /// Nudges the selection one pixel down.
        NudgeDown,
        /// Nudges the selection one tile left.
        NudgeLeftTile,
        /// Nudges the selection one tile right.
        NudgeRightTile,
        /// Nudges the selection one tile up.
        NudgeUpTile,
        /// Nudges the selection one tile down.
        NudgeDownTile
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

    // Right-clicking that same `**/worlds/**/*.toml` offers the file ops
    // that need to know it IS a world -- currently just "Delete World".
    workspace::register_context_menu_contributor(cx, contribute_world_menu);

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

/// Screen px one arrow keypress pans the camera when nothing is selected
/// (Shift: 4x).
const CAMERA_PAN_STEP_PX: f64 = 32.0;

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

/// The component whose `stem` the inspector offers a "go to sprite" jump
/// for, and the extension that stem resolves with. A `MetaSprite`'s stem is
/// ASSET-ROOT-relative and extensionless (`sprites/hero`), the same frame
/// `loader::compose_meta_sprite_rgba` opens it in (`{stem}.spr`).
const META_SPRITE: &str = "MetaSprite";
const SPRITE_COMPONENT: &str = "Sprite";
const SPRITE_EXT: &str = "spr";

/// How many stem suggestions render under a focused Asset field.
const STEM_SUGGESTION_CAP: usize = 8;

/// The WORKTREE-relative `.spr` path an asset-root-relative `stem` names,
/// or `None` when it does not resolve to a file that exists.
///
/// Two frames are in play and they are not the same one (the F4 asset-root
/// split): the stem resolves under `asset_root` (`<worktree>/assets` for an
/// `assets/worlds/main.toml`), while `SpritePanel::open_rel_path` -- like
/// every explorer-driven open -- takes a path relative to the WORKTREE.
/// So this joins in the first frame and re-relativizes into the second.
///
/// Declines rather than guessing when: the stem names nothing on disk (a
/// world may legitimately reference a sprite that has not been authored
/// yet, and handing the sprite panel a missing path would only park it in
/// an error state), or the asset root is not inside the worktree at all
/// (nothing worktree-relative to hand over).
/// Every asset-root-relative, extensionless `/`-separated stem with
/// extension `ext` under `root`, sorted -- the completion feed for one
/// Asset-kind inspector field. Recursive on purpose: a fresh project's
/// `sprites/gg_icon.spr` (or any nested layout) must surface.
fn list_asset_stems(root: &Path, ext: &str) -> Vec<String> {
    let mut stems = Vec::new();
    walk_asset_stems(root, root, ext, &mut stems);
    stems.sort();
    stems
}

fn walk_asset_stems(root: &Path, dir: &Path, ext: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_asset_stems(root, &path, ext, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(
                rel.with_extension("")
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            );
        }
    }
}

fn sprite_rel_for_stem(project_root: &Path, asset_root: &Path, stem: &str) -> Option<String> {
    if stem.is_empty() {
        return None;
    }
    let abs = asset_root.join(format!("{stem}.{SPRITE_EXT}"));
    if !abs.is_file() {
        return None;
    }
    let rel = abs.strip_prefix(project_root).ok()?;
    Some(
        rel.to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
}

/// The assets-root-relative world STEM a worktree-relative path names --
/// `assets/worlds/main.toml` -> `worlds/main` -- or `None` when the path
/// is not a world file at all.
///
/// This is the identity emerald itself uses for a world everywhere it
/// names one: `emerald.toml`'s `default_world`, `[[instance]] world`, and
/// `emd build/pack/pack-ggo --world <stem>` (which is sugar for setting
/// `EMERALD_DEFAULT_WORLD`). Exported because `ggo_emu_panel`'s "Emulate
/// this world" entry needs BOTH halves of what this module already knows
/// -- the "is this a world?" predicate and the stem to bake in -- and a
/// second copy of the `worlds/`-splitting rule over there is exactly the
/// drift the fork's single-source rule exists to stop. It hands back a
/// `String` rather than worldlib's `WorldListing` so the emu panel does
/// not have to depend on worldlib for it.
pub fn world_stem(rel: &str) -> Option<String> {
    split_world_path(rel).map(|(_, listing)| listing.stem)
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

/// `workspace::ContextMenuContributor` for `**/worlds/**/*.toml`: the world
/// file ops the project panel's own menu can't offer, because upstream has
/// no idea a world is anything but a `.toml`.
///
/// Gated on exactly the predicate [`intercept_world_open`] uses --
/// [`rel_in_primary_worktree`] then [`split_world_path`], no second copy of
/// the rule -- so the menu and the click agree on what a world is by
/// construction. Contributes nothing for a directory, for a non-world file,
/// or for a path outside the primary worktree of a local project.
///
/// MUST NOT touch the project panel or any GGO panel: contributors run
/// while `ProjectPanel` is leased (see
/// `Workspace::context_menu_contributions`). Everything panel-shaped is
/// deferred into the entry's handler via [`ggo_common::panel_entry_handler`],
/// which runs after the lease is released.
///
/// [`rel_in_primary_worktree`]: ggo_common::rel_in_primary_worktree
fn contribute_world_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    if is_dir {
        return Vec::new();
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    if split_world_path(&rel).is_none() {
        return Vec::new();
    }
    vec![
        ui::ContextMenuEntry::new("Delete World")
            .icon(ui::IconName::Trash)
            .handler(delete_world_handler(cx.weak_entity(), rel))
            .into(),
    ]
}

/// The "Delete World" entry's handler: reach the panel, hand it the
/// worktree-relative path, and let it prompt. Split out from
/// [`contribute_world_menu`] so a test can invoke exactly what the menu
/// invokes -- `ContextMenuEntry` keeps its handler private, so a
/// contributed entry cannot be fired from a test any other way.
fn delete_world_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(workspace, move |panel: &Entity<WorldPanel>, window, cx| {
        let rel = rel.clone();
        panel
            .update(cx, |panel, cx| panel.delete_world(rel, window, cx))
            .detach();
    })
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
        // Arrow-key nudge, same panel-focused-only rule: the arrows keep
        // moving the cursor while an inspector field editor has focus,
        // because `Editor` is the deeper context.
        KeyBinding::new("left", NudgeLeft, Some(KEY_CONTEXT)),
        KeyBinding::new("right", NudgeRight, Some(KEY_CONTEXT)),
        KeyBinding::new("up", NudgeUp, Some(KEY_CONTEXT)),
        KeyBinding::new("down", NudgeDown, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-left", NudgeLeftTile, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-right", NudgeRightTile, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-up", NudgeUpTile, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-down", NudgeDownTile, Some(KEY_CONTEXT)),
        // Single-line editors don't bind Enter themselves (the default
        // keymap's `enter -> editor::Newline` is `mode == full` only), so
        // this fires while an inspector field editor is focused.
        KeyBinding::new(
            "enter",
            CommitField,
            Some(&format!("{KEY_CONTEXT} > Editor")),
        ),
        // These outrank the default keymap's `Editor`-context
        // `tab -> editor::Tab`: same dispatch depth (both match at the
        // editor node), and later-added bindings win -- `init` re-adds
        // these after every keymap reload.
        KeyBinding::new(
            "tab",
            FocusNextField,
            Some(&format!("{KEY_CONTEXT} > Editor")),
        ),
        KeyBinding::new(
            "shift-tab",
            FocusPrevField,
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
    /// The doc-derived text last pushed into the editor. Refresh compares
    /// against THIS, not the editor's live buffer: a draw runs between a
    /// focus change and the old editor's `Blurred` delivery (gpui fires
    /// focus-out listeners after the draw), so comparing the buffer would
    /// clobber a just-typed, not-yet-committed value with the stale doc
    /// text.
    last_display: String,
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
    /// Tile-grid overlay under the draw list -- ggo-ide `open.grid`, which
    /// also defaults ON.
    grid: bool,
    edit_drag: Option<EditDrag>,
    /// The gesture id an in-flight RUN of arrow-key nudges shares, so the
    /// store coalesces the run into one undo entry the way it coalesces a
    /// drag. `None` between runs; see [`WorldPanel::nudge_impl`].
    nudge_gesture: Option<String>,
    gesture_counter: u64,
    inspector: Vec<InspectorEntry>,
    save_error: Option<String>,
    /// The open palette color picker, if any -- at most one, anchored to
    /// one Color565 field.
    color_picker: Option<ColorPicker>,
    /// The focused Asset field's completion feed, if any -- at most one.
    /// Recomputed only when focus moves onto a DIFFERENT asset field
    /// ([`WorldPanel::refresh_stem_completion`]), so the directory walk
    /// runs once per focus, not per frame.
    stem_completion: Option<StemCompletion>,
}

/// One Asset-kind field's stem candidates: every `{stem}.{ext}` under the
/// open world's asset root, asset-root-relative and extensionless -- the
/// exact frame the engine's tags resolve in (`sprite_tag`/`map_tag`
/// append the extension back).
struct StemCompletion {
    target: inspector::FieldTarget,
    stems: Vec<String>,
}

/// Inline palette picker state for one Color565 inspector field: pick a
/// project `.pal`, then either take an existing slot's color or write a
/// new RGB565 color into the selected slot (saving the `.pal`).
struct ColorPicker {
    target: inspector::FieldTarget,
    /// Valid `.pal` rel_paths under the project root, scanned on open.
    candidates: Vec<String>,
    pal_rel: Option<String>,
    palette: Option<ggo_worldlib::sprites::palette565::Pal>,
    slot: Option<usize>,
    r_editor: Entity<Editor>,
    g_editor: Entity<Editor>,
    b_editor: Entity<Editor>,
    error: Option<String>,
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
                zoom: canvas::ZOOM_DEFAULT,
                pan: None,
                last_bounds: None,
                drag: None,
            })),
            selected: None,
            snap: false,
            grid: true,
            edit_drag: None,
            nudge_gesture: None,
            gesture_counter: 0,
            inspector: Vec::new(),
            save_error: None,
            color_picker: None,
            stem_completion: None,
        }
    }
}

/// Packed RGB565 -> gpui color, via the PPU's own 565->888 expansion.
fn color565_rgba(c: u16) -> gpui::Rgba {
    let (r, g, b) = ggo_asset_formats::pixel::rgb888(c);
    gpui::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
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

    /// The world currently open, as the worktree-relative path it was
    /// opened WITH -- `None` unless a world is loaded.
    ///
    /// Public purely as an observation point for the panels that hand a
    /// world OFF to this one: `ggo_emerald_panel` opens the world
    /// `emd generate world` just wrote, and the rel it passes has to be
    /// the worktree-relative one. Without this, that hand-off could only
    /// be asserted from inside this crate, which is not where the bug
    /// would be. (Same reason `ggo_tileset_panel::open_rel_path_now`
    /// exists.)
    pub fn open_rel_path_now(&self) -> Option<&str> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.source_rel.as_str()),
            _ => None,
        }
    }

    /// The inspector's current component-schema names. An observation
    /// point for the same reason as [`Self::open_rel_path_now`]:
    /// `ggo_emerald_panel` CHANGES this set from outside (via
    /// [`Self::refresh_schemas`]) and has to be able to prove it.
    pub fn schema_names(&self) -> Vec<String> {
        match &self.state {
            ViewerState::Ready(open) => open.schemas.iter().map(|s| s.name.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Re-read `manifests/components.toml` into the OPEN world's inspector
    /// schema set, without reloading the document.
    ///
    /// The schema set is otherwise built exactly once, at load time
    /// ([`loader::load_world`]), so a component created outside this panel
    /// -- `ggo_emerald_panel`'s `emd generate component`, which calls this
    /// -- would not be offerable on an entity until the world was closed
    /// and reopened. A no-op when nothing is loaded: the next load reads
    /// the manifest fresh anyway.
    ///
    /// Reads against the OPEN document's own asset root (via
    /// [`loader::schemas_near`], which walks up to the project root), not
    /// the panel's live `project_root` -- same stale-root reasoning as
    /// save's.
    pub fn refresh_schemas(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.schemas = loader::schemas_near(&open.root);
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

    /// Confirm, then delete the world file at worktree-relative `rel` --
    /// the body of the project panel's "Delete World" entry
    /// ([`contribute_world_menu`]).
    ///
    /// The prompt names the world by its STEM (`worlds/main`, the name
    /// every `[[instance]]` and `default_world` refers to it by) as well as
    /// by the file, because those differ under an asset root and only the
    /// stem tells you what will break elsewhere.
    ///
    /// Refreshes FIRST because `project_root` is only re-discovered on
    /// panel activation, and a right-click in the explorer can reach a
    /// panel that has never been activated. Safe here: the caller is a
    /// context-menu entry handler, which runs outside both the project
    /// panel's lease and any `Workspace` update (see
    /// [`ggo_common::panel_entry_handler`]).
    ///
    /// Deliberately scoped to one file: a world referenced by another
    /// world's `[[instance]]` is NOT chased down and unlinked, and a failed
    /// unlink leaves the panel exactly as it was rather than half-clearing
    /// it. Returns the `Task` so tests can await the whole prompt->delete
    /// round trip; the menu handler detaches it.
    fn delete_world(
        &mut self,
        rel: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        self.refresh_worlds(cx);
        let Some(project_root) = self.project_root.clone() else {
            return Task::ready(());
        };
        let Some((_, listing)) = split_world_path(&rel) else {
            return Task::ready(());
        };
        // Named, not offered a save: deleting the file makes an unsaved edit
        // to it moot, so this warns instead of routing through
        // `prepare_to_close_dirty` (which would offer to write bytes that
        // are about to be unlinked). ggo-ide's delete made the same call.
        let unsaved = self.dirty_world_name().is_some_and(|name| name == rel);
        let confirm = ggo_common::confirm_destructive(
            &format!("Delete the world \"{}\" ({rel})?", listing.stem),
            "Delete",
            unsaved,
            window,
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !confirm.await {
                return;
            }
            if let Err(e) = std::fs::remove_file(project_root.join(&rel)) {
                // No toast yet (F5.2 owns the notification surface), but a
                // silent no-op would be indistinguishable from a bug.
                // Upstream logs AND toasts at the same point.
                log::error!("GGO: failed to delete world {rel}: {e}");
                return;
            }
            this.update(cx, |this, cx| {
                // The open document's file is gone: showing it would offer
                // edits, undo and a save that all target nothing.
                if matches!(&this.state, ViewerState::Ready(open) if open.source_rel == rel) {
                    this.state = ViewerState::Empty;
                }
                // Re-enumerate so `+ Instance` stops offering the world
                // that no longer exists.
                this.refresh_worlds(cx);
            })
            .ok();
        })
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
        open.nudge_gesture = None;
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
        open.nudge_gesture = None;

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
        open.nudge_gesture = None;
        cx.notify();
    }

    /// Move the selected entity/instance by one nudge step and repaint --
    /// the arrow keys (`key` is the JS-style name worldlib's
    /// [`drag_ops::nudge_delta`] takes; `tile` is the Shift modifier, one
    /// tile instead of one pixel). Snap applies to the RESULT, exactly as
    /// it does for a drag.
    ///
    /// Goes through the SAME `MoveEntity`/`MoveInstance` ops the drag path
    /// applies, WITH a gesture id: a run of nudges shares one id, so the
    /// store amends its top undo entry instead of pushing one per
    /// keypress, and a single undo puts the item back where the run
    /// started. That id is minted lazily here and torn down by
    /// [`Self::end_nudge_run`] at every event that means "the run is over"
    /// -- a new selection, a drag, a delete, an undo/redo.
    ///
    /// **Deviation from ggo-ide**, which passes `gesture: None` and gets
    /// one undo entry per keypress; holding an arrow key there buries the
    /// pre-nudge position under dozens of entries.
    fn nudge_impl(&mut self, key: &str, tile: bool, cx: &mut Context<Self>) {
        let Some(delta) = drag_ops::nudge_delta(key, tile) else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // Nothing selected: the arrows look around instead -- pan the
        // camera opposite the content (arrow right slides content left).
        // `delta`'s sign is the world direction of the keypress, so the pan
        // step reuses it; the magnitude is the camera's own (screen px, not
        // the nudge's world px). No pan before first layout: nothing to do.
        let Some(selection) = open.selected else {
            let step = if tile {
                CAMERA_PAN_STEP_PX * 4.0
            } else {
                CAMERA_PAN_STEP_PX
            };
            let mut view = open.view.borrow_mut();
            let Some(pan) = view.pan else {
                return;
            };
            // Not `signum`: `0.0_f64.signum()` is 1.0, which would drag the
            // cross axis along.
            let sign = |axis: f64| {
                if axis > 0.0 {
                    1.0
                } else if axis < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            };
            view.pan = Some([
                pan[0] - sign(delta[0]) * step,
                pan[1] - sign(delta[1]) * step,
            ]);
            drop(view);
            cx.notify();
            return;
        };
        let state = open.store.state();
        let pos = match selection {
            Selection::Entity(index) => inspector::entity_pos(&state, index),
            Selection::Instance(index) => state.instances.get(index).map(|inst| inst.pos),
        };
        // A selection gone stale against an undo/redo restructure, or an
        // entity with no Transform, has nothing to move.
        let Some(pos) = pos else {
            return;
        };
        let mut next = [pos[0] + delta[0], pos[1] + delta[1]];
        if open.snap {
            next = drag_ops::snap_to_tile(next);
        }
        let gesture = match &open.nudge_gesture {
            Some(id) => id.clone(),
            None => {
                open.gesture_counter += 1;
                let id = format!("nudge-{}", open.gesture_counter);
                open.nudge_gesture = Some(id.clone());
                id
            }
        };
        match selection {
            Selection::Entity(entity) => open.store.apply(WorldOp::MoveEntity {
                entity,
                pos: next,
                gesture: Some(gesture),
            }),
            Selection::Instance(index) => open.store.apply(WorldOp::MoveInstance {
                index,
                pos: next,
                gesture: Some(gesture),
            }),
        }
        cx.notify();
    }

    /// The worktree-relative `.spr` path entity `entity_ix`'s `MetaSprite`
    /// component points at, or `None` when the entity has no `MetaSprite`,
    /// its `stem` is missing/blank, or the stem doesn't resolve to a file
    /// ([`sprite_rel_for_stem`]). Drives BOTH whether the inspector offers
    /// the jump and where it goes, so the button can't exist without a
    /// destination.
    fn goto_sprite_target(&self, entity_ix: usize, component: &str) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let project_root = self.project_root.as_ref()?;
        let stem = open
            .store
            .state()
            .entities
            .get(entity_ix)?
            .components
            .get(component)?
            .get("stem")?
            .as_str()?
            .to_string();
        sprite_rel_for_stem(project_root, &open.root, &stem)
    }

    /// Open `rel` in the sprite panel -- the `MetaSprite` inspector's
    /// "go to sprite" jump. A workspace with no sprite panel docked is a
    /// no-op, the same graceful degradation every other GGO panel handoff
    /// makes; the `bool` (did a panel claim it?) is `open_in_panel`'s own
    /// return, surfaced so a test can tell the two apart.
    fn goto_sprite(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return false;
        };
        workspace.update(cx, |workspace, cx| {
            ggo_common::open_in_panel(
                workspace,
                window,
                cx,
                move |panel: &mut ggo_sprite_panel::SpritePanel, window, cx| {
                    panel.open_rel_path(&rel, window, cx);
                },
            )
        })
    }

    /// Seal any in-flight nudge run, so the NEXT nudge starts a fresh undo
    /// entry rather than amending the last one. Called from every path
    /// that changes what a nudge would even be moving.
    fn end_nudge_run(&mut self) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.nudge_gesture = None;
        }
    }

    /// Restore the default camera framing -- `canvas::reset_camera`'s pair,
    /// with the `None` pan letting the next prepaint re-center on the
    /// active camera (see that function's doc).
    fn reset_view_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let (zoom, pan) = canvas::reset_camera();
        let mut view = open.view.borrow_mut();
        view.zoom = zoom;
        view.pan = pan;
        drop(view);
        cx.notify();
    }

    fn set_grid(&mut self, on: bool, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.grid = on;
            cx.notify();
        }
    }

    /// Set the camera zoom from the zoom bar or its `-`/`+` buttons,
    /// keeping the world point at the CANVAS CENTER fixed (these controls
    /// have no cursor to anchor on, unlike wheel zoom). Before the first
    /// layout there are no bounds and no pan: only the zoom is set, and the
    /// initial centering still runs on the next paint.
    fn set_zoom(&mut self, new_zoom: f64, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let new_zoom = new_zoom.clamp(canvas::ZOOM_MIN, canvas::ZOOM_MAX);
        let mut view = open.view.borrow_mut();
        if new_zoom == view.zoom {
            return;
        }
        if let (Some(pan), Some(bounds)) = (view.pan, view.last_bounds) {
            let center = [
                f64::from(bounds.size.width) / 2.0,
                f64::from(bounds.size.height) / 2.0,
            ];
            view.pan = Some(canvas::zoom_at(pan, view.zoom, center, new_zoom));
        }
        view.zoom = new_zoom;
        drop(view);
        cx.notify();
    }

    fn step_zoom(&mut self, dir: i32, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let zoom = open.view.borrow().zoom;
        self.set_zoom(canvas::zoom_step(zoom, dir), cx);
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        self.end_nudge_run();
        if let ViewerState::Ready(open) = &mut self.state
            && open.store.undo()
        {
            cx.notify();
        }
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        self.end_nudge_run();
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

    /// Write the open world if -- and only if -- it IS `rel` and has
    /// unsaved edits; returns whether `rel` is on disk in the state the
    /// user can see, i.e. `false` only when a needed write actually
    /// failed.
    ///
    /// The "save" half of the emu panel's "Emulate this world": building a
    /// cart from a world the user has edited but not written would boot
    /// the stale file, silently. No prompt, unlike
    /// [`ggo_common::prepare_to_close_dirty`] -- the user asked to run
    /// THIS world, so writing it is the thing they asked for, and a
    /// Save/Don't-Save dialog in front of a build is a click that can only
    /// produce a wrong answer. A clean panel, a different world, or no
    /// world open at all is a no-op `true`.
    pub fn save_if_open_and_dirty(&mut self, rel: &str, cx: &mut Context<Self>) -> bool {
        if self.dirty_world_name().as_deref() != Some(rel) {
            return true;
        }
        self.save_for_close(cx)
    }

    /// Read this panel's documents from `root` instead of the workspace's
    /// first visible worktree.
    ///
    /// `test-support` only. `ggo_emu_panel`'s "Emulate this world saves
    /// first" tests need this panel loading from a REAL directory (its
    /// FakeFs worktree is only there to make `ProjectPath`s resolve), and
    /// `root_override` is crate-private -- the same shape the panel's own
    /// tests use, exported the way `project`/`workspace` export theirs.
    #[cfg(feature = "test-support")]
    pub fn test_root_override(&mut self, root: std::path::PathBuf) {
        self.root_override = Some(root);
    }

    /// Make the open document dirty, and report whether it now is.
    ///
    /// `test-support` only, and deliberately op-free: the caller
    /// (`ggo_emu_panel`) does not depend on worldlib and has no business
    /// naming a `WorldOp`. It only needs "a document with unsaved edits",
    /// which is the precondition `save_if_open_and_dirty` acts on. The
    /// edit itself is the same `MoveEntity` this module's own
    /// `dirty_the_world` helper uses.
    #[cfg(feature = "test-support")]
    pub fn test_dirty_open_world(&mut self, cx: &mut Context<Self>) -> bool {
        self.apply_op(
            WorldOp::MoveEntity {
                entity: 0,
                pos: [50.0, 60.0],
                gesture: None,
            },
            cx,
        );
        self.dirty_world_name().is_some()
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
        // A click ends whatever nudge run was in flight, whether or not it
        // lands on the same item: the next arrow key starts a fresh undo
        // entry, not an amendment of one from before the click.
        open.nudge_gesture = None;
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
            for entry in &mut open.inspector {
                let text = inspector::display_text(&entry.target, &state, &open.schemas);
                if entry.last_display == text {
                    continue;
                }
                entry.last_display = text.clone();
                if entry.editor.focus_handle(cx).is_focused(window) {
                    continue;
                }
                entry
                    .editor
                    .update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
            return;
        }

        let mut entries = Vec::with_capacity(specs.len());
        for spec in &specs {
            let text = inspector::display_text(&spec.target, &state, &open.schemas);
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(text.clone(), window, cx);
                editor
            });
            let subscription = cx.subscribe_in(&editor, window, Self::handle_editor_event);
            entries.push(InspectorEntry {
                target: spec.target.clone(),
                editor,
                last_display: text,
                _subscription: subscription,
            });
        }
        open.inspector = entries;
    }

    /// Keep [`OpenWorld::stem_completion`] pointed at the focused Asset
    /// field: cleared when no asset field is focused, rescanned when the
    /// focus moved onto a different one. Runs from `render`, right after
    /// [`Self::ensure_inspector`].
    fn refresh_stem_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let focused = open
            .inspector
            .iter()
            .find(|entry| entry.editor.focus_handle(cx).is_focused(window))
            .and_then(|entry| {
                inspector::asset_field_ext(&entry.target, &open.schemas)
                    .map(|ext| (entry.target.clone(), ext))
            });
        match focused {
            None => open.stem_completion = None,
            Some((target, ext)) => {
                if open.stem_completion.as_ref().map(|c| &c.target) != Some(&target) {
                    open.stem_completion = Some(StemCompletion {
                        stems: list_asset_stems(&open.root, &ext),
                        target,
                    });
                    cx.notify();
                }
            }
        }
    }

    /// A clicked suggestion: fill the field's editor with `stem` and
    /// commit it -- the same path Enter takes, so undo and resync behave
    /// identically.
    fn pick_stem(
        &mut self,
        target: inspector::FieldTarget,
        stem: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(editor) = open
            .inspector
            .iter()
            .find(|entry| entry.target == target)
            .map(|entry| entry.editor.clone())
        else {
            return;
        };
        editor.update(cx, |editor, cx| editor.set_text(stem, window, cx));
        self.commit_editor(editor.entity_id(), cx);
        cx.notify();
    }

    /// Blur commits the field, matching the brief's enter/blur rule (and
    /// ggo-ide's cross-field commit-on-input). An unchanged or unparsable
    /// buffer is a no-op in the store, so committing every blur is safe.
    /// After the commit the editor is resynced from the doc, so a dropped
    /// (unparsable) buffer reverts instead of lingering on screen.
    fn handle_editor_event(
        &mut self,
        editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, EditorEvent::Blurred) {
            return;
        }
        self.commit_editor(editor.entity_id(), cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        if let Some(entry) = open
            .inspector
            .iter_mut()
            .find(|e| e.editor.entity_id() == editor.entity_id())
        {
            let text = inspector::display_text(&entry.target, &state, &open.schemas);
            entry.last_display = text.clone();
            if entry.editor.read(cx).text(cx) != text {
                entry
                    .editor
                    .update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
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

    /// Tab/shift-tab between inspector fields: the focus move blurs the
    /// current editor, which commits it.
    fn focus_field_sibling(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(current) = open
            .inspector
            .iter()
            .position(|e| e.editor.focus_handle(cx).is_focused(window))
        else {
            return;
        };
        let len = open.inspector.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        if let Some(entry) = open.inspector.get(next) {
            entry.editor.focus_handle(cx).focus(window, cx);
        }
    }

    /// The toolbar's Emulate button: boot the cart to the world this panel
    /// is viewing, via `ggo_common`'s emulator registry (`ggo_emu_panel`
    /// depends on this crate, so it cannot be called directly). Deferred
    /// because the registered emulator saves THIS panel's doc through its
    /// own `update`, which must not nest inside this one.
    fn emulate_impl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let rel = open.source_rel.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                if !ggo_common::emulate_world(workspace, &rel, window, cx) {
                    log::warn!("no emulator pane is available to boot {rel}");
                }
            });
        });
    }

    // ------------------------------------------------- color picker

    /// Open the palette picker for `target` (or close it if it is already
    /// open there). The first `.pal` found in the project is preselected.
    fn toggle_color_picker(
        &mut self,
        target: inspector::FieldTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        {
            let ViewerState::Ready(open) = &mut self.state else {
                return;
            };
            if open
                .color_picker
                .as_ref()
                .is_some_and(|p| p.target == target)
            {
                open.color_picker = None;
                cx.notify();
                return;
            }
        }
        let mut candidates = ggo_worldlib::sprites::io::list_pals(&project_root);
        let mut error = None;
        if candidates.is_empty() {
            // No palette anywhere in the project: seed `main.pal` at the
            // asset root with the default ramp so the picker always has a
            // base palette to offer.
            if let Some(rel) = self.default_pal_rel(&project_root) {
                match ggo_worldlib::sprites::io::save_pal(
                    &project_root,
                    &rel,
                    &ggo_worldlib::sprites::sprite_doc::default_palette(),
                ) {
                    Ok(()) => candidates = vec![rel],
                    Err(e) => error = Some(e.to_string()),
                }
            }
        }
        let first = candidates.first().cloned();
        let picker = ColorPicker {
            target,
            candidates,
            pal_rel: None,
            palette: None,
            slot: None,
            r_editor: cx.new(|cx| Editor::single_line(window, cx)),
            g_editor: cx.new(|cx| Editor::single_line(window, cx)),
            b_editor: cx.new(|cx| Editor::single_line(window, cx)),
            error,
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.color_picker = Some(picker);
        if let Some(rel) = first {
            self.picker_select_pal(rel, cx);
        }
        cx.notify();
    }

    /// Where the seeded default palette goes: `main.pal` at the ASSET root
    /// (`assets/main.pal` in an assets-dir project), as a rel path under
    /// the project root.
    fn default_pal_rel(&self, project_root: &Path) -> Option<String> {
        let asset_root = self.asset_root()?;
        let rel = asset_root.strip_prefix(project_root).ok()?.join("main.pal");
        Some(rel.to_string_lossy().replace('\\', "/"))
    }

    fn picker_select_pal(&mut self, rel: String, cx: &mut Context<Self>) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(picker) = &mut open.color_picker else {
            return;
        };
        match ggo_worldlib::sprites::io::open_pal(&project_root, &rel) {
            Ok(palette) => {
                picker.pal_rel = Some(rel);
                picker.palette = Some(palette);
                picker.slot = None;
                picker.error = None;
            }
            Err(e) => picker.error = Some(e.to_string()),
        }
        cx.notify();
    }

    /// Click a swatch: the slot's existing color becomes the field value,
    /// and the channel inputs are prefilled with it for editing.
    fn picker_select_slot(&mut self, slot: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(picker) = &mut open.color_picker else {
            return;
        };
        let Some(palette) = picker.palette else {
            return;
        };
        let Some(&color) = palette.get(slot) else {
            return;
        };
        picker.slot = Some(slot);
        picker.error = None;
        let target = picker.target.clone();
        let channel_editors = [
            picker.r_editor.clone(),
            picker.g_editor.clone(),
            picker.b_editor.clone(),
        ];
        let (r, g, b) = palette565::unpack_rgb565(color);
        for (editor, value) in channel_editors.iter().zip([r, g, b]) {
            editor.update(cx, |editor, cx| {
                editor.set_text(value.to_string(), window, cx)
            });
        }
        self.set_color_field(&target, color, cx);
    }

    /// "Set": pack the numeric R/G/B channels, write them into the
    /// selected slot of the selected `.pal` (saved atomically), and make
    /// that color the field value.
    fn picker_set_slot_color(&mut self, cx: &mut Context<Self>) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(picker) = &mut open.color_picker else {
            return;
        };
        let channels = [
            (picker.r_editor.clone(), palette565::R5_MAX),
            (picker.g_editor.clone(), palette565::G6_MAX),
            (picker.b_editor.clone(), palette565::B5_MAX),
        ]
        .map(|(editor, max)| {
            let text = editor.read(cx).text(cx);
            text.trim().parse::<u16>().ok().filter(|v| *v <= max)
        });

        let result = (|| -> Result<(inspector::FieldTarget, u16), String> {
            let slot = picker.slot.ok_or("select a palette slot first")?;
            if slot == palette565::TRANSPARENT_SLOT {
                return Err(format!("slot {slot} is the locked transparent slot"));
            }
            let (Some(rel), Some(mut palette)) = (picker.pal_rel.clone(), picker.palette) else {
                return Err("select a .pal first".to_string());
            };
            let [Some(r), Some(g), Some(b)] = channels else {
                return Err(format!(
                    "channels are r 0-{}, g 0-{}, b 0-{}",
                    palette565::R5_MAX,
                    palette565::G6_MAX,
                    palette565::B5_MAX
                ));
            };
            let color = palette565::pack_rgb565(r, g, b);
            if let Some(entry) = palette.get_mut(slot) {
                *entry = color;
            }
            ggo_worldlib::sprites::io::save_pal(&project_root, &rel, &palette)
                .map_err(|e| e.to_string())?;
            picker.palette = Some(palette);
            picker.error = None;
            Ok((picker.target.clone(), color))
        })();
        match result {
            Ok((target, color)) => self.set_color_field(&target, color, cx),
            Err(message) => {
                if let ViewerState::Ready(open) = &mut self.state
                    && let Some(picker) = &mut open.color_picker
                {
                    picker.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    fn set_color_field(
        &mut self,
        target: &inspector::FieldTarget,
        color: u16,
        cx: &mut Context<Self>,
    ) {
        let inspector::FieldTarget::EntityField {
            entity,
            component,
            field,
        } = target
        else {
            return;
        };
        self.apply_op(
            WorldOp::SetField {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
                value: Value::from(i64::from(color)),
            },
            cx,
        );
    }

    /// The inline picker block rendered under an open Color565 field row.
    /// The suggestion list under the focused Asset field: the completion
    /// feed fuzzy-ranked by the buffer, capped at [`STEM_SUGGESTION_CAP`].
    /// Absent when the feed points at another field, nothing matches, or
    /// the buffer already IS the single match.
    // ponytail: click-to-pick only; arrow-key navigation when it itches.
    fn render_stem_suggestions(
        &self,
        target: &inspector::FieldTarget,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let completion = open
            .stem_completion
            .as_ref()
            .filter(|completion| &completion.target == target)?;
        let typed = editor.read(cx).text(cx);
        let ranked = inspector::rank_stem_matches(&typed, &completion.stems);
        if ranked.is_empty() || ranked == [typed.trim().to_string()] {
            return None;
        }
        let mut list = v_flex().pl_2();
        for (ix, stem) in ranked.into_iter().take(STEM_SUGGESTION_CAP).enumerate() {
            let target = target.clone();
            let selector_stem = stem.clone();
            let pick = stem.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("ggo-stem-suggestion-{ix}")))
                    .debug_selector(move || format!("ggo-stem-suggestion-{selector_stem}"))
                    .px_1()
                    .rounded_xs()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .child(
                        Label::new(SharedString::from(stem))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick_stem(target.clone(), pick.clone(), window, cx)
                    })),
            );
        }
        Some(list.into_any_element())
    }

    fn render_color_picker(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            return gpui::Empty.into_any_element();
        };
        let Some(picker) = &open.color_picker else {
            return gpui::Empty.into_any_element();
        };

        let weak = cx.weak_entity();
        let candidates = picker.candidates.clone();
        let pal_menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for rel in candidates {
                let weak = weak.clone();
                let label = rel.clone();
                menu = menu.entry(SharedString::from(label), None, move |_window, cx| {
                    let rel = rel.clone();
                    weak.update(cx, |this, cx| this.picker_select_pal(rel, cx))
                        .ok();
                });
            }
            menu
        });

        let mut block = v_flex()
            .gap_1()
            .p_1()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .child(
                h_flex()
                    .gap_1()
                    .child(Self::field_label("palette"))
                    .child(DropdownMenu::new(
                        "ggo-color-pal",
                        SharedString::from(
                            picker
                                .pal_rel
                                .clone()
                                .unwrap_or_else(|| "Select .pal".to_string()),
                        ),
                        pal_menu,
                    )),
            );
        if picker.candidates.is_empty() {
            block = block.child(
                Label::new("no .pal files in this project")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        if let Some(palette) = &picker.palette {
            let mut grid = h_flex().gap_1().flex_wrap();
            for (i, &c) in palette.iter().enumerate() {
                let selected = picker.slot == Some(i);
                grid = grid.child(
                    div()
                        .id(i)
                        .size_4()
                        .flex_none()
                        .rounded_xs()
                        .border_1()
                        .border_color(if selected {
                            cx.theme().colors().border_focused
                        } else {
                            cx.theme().colors().border
                        })
                        .bg(color565_rgba(c))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.picker_select_slot(i, window, cx)
                        })),
                );
            }
            block = block.child(grid).child(
                h_flex()
                    .gap_1()
                    .child(Label::new("R").size(LabelSize::XSmall).color(Color::Muted))
                    .child(Self::editor_input(&picker.r_editor, cx))
                    .child(Label::new("G").size(LabelSize::XSmall).color(Color::Muted))
                    .child(Self::editor_input(&picker.g_editor, cx))
                    .child(Label::new("B").size(LabelSize::XSmall).color(Color::Muted))
                    .child(Self::editor_input(&picker.b_editor, cx))
                    .child(
                        Button::new("ggo-color-set-slot", "Set")
                            .on_click(cx.listener(|this, _, _, cx| this.picker_set_slot_color(cx))),
                    ),
            );
        }
        if let Some(error) = &picker.error {
            block = block.child(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            );
        }
        block.into_any_element()
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
                IconButton::new("ggo-world-emulate", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Emulate this world (cart)"))
                    .on_click(cx.listener(|this, _, window, cx| this.emulate_impl(window, cx))),
            )
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

    /// The view-control row under the toolbar: grid + snap toggles, the
    /// zoom bar with its `-`/`+` buttons and live readout, and "Reset".
    fn render_view_controls(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_view_controls is only called in the Ready state");
        };
        let grid = open.grid;
        let snap = open.snap;
        let zoom = open.view.borrow().zoom;
        let grid_weak = cx.weak_entity();
        let snap_weak = cx.weak_entity();
        h_flex()
            .gap_1()
            .px_1()
            .pb_1()
            .child(
                Checkbox::new("ggo-world-grid", ToggleState::from(grid))
                    .label("Grid")
                    .on_click(move |toggle, _window, cx| {
                        let on = matches!(toggle, ToggleState::Selected);
                        grid_weak.update(cx, |this, cx| this.set_grid(on, cx)).ok();
                    }),
            )
            .child(
                Checkbox::new("ggo-world-snap", ToggleState::from(snap))
                    .label("Snap")
                    .on_click(move |toggle, _window, cx| {
                        let on = matches!(toggle, ToggleState::Selected);
                        snap_weak
                            .update(cx, |this, cx| {
                                if let ViewerState::Ready(open) = &mut this.state {
                                    open.snap = on;
                                    cx.notify();
                                }
                            })
                            .ok();
                    }),
            )
            .child(
                IconButton::new("ggo-world-zoom-minus", IconName::Dash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(ui::Tooltip::text("Zoom out"))
                    .disabled(zoom <= canvas::ZOOM_MIN)
                    .on_click(cx.listener(|this, _, _, cx| this.step_zoom(-1, cx))),
            )
            .child(
                // The zoom bar: one segment per ladder level, filled up to
                // the current zoom. Click or drag across segments to set the
                // level directly -- per-segment mouse handlers, so no
                // track-bounds math and drag needs no capture (the segment
                // under the cursor hears the move).
                h_flex()
                    .gap_0p5()
                    .children(
                        canvas::ZOOM_LEVELS
                            .iter()
                            .enumerate()
                            .map(|(index, &level)| {
                                let filled = level <= zoom + 1e-9;
                                div()
                                    .id(("ggo-world-zoom-bar", index))
                                    .w(px(6.0))
                                    .h(px(10.0))
                                    .rounded_xs()
                                    .bg(if filled {
                                        cx.theme().colors().text_accent
                                    } else {
                                        cx.theme().colors().element_background
                                    })
                                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| this.set_zoom(level, cx)),
                                    )
                                    .on_mouse_move(cx.listener(
                                        move |this, event: &MouseMoveEvent, _, cx| {
                                            if event.pressed_button == Some(MouseButton::Left) {
                                                this.set_zoom(level, cx);
                                            }
                                        },
                                    ))
                            }),
                    ),
            )
            .child(
                IconButton::new("ggo-world-zoom-plus", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .tooltip(ui::Tooltip::text("Zoom in"))
                    .disabled(zoom >= canvas::ZOOM_MAX)
                    .on_click(cx.listener(|this, _, _, cx| this.step_zoom(1, cx))),
            )
            .child(
                Label::new(format!("{:.0}%", zoom * 100.0))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Button::new("ggo-world-reset-view", "Reset")
                    .tooltip(ui::Tooltip::text("Reset the camera to the active camera"))
                    .on_click(cx.listener(|this, _, _, cx| this.reset_view_impl(cx))),
            )
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
        let grid = open.grid;
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
                    grid,
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
            // "Go to sprite" for a Sprite or MetaSprite whose stem resolves
            // to a real `.spr` -- absent (not disabled) otherwise, since an
            // unresolved stem has nowhere to go.
            let goto = matches!(component.as_str(), META_SPRITE | SPRITE_COMPONENT)
                .then(|| self.goto_sprite_target(entity_ix, component))
                .flatten();
            let mut panel = v_flex().gap_1().child(
                h_flex()
                    .justify_between()
                    .child(Label::new(SharedString::from(component.clone())))
                    .child(
                        h_flex()
                            .gap_1()
                            .children(goto.map(|rel| {
                                IconButton::new("ggo-goto-sprite", IconName::ArrowUpRight)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(ui::Tooltip::text(format!("Open {rel}")))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let _ = this.goto_sprite(rel.clone(), window, cx);
                                    }))
                            }))
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("ggo-remove-{component}")),
                                    IconName::Trash,
                                )
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        // Direct undoable removal (ggo-ide's
                                        // Transform-with-visual confirm modal is
                                        // not ported; undo covers it).
                                        this.apply_op(
                                            WorldOp::RemoveComponent {
                                                entity: entity_ix,
                                                name: name.clone(),
                                            },
                                            cx,
                                        );
                                    },
                                )),
                            ),
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
                            Some(FieldKind::Color565) => {
                                let target = inspector::FieldTarget::EntityField {
                                    entity: entity_ix,
                                    component: component.clone(),
                                    field: field.clone(),
                                };
                                if let Some(editor) = editors.get(&target) {
                                    let color = field_value.as_f64().unwrap_or(0.0) as i64 as u16;
                                    let swatch_target = target.clone();
                                    panel = panel.child(
                                        h_flex()
                                            .gap_1()
                                            .child(Self::field_label(field.as_str()))
                                            .child(Self::editor_input(editor, cx))
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "ggo-color-swatch-{component}-{field}"
                                                    )))
                                                    .debug_selector(|| {
                                                        format!(
                                                            "ggo-color-swatch-{component}-{field}"
                                                        )
                                                    })
                                                    .size_4()
                                                    .flex_none()
                                                    .rounded_xs()
                                                    .border_1()
                                                    .border_color(cx.theme().colors().border)
                                                    .bg(color565_rgba(color))
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.toggle_color_picker(
                                                                swatch_target.clone(),
                                                                window,
                                                                cx,
                                                            )
                                                        },
                                                    )),
                                            ),
                                    );
                                    let open_here = matches!(
                                        &self.state,
                                        ViewerState::Ready(open)
                                            if open.color_picker.as_ref()
                                                .is_some_and(|p| p.target == target)
                                    );
                                    if open_here {
                                        panel = panel.child(self.render_color_picker(window, cx));
                                    }
                                }
                            }
                            Some(FieldKind::Asset(_)) => {
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
                                    if let Some(suggestions) =
                                        self.render_stem_suggestions(&target, editor, cx)
                                    {
                                        panel = panel.child(suggestions);
                                    }
                                }
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
            .child(self.render_view_controls(cx))
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
        self.refresh_stem_completion(window, cx);
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
            .on_action(cx.listener(|this, _: &ResetView, _window, cx| this.reset_view_impl(cx)))
            .on_action(cx.listener(|this, _: &NudgeLeft, _window, cx| {
                this.nudge_impl("ArrowLeft", false, cx)
            }))
            .on_action(cx.listener(|this, _: &NudgeRight, _window, cx| {
                this.nudge_impl("ArrowRight", false, cx)
            }))
            .on_action(
                cx.listener(|this, _: &NudgeUp, _window, cx| this.nudge_impl("ArrowUp", false, cx)),
            )
            .on_action(cx.listener(|this, _: &NudgeDown, _window, cx| {
                this.nudge_impl("ArrowDown", false, cx)
            }))
            .on_action(cx.listener(|this, _: &NudgeLeftTile, _window, cx| {
                this.nudge_impl("ArrowLeft", true, cx)
            }))
            .on_action(cx.listener(|this, _: &NudgeRightTile, _window, cx| {
                this.nudge_impl("ArrowRight", true, cx)
            }))
            .on_action(cx.listener(|this, _: &NudgeUpTile, _window, cx| {
                this.nudge_impl("ArrowUp", true, cx)
            }))
            .on_action(cx.listener(|this, _: &NudgeDownTile, _window, cx| {
                this.nudge_impl("ArrowDown", true, cx)
            }))
            .on_action(cx.listener(Self::on_commit_field))
            .on_action(cx.listener(|this, _: &FocusNextField, window, cx| {
                this.focus_field_sibling(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPrevField, window, cx| {
                this.focus_field_sibling(-1, window, cx)
            }))
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

    /// Fixture note: the world is built from Text/Camera entities
    /// plus a nested `[[instance]]` -- deliberately NO image assets
    /// (authoring a valid `.spr`/`.til`/`.pal`/`.map` set through
    /// `sprites::io` save fns would dwarf the test; the brief allows this
    /// choice). Real-image composition is worldlib-tested; the BGRA bridge
    /// has its own unit test in `canvas`.
    fn write_fixture(root: &std::path::Path) {
        let sub = WorldFile {
            entities: vec![entity(json!({
                "Transform": { "pos": [0.0, 0.0], "z": 1.0 },
                "Text": { "content": "a" }
            }))],
            instances: vec![],
            backgrounds: vec![],
        };
        write_world(root, "worlds/sub.toml", &sub).unwrap();

        let main = WorldFile {
            entities: vec![
                entity(json!({
                    "Transform": { "pos": [4.0, 4.0], "z": 0.0 },
                    "Text": { "content": "ab", "max_width": 16.0, "max_height": 12.0 }
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
    /// both the top-level and the instance-resolved Text entities.
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

            let texts = items
                .iter()
                .filter(|i| matches!(i.kind, DrawKind::Text { .. }))
                .count();
            assert_eq!(
                texts, 3,
                "two top-level Texts + one from the resolved worlds/sub instance"
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
            let texts = draw_items(open)
                .iter()
                .filter(|i| matches!(i.kind, DrawKind::Text { .. }))
                .count();
            assert_eq!(
                texts, 3,
                "top-level Texts + the worlds/sub instance's, which only \
                 resolves if the derived root reached instance resolution"
            );
            assert_eq!(
                panel.instance_candidates(),
                vec!["worlds/sub".to_string()],
                "+ Instance must enumerate from the SAME derived root"
            );
        });
    }

    /// Click select: a primary-down over the Text box (world [4,4]..[20,12]
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
    /// `worlds/sub` carries a Text child, so the draw list's text
    /// count must grow right after the add.
    #[gpui::test]
    async fn test_add_instance_resolves_subtree_without_reload(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let text_count = |open: &OpenWorld| {
                draw_items(open)
                    .iter()
                    .filter(|i| matches!(i.kind, DrawKind::Text { .. }))
                    .count()
            };
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                assert_eq!(
                    text_count(open),
                    3,
                    "fixture baseline: top-level Texts + the existing sub instance's"
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
                text_count(open),
                4,
                "the new instance's Text child renders without a reload"
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

    /// Load `worlds/test` into a panel that is the root view of a real
    /// test window, with Entity(0) selected so the inspector editors
    /// exist. Rendering in a window is what drives gpui's draw/focus
    /// cycle, which the blur-commit ordering tests below depend on.
    async fn ready_panel_in_window<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (gpui::Entity<WorldPanel>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        write_fixture(root);
        let root = root.to_path_buf();
        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = WorldPanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        // Focus in/out events only fire while the window is ACTIVE -- an
        // inactive window's focus paths are blanked in the draw's focus
        // phase, and blur-commit never runs.
        cx.update(|window, _| window.activate_window());
        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            panel.load_rel_path("worlds/test.toml", cx);
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready state after load");
            };
            open.view.borrow_mut().pan = Some([0.0, 0.0]);
            open.selected = Some(Selection::Entity(0));
            cx.notify();
        });
        cx.run_until_parked();
        (panel, cx)
    }

    fn field_editor(
        panel: &gpui::Entity<WorldPanel>,
        cx: &mut gpui::VisualTestContext,
        component: &str,
        field: &str,
    ) -> gpui::Entity<Editor> {
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            open.inspector
                .iter()
                .find(|e| {
                    matches!(
                        &e.target,
                        inspector::FieldTarget::EntityField { component: c, field: f, .. }
                            if c == component && f == field
                    )
                })
                .unwrap_or_else(|| panic!("no editor for {component}.{field}"))
                .editor
                .clone()
        })
    }

    /// Give entity 0 an `Fx` component with a Color565 `color` field (no
    /// builtin component carries one), so picker tests have a target.
    fn add_fx_color(panel: &gpui::Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.schemas.push(ComponentSchema {
                name: "Fx".to_string(),
                fields: vec![ggo_worldlib::schemas::SchemaField {
                    name: "color".to_string(),
                    kind: ggo_worldlib::schemas::FieldKind::Color565,
                    def: None,
                }],
            });
            open.store.apply(WorldOp::AddComponent {
                entity: 0,
                name: "Fx".to_string(),
                defaults: serde_json::json!({"color": 0})
                    .as_object()
                    .expect("object literal")
                    .clone(),
            });
            cx.notify();
        });
        cx.run_until_parked();
    }

    /// The full stem-completion flow: focusing `Sprite.stem` scans the
    /// project's `.spr` stems -- recursively, so a fresh project's
    /// `sprites/gg_icon` surfaces -- other extensions stay out, blurring
    /// clears the feed, and picking a suggestion commits the stem through
    /// the normal field-commit path (undoable like a typed edit).
    #[gpui::test]
    async fn test_stem_completion_offers_and_commits_project_sprites(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sprites")).unwrap();
        std::fs::write(dir.path().join("sprites/gg_icon.spr"), "").unwrap();
        std::fs::write(dir.path().join("hero.spr"), "").unwrap();
        std::fs::write(dir.path().join("sprites/level.map"), "").unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.store.apply(WorldOp::AddComponent {
                entity: 0,
                name: "Sprite".to_string(),
                defaults: serde_json::json!({"stem": ""})
                    .as_object()
                    .expect("object literal")
                    .clone(),
            });
            cx.notify();
        });
        cx.run_until_parked();

        let editor = field_editor(&panel, cx, "Sprite", "stem");
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
            panel.refresh_stem_completion(window, cx);
        });
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let completion = open
                .stem_completion
                .as_ref()
                .expect("a focused Sprite.stem field completes");
            assert_eq!(
                completion.stems,
                vec!["hero".to_string(), "sprites/gg_icon".to_string()],
                "every .spr stem, sorted and extensionless; the .map stays out"
            );
        });

        let target = inspector::FieldTarget::EntityField {
            entity: 0,
            component: "Sprite".to_string(),
            field: "stem".to_string(),
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.pick_stem(target, "sprites/gg_icon".to_string(), window, cx);
        });
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Sprite"]["stem"],
                json!("sprites/gg_icon"),
                "a picked suggestion commits into the store"
            );
            panel.undo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Sprite"]["stem"],
                json!(""),
                "and it is a normal undoable edit"
            );
        });

        // Focus leaving the field clears the feed.
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
            panel.refresh_stem_completion(window, cx);
        });
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                open.stem_completion.is_none(),
                "no focused asset field, no feed"
            );
        });
    }

    /// The full color-picker flow against a real `.pal` on disk: open the
    /// picker on the Fx test component's Color565 field (the project's
    /// one `.pal` preselects),
    /// take slot 2's color, then write new channels into slot 3 -- which
    /// must land in the `.pal` file AND the field. Slot 0 stays locked.
    #[gpui::test]
    async fn test_color_picker_selects_and_writes_pal_slots(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut pal = [0u16; ggo_asset_formats::PAL_ENTRIES];
        pal[2] = 0x07e0;
        std::fs::create_dir_all(dir.path().join("art")).unwrap();
        std::fs::write(
            dir.path().join("art/main.pal"),
            ggo_asset_formats::encode_pal(&pal),
        )
        .unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        add_fx_color(&panel, cx);

        let target = inspector::FieldTarget::EntityField {
            entity: 0,
            component: "Fx".to_string(),
            field: "color".to_string(),
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_color_picker(target.clone(), window, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker open");
            assert_eq!(picker.candidates, ["art/main.pal"]);
            assert_eq!(picker.pal_rel.as_deref(), Some("art/main.pal"));
            assert!(picker.palette.is_some(), "the only .pal preselects");
        });

        // Take an existing slot's color.
        panel.update_in(cx, |panel, window, cx| {
            panel.picker_select_slot(2, window, cx)
        });
        panel.read_with(cx, |panel, cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Fx"]["color"],
                json!(0x07e0),
                "slot click writes the slot's color into the field"
            );
            let picker = open.color_picker.as_ref().expect("picker open");
            assert_eq!(
                picker.g_editor.read(cx).text(cx),
                "63",
                "channel inputs prefill from the slot (0x07e0 = pure green)"
            );
        });

        // Write a new color into slot 3.
        panel.update_in(cx, |panel, window, cx| {
            panel.picker_select_slot(3, window, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker open");
            for (editor, text) in [
                (picker.r_editor.clone(), "31"),
                (picker.g_editor.clone(), "0"),
                (picker.b_editor.clone(), "31"),
            ] {
                editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
            panel.picker_set_slot_color(cx);
        });
        let saved = std::fs::read(dir.path().join("art/main.pal")).unwrap();
        let saved = ggo_asset_formats::decode_pal(&saved).unwrap();
        assert_eq!(saved[3], 0xf81f, "magenta landed in the .pal on disk");
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Fx"]["color"],
                json!(0xf81f),
                "and became the field value"
            );
            assert!(
                open.color_picker
                    .as_ref()
                    .is_some_and(|p| p.error.is_none()),
                "no error on a successful write"
            );
        });

        // Slot 0 is the locked transparent slot: refused, file untouched.
        panel.update_in(cx, |panel, window, cx| {
            panel.picker_select_slot(0, window, cx);
            panel.picker_set_slot_color(cx);
        });
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker open");
            assert!(
                picker
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("slot 0")),
                "writing slot 0 reports the lock"
            );
        });
        let saved = std::fs::read(dir.path().join("art/main.pal")).unwrap();
        let saved = ggo_asset_formats::decode_pal(&saved).unwrap();
        assert_eq!(saved[0], 0, "slot 0 untouched on disk");
    }

    /// A project with no `.pal` anywhere: opening the picker seeds the
    /// default palette at the asset root as `main.pal` (ggo-ide's sprite
    /// default black->white ramp) and preselects it.
    #[gpui::test]
    async fn test_picker_with_no_palettes_creates_a_default_main_pal(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        add_fx_color(&panel, cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_color_picker(
                inspector::FieldTarget::EntityField {
                    entity: 0,
                    component: "Fx".to_string(),
                    field: "color".to_string(),
                },
                window,
                cx,
            )
        });

        let expected = ggo_worldlib::sprites::sprite_doc::default_palette();
        let on_disk = ggo_asset_formats::decode_pal(
            &std::fs::read(dir.path().join("main.pal")).expect("main.pal was created"),
        )
        .expect("a valid .pal");
        assert_eq!(on_disk, expected, "seeded with the default ramp");

        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker open");
            assert_eq!(picker.candidates, ["main.pal"]);
            assert_eq!(picker.pal_rel.as_deref(), Some("main.pal"));
            assert_eq!(picker.palette, Some(expected), "and preselected");
        });
    }

    /// A real CLICK on the rendered swatch (not a direct method call) must
    /// open the picker -- covers the element wiring end to end.
    #[gpui::test]
    async fn test_clicking_the_color_swatch_opens_the_picker(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        add_fx_color(&panel, cx);

        let bounds = cx
            .debug_bounds("ggo-color-swatch-Fx-color")
            .expect("the color swatch is rendered for the selected entity's Fx color");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker opened by click");
            assert_eq!(
                picker.target,
                inspector::FieldTarget::EntityField {
                    entity: 0,
                    component: "Fx".to_string(),
                    field: "color".to_string(),
                }
            );
        });
    }

    /// Several `.pal` files: all are listed (sorted), switching palettes
    /// swaps the swatch source and resets the slot, and a slot edit lands
    /// in the SELECTED palette's file only.
    #[gpui::test]
    async fn test_color_picker_lists_and_switches_between_palettes(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut pal_a = [0u16; ggo_asset_formats::PAL_ENTRIES];
        pal_a[1] = 0x001f;
        let pal_b = [0x0777u16; ggo_asset_formats::PAL_ENTRIES];
        std::fs::create_dir_all(dir.path().join("art")).unwrap();
        std::fs::write(
            dir.path().join("art/a.pal"),
            ggo_asset_formats::encode_pal(&pal_a),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("art/b.pal"),
            ggo_asset_formats::encode_pal(&pal_b),
        )
        .unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        add_fx_color(&panel, cx);

        let target = inspector::FieldTarget::EntityField {
            entity: 0,
            component: "Fx".to_string(),
            field: "color".to_string(),
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_color_picker(target.clone(), window, cx);
            panel.picker_select_slot(1, window, cx);
        });
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker open");
            assert_eq!(
                picker.candidates,
                ["art/a.pal", "art/b.pal"],
                "all pals listed"
            );
            assert_eq!(
                picker.pal_rel.as_deref(),
                Some("art/a.pal"),
                "first preselects"
            );
            assert_eq!(picker.slot, Some(1));
            assert_eq!(
                open.store.state().entities[0].components["Fx"]["color"],
                json!(0x001f),
                "slot color comes from palette A"
            );
        });

        panel.update(cx, |panel, cx| {
            panel.picker_select_pal("art/b.pal".to_string(), cx)
        });
        panel.update_in(cx, |panel, window, cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker open");
            assert_eq!(picker.pal_rel.as_deref(), Some("art/b.pal"));
            assert_eq!(picker.slot, None, "slot resets on palette switch");
            assert_eq!(picker.palette.map(|p| p[1]), Some(0x0777));

            // Edit slot 5 of palette B.
            panel.picker_select_slot(5, window, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let picker = open.color_picker.as_ref().expect("picker open");
            for (editor, text) in [
                (picker.r_editor.clone(), "10"),
                (picker.g_editor.clone(), "20"),
                (picker.b_editor.clone(), "30"),
            ] {
                editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
            panel.picker_set_slot_color(cx);
        });

        let expected = (10u16 << 11) | (20 << 5) | 30;
        let b =
            ggo_asset_formats::decode_pal(&std::fs::read(dir.path().join("art/b.pal")).unwrap())
                .unwrap();
        assert_eq!(b[5], expected, "edit landed in palette B slot 5");
        let a =
            ggo_asset_formats::decode_pal(&std::fs::read(dir.path().join("art/a.pal")).unwrap())
                .unwrap();
        assert_eq!(a[5], 0, "palette A untouched");
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Fx"]["color"],
                json!(expected),
                "edited color became the field value"
            );
        });
    }

    /// Typing into a field and then clicking (focusing) another field must
    /// commit the typed buffer -- the draw that follows the focus change
    /// runs before the old editor's Blurred event is delivered, and must
    /// not clobber the in-progress buffer with the stale doc value.
    #[gpui::test]
    async fn test_focus_move_to_another_field_commits_the_buffer(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        let w_editor = field_editor(&panel, cx, "Text", "max_width");
        let h_editor = field_editor(&panel, cx, "Text", "max_height");

        w_editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        w_editor.update_in(cx, |editor, window, cx| editor.set_text("12", window, cx));
        cx.run_until_parked();

        h_editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Text"]["max_width"],
                json!(12),
                "the typed value must be committed when focus moves away"
            );
        });
        assert_eq!(
            w_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "12",
            "the field keeps showing the committed value"
        );
    }

    /// An unparsable buffer is dropped on blur (ggo-ide's rule) and the
    /// field reverts to the doc value instead of showing the stale buffer.
    #[gpui::test]
    async fn test_unparsable_buffer_reverts_to_doc_value_on_blur(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        let w_editor = field_editor(&panel, cx, "Text", "max_width");
        let h_editor = field_editor(&panel, cx, "Text", "max_height");

        w_editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        w_editor.update_in(cx, |editor, window, cx| editor.set_text("abc", window, cx));
        h_editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Text"]["max_width"],
                json!(16),
                "an unparsable buffer must not change the doc"
            );
        });
        assert_eq!(
            w_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "16",
            "the field reverts to the doc value"
        );
    }

    /// Tab commits the focused field (via the blur it causes) and moves
    /// focus to the next field editor; shift-tab moves back.
    #[gpui::test]
    async fn test_tab_moves_to_next_field_and_commits(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        let w_editor = field_editor(&panel, cx, "Text", "max_width");

        // The editor following `w` in the inspector's own (rendered) order,
        // whatever that order is.
        let next_editor = panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let w_index = open
                .inspector
                .iter()
                .position(|e| e.editor == w_editor)
                .expect("w editor is in the inspector");
            open.inspector[(w_index + 1) % open.inspector.len()]
                .editor
                .clone()
        });

        w_editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        w_editor.update_in(cx, |editor, window, cx| editor.set_text("12", window, cx));

        cx.simulate_keystrokes("tab");
        cx.run_until_parked();

        assert!(
            next_editor.update_in(cx, |editor, window, cx| editor
                .focus_handle(cx)
                .is_focused(window)),
            "tab moves focus to the next field"
        );
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Text"]["max_width"],
                json!(12),
                "tabbing away commits the typed value"
            );
        });

        cx.simulate_keystrokes("shift-tab");
        cx.run_until_parked();
        assert!(
            w_editor.update_in(cx, |editor, window, cx| editor
                .focus_handle(cx)
                .is_focused(window)),
            "shift-tab moves focus back to the previous field"
        );
    }

    // ------------------------------------------- unsaved-document guard

    // ------------------------------------------------- view controls

    fn open_of(panel: &WorldPanel) -> &OpenWorld {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
    }

    /// Grid toggle and "Reset" -- the view controls carried over from
    /// ggo-ide, at panel level (the pure math each one leans on is tested
    /// in `canvas`).
    #[gpui::test]
    async fn test_view_controls_grid_and_reset(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            assert!(open_of(panel).grid, "grid defaults on, as in ggo-ide");

            panel.set_grid(false, cx);
            assert!(!open_of(panel).grid);
            panel.set_grid(true, cx);
            assert!(open_of(panel).grid);

            // Reset: a camera moved by wheel-zoom and pan goes back to the
            // default zoom and to "not laid out", which is what re-runs the
            // initial centering on the next paint.
            {
                let mut v = open_of(panel).view.borrow_mut();
                v.zoom = 4.0;
                v.pan = Some([123.0, -45.0]);
            }
            panel.reset_view_impl(cx);
            let v = open_of(panel).view.borrow();
            assert_eq!(v.zoom, canvas::ZOOM_DEFAULT);
            assert_eq!(v.pan, None, "reset re-arms the initial centering");
        });
    }

    /// `set_zoom` (the zoom bar and `-`/`+` buttons) re-anchors the pan so
    /// the world point at the CANVAS CENTER stays put, and falls back to a
    /// plain zoom assignment before the first layout (no bounds yet).
    #[gpui::test]
    async fn test_set_zoom_anchors_at_the_canvas_center(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            {
                let mut v = open_of(panel).view.borrow_mut();
                v.zoom = 1.0;
                v.pan = Some([10.0, 20.0]);
                v.last_bounds = Some(gpui::bounds(
                    gpui::point(px(0.0), px(0.0)),
                    gpui::size(px(200.0), px(100.0)),
                ));
            }
            panel.set_zoom(2.0, cx);
            {
                let v = open_of(panel).view.borrow();
                assert_eq!(v.zoom, 2.0);
                assert_eq!(
                    v.pan,
                    Some(canvas::zoom_at([10.0, 20.0], 1.0, [100.0, 50.0], 2.0))
                );
            }

            // Out-of-range requests clamp to the ladder ends.
            panel.set_zoom(100.0, cx);
            assert_eq!(open_of(panel).view.borrow().zoom, canvas::ZOOM_MAX);

            // Before the first layout there is nothing to anchor: zoom is
            // set, pan stays None so the initial centering still runs.
            {
                let mut v = open_of(panel).view.borrow_mut();
                v.pan = None;
                v.last_bounds = None;
            }
            panel.set_zoom(0.5, cx);
            {
                let v = open_of(panel).view.borrow();
                assert_eq!(v.zoom, 0.5);
                assert_eq!(v.pan, None);
            }

            // The `-`/`+` buttons walk the same ladder as the wheel.
            panel.step_zoom(1, cx);
            assert_eq!(open_of(panel).view.borrow().zoom, 1.0);
            panel.step_zoom(-1, cx);
            assert_eq!(open_of(panel).view.borrow().zoom, 0.5);
        });
    }

    /// With NOTHING selected, arrows fall through to panning the camera:
    /// arrow right looks right (content slides left, pan.x decreases),
    /// shift pans by the larger step. Before first layout (pan `None`)
    /// there is no camera to move, and with a selection arrows still
    /// nudge -- pan untouched.
    #[gpui::test]
    async fn test_arrow_keys_pan_the_camera_when_nothing_is_selected(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = None,
                _ => panic!("expected Ready"),
            }
            open_of(panel).view.borrow_mut().pan = Some([0.0, 0.0]);

            panel.nudge_impl("ArrowRight", false, cx);
            assert_eq!(open_of(panel).view.borrow().pan, Some([-32.0, 0.0]));
            panel.nudge_impl("ArrowDown", true, cx);
            assert_eq!(open_of(panel).view.borrow().pan, Some([-32.0, -128.0]));
            panel.nudge_impl("ArrowLeft", false, cx);
            panel.nudge_impl("ArrowUp", false, cx);
            assert_eq!(open_of(panel).view.borrow().pan, Some([0.0, -96.0]));

            // Pre-layout: no pan yet, nothing to move.
            open_of(panel).view.borrow_mut().pan = None;
            panel.nudge_impl("ArrowRight", false, cx);
            assert_eq!(open_of(panel).view.borrow().pan, None);

            // Selected: the nudge path runs instead, camera stays put.
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = Some(Selection::Entity(0)),
                _ => panic!("expected Ready"),
            }
            open_of(panel).view.borrow_mut().pan = Some([5.0, 6.0]);
            let start = entity_pos_of(panel, 0);
            panel.nudge_impl("ArrowRight", false, cx);
            assert_eq!(open_of(panel).view.borrow().pan, Some([5.0, 6.0]));
            assert_eq!(entity_pos_of(panel, 0), [start[0] + 1.0, start[1]]);
        });
    }

    // ------------------------------------------------- arrow-key nudge

    fn entity_pos_of(panel: &WorldPanel, index: usize) -> [f64; 2] {
        inspector::entity_pos(&open_of(panel).store.state(), index).expect("entity has a Transform")
    }

    /// Nudge moves the selection through the drag's own `WorldOp` path (1px,
    /// one tile with Shift), and a RUN of nudges collapses into a single
    /// undo entry -- asserted by depth, not just by position: after one
    /// undo the stack is empty, so there was exactly one entry for the
    /// whole run.
    #[gpui::test]
    async fn test_arrow_nudge_moves_the_selection_and_coalesces_one_undo_entry(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let start = entity_pos_of(panel, 0);
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = Some(Selection::Entity(0)),
                _ => panic!("expected Ready"),
            }

            // Three 1px steps right, one 1px up.
            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowUp", false, cx);
            assert_eq!(
                entity_pos_of(panel, 0),
                [start[0] + 3.0, start[1] - 1.0],
                "one pixel per press, up is -y"
            );

            // Shift is a whole tile, and stays inside the same run.
            panel.nudge_impl("ArrowDown", true, cx);
            assert_eq!(
                entity_pos_of(panel, 0),
                [start[0] + 3.0, start[1] - 1.0 + 16.0],
                "Shift moves by one tile"
            );

            // ONE undo takes the whole run back...
            panel.undo_impl(cx);
            assert_eq!(entity_pos_of(panel, 0), start);
            // ...and there is nothing left underneath it: five keypresses
            // produced exactly one undo entry.
            match &mut panel.state {
                ViewerState::Ready(open) => assert!(
                    !open.store.undo(),
                    "the run must be ONE undo entry, not one per keypress"
                ),
                _ => panic!("expected Ready"),
            }
        });
    }

    /// The run has to END somewhere, or every nudge for the rest of the
    /// session would amend one entry. A canvas click (i.e. a new selection
    /// or a drag) seals it, so the next run is its own undo entry.
    #[gpui::test]
    async fn test_a_click_seals_the_nudge_run_so_the_next_one_undoes_separately(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let start = entity_pos_of(panel, 0);
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = Some(Selection::Entity(0)),
                _ => panic!("expected Ready"),
            }
            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowRight", false, cx);
            let after_first_run = entity_pos_of(panel, 0);

            // A canvas click on empty space: deselects, and ends the run.
            panel.canvas_primary_down([-9999.0, -9999.0], cx);
            assert!(open_of(panel).nudge_gesture.is_none());
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = Some(Selection::Entity(0)),
                _ => panic!("expected Ready"),
            }

            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowRight", false, cx);
            assert_eq!(entity_pos_of(panel, 0), [start[0] + 4.0, start[1]]);

            panel.undo_impl(cx);
            assert_eq!(
                entity_pos_of(panel, 0),
                after_first_run,
                "the second run undoes on its own"
            );
            panel.undo_impl(cx);
            assert_eq!(entity_pos_of(panel, 0), start);
        });
    }

    /// Nothing selected, or a selection with nothing to move, must be a
    /// no-op -- not a panic and not a spurious undo entry.
    #[gpui::test]
    async fn test_nudge_without_a_movable_selection_does_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            assert!(open_of(panel).selected.is_none());
            panel.nudge_impl("ArrowRight", false, cx);
            assert!(!open_of(panel).store.state().dirty, "no selection, no op");

            // A stale selection index (undo/redo restructure) is guarded the
            // same way `delete_selected_impl` guards its own.
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = Some(Selection::Entity(999)),
                _ => panic!("expected Ready"),
            }
            panel.nudge_impl("ArrowRight", false, cx);
            assert!(!open_of(panel).store.state().dirty);

            // A key that isn't an arrow resolves to no delta.
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = Some(Selection::Entity(0)),
                _ => panic!("expected Ready"),
            }
            panel.nudge_impl("Home", false, cx);
            assert!(!open_of(panel).store.state().dirty);
        });
    }

    /// An instance nudges through `MoveInstance`, the same op its drag
    /// applies, and coalesces the same way.
    #[gpui::test]
    async fn test_nudge_moves_an_instance_through_the_same_op(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let start = open_of(panel).store.state().instances[0].pos;
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = Some(Selection::Instance(0)),
                _ => panic!("expected Ready"),
            }
            panel.nudge_impl("ArrowLeft", false, cx);
            panel.nudge_impl("ArrowLeft", true, cx);
            assert_eq!(
                open_of(panel).store.state().instances[0].pos,
                [start[0] - 17.0, start[1]]
            );
            panel.undo_impl(cx);
            assert_eq!(open_of(panel).store.state().instances[0].pos, start);
        });
    }

    /// Snap applies to the nudged RESULT, exactly as it does for a drag
    /// (ggo-ide's rule).
    #[gpui::test]
    async fn test_nudge_snaps_the_result_when_snap_is_on(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            match &mut panel.state {
                ViewerState::Ready(open) => {
                    open.selected = Some(Selection::Entity(0));
                    open.snap = true;
                }
                _ => panic!("expected Ready"),
            }
            // The fixture's entity 0 sits at [4, 4]: a 1px step right lands
            // on [5, 4], which snaps back to the nearest tile line, [0, 0].
            panel.nudge_impl("ArrowRight", false, cx);
            assert_eq!(entity_pos_of(panel, 0), [0.0, 0.0]);
        });
    }

    // ------------------------------------------------- goto sprite

    /// The `MetaSprite` -> sprite-panel jump resolves the component's
    /// ASSET-ROOT-relative stem into the WORKTREE-relative path the sprite
    /// panel opens, and declines when the stem names no file.
    #[gpui::test]
    async fn test_goto_sprite_target_resolves_the_stem_and_declines_when_missing(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // No MetaSprite on the fixture's entity 0 yet.
            assert_eq!(panel.goto_sprite_target(0, META_SPRITE), None);

            let mut defaults = serde_json::Map::new();
            defaults.insert("stem".to_string(), json!("sprites/hero"));
            panel.apply_op(
                WorldOp::AddComponent {
                    entity: 0,
                    name: META_SPRITE.to_string(),
                    defaults,
                },
                cx,
            );
            // The stem is there, but nothing is on disk: decline rather
            // than park the sprite panel in an error state.
            assert_eq!(
                panel.goto_sprite_target(0, META_SPRITE),
                None,
                "an unauthored sprite offers no jump"
            );
        });

        std::fs::create_dir_all(dir.path().join("sprites")).unwrap();
        std::fs::write(dir.path().join("sprites/hero.spr"), b"not really a sprite").unwrap();

        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.goto_sprite_target(0, META_SPRITE),
                Some("sprites/hero.spr".to_string()),
                "worktree-relative, with the .spr extension the stem omits"
            );

            // The jump reads the NAMED component -- a `Sprite` resolves its
            // own stem, not the MetaSprite's.
            let mut defaults = serde_json::Map::new();
            defaults.insert("stem".to_string(), json!("sprites/hero"));
            panel.apply_op(
                WorldOp::AddComponent {
                    entity: 0,
                    name: SPRITE_COMPONENT.to_string(),
                    defaults,
                },
                cx,
            );
            assert_eq!(
                panel.goto_sprite_target(0, SPRITE_COMPONENT),
                Some("sprites/hero.spr".to_string()),
                "a static Sprite gets the jump too"
            );

            // A blank stem, and a stem that walks nowhere, both decline.
            panel.apply_op(
                WorldOp::SetField {
                    entity: 0,
                    component: META_SPRITE.to_string(),
                    field: "stem".to_string(),
                    value: json!(""),
                },
                cx,
            );
            assert_eq!(panel.goto_sprite_target(0, META_SPRITE), None);
            panel.apply_op(
                WorldOp::SetField {
                    entity: 0,
                    component: META_SPRITE.to_string(),
                    field: "stem".to_string(),
                    value: json!("sprites/nobody"),
                },
                cx,
            );
            assert_eq!(panel.goto_sprite_target(0, META_SPRITE), None);
        });
    }

    /// `sprite_rel_for_stem` is the asset-root/worktree bridge; the panel
    /// test above exercises it through a worktree-rooted world, this pins
    /// the ASSET-ROOTED layout (`<worktree>/assets/...`) the F4 split
    /// introduced, where the two frames genuinely differ.
    #[gpui::test]
    fn test_sprite_rel_for_stem_bridges_the_asset_root_and_the_worktree(_cx: &mut gpui::App) {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path();
        let assets = worktree.join("assets");
        std::fs::create_dir_all(assets.join("sprites")).unwrap();
        std::fs::write(assets.join("sprites/hero.spr"), b"x").unwrap();

        assert_eq!(
            sprite_rel_for_stem(worktree, &assets, "sprites/hero"),
            Some("assets/sprites/hero.spr".to_string()),
            "the stem resolves under the ASSET root but is handed over worktree-relative"
        );
        assert_eq!(
            sprite_rel_for_stem(worktree, &assets, "sprites/ghost"),
            None
        );
        assert_eq!(sprite_rel_for_stem(worktree, &assets, ""), None);
        // An asset root outside the worktree has nothing to relativize.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(elsewhere.path().join("sprites")).unwrap();
        std::fs::write(elsewhere.path().join("sprites/hero.spr"), b"x").unwrap();
        assert_eq!(
            sprite_rel_for_stem(worktree, elsewhere.path(), "sprites/hero"),
            None
        );
    }

    /// The jump itself, on a real workspace: with a sprite panel docked
    /// the handoff is claimed and the sprite panel is revealed; with none
    /// docked it declines instead of swallowing the click.
    #[gpui::test]
    async fn test_goto_sprite_hands_the_path_to_a_docked_sprite_panel(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "worlds/test.toml");
        std::fs::create_dir_all(dir.path().join("sprites")).unwrap();
        std::fs::write(dir.path().join("sprites/hero.spr"), b"x").unwrap();

        let rel = panel.update(cx, |panel, cx| {
            let mut defaults = serde_json::Map::new();
            defaults.insert("stem".to_string(), json!("sprites/hero"));
            panel.apply_op(
                WorldOp::AddComponent {
                    entity: 0,
                    name: META_SPRITE.to_string(),
                    defaults,
                },
                cx,
            );
            panel.goto_sprite_target(0, META_SPRITE).expect("the stem resolves")
        });
        assert_eq!(rel, "sprites/hero.spr");

        // No sprite panel in this workspace yet: decline, don't panic.
        let claimed = panel.update_in(cx, |panel, window, cx| {
            panel.goto_sprite(rel.clone(), window, cx)
        });
        assert!(!claimed, "no sprite panel docked => not claimed");
        assert!(
            !workspace.read_with(cx, |workspace, cx| workspace
                .panel::<ggo_sprite_panel::SpritePanel>(cx)
                .is_some()),
            "precondition: the sprite panel really was absent"
        );

        // Dock one (its own `init` is what does that in production) and the
        // same jump is claimed.
        cx.update(|_window, cx| ggo_sprite_panel::init(cx));
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let sprite = cx.new(|cx| {
                    ggo_sprite_panel::SpritePanel::new(Some(workspace.weak_handle()), cx)
                });
                workspace.add_panel(sprite, window, cx);
            });
        });
        let claimed = panel.update_in(cx, |panel, window, cx| panel.goto_sprite(rel, window, cx));
        cx.run_until_parked();
        assert!(claimed, "a docked sprite panel claims the jump");
    }

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

    /// `save_if_open_and_dirty` is the "save first" half of the emu
    /// panel's "Emulate this world" (S4). It writes ONLY when the world it
    /// is asked about is the open, dirty one -- a build must never boot a
    /// stale file, and must never write one the user did not name.
    #[gpui::test]
    async fn test_save_if_open_and_dirty_writes_only_the_named_dirty_world(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let path = dir.path().join("worlds/test.toml");
        let before = std::fs::read_to_string(&path).unwrap();

        // Clean: nothing to do, and nothing written.
        panel.update(cx, |panel, cx| {
            assert!(panel.save_if_open_and_dirty("worlds/test.toml", cx));
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [50.0, 60.0],
                    gesture: None,
                },
                cx,
            );
            assert!(panel.dirty_world_name().is_some());
            // A DIFFERENT world: not ours to write, even though we are dirty.
            assert!(panel.save_if_open_and_dirty("worlds/sub.toml", cx));
        });
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "another world's build must not write this one's document"
        );

        panel.update(cx, |panel, cx| {
            assert!(panel.save_if_open_and_dirty("worlds/test.toml", cx));
            assert!(
                panel.dirty_world_name().is_none(),
                "the write clears dirty, so a second build is a no-op"
            );
        });
        assert_ne!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "the edit the user made must be on disk before the build reads it"
        );
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
                "hero.til": "",
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

    thread_local! {
        static EMULATED: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn recording_emulator(
        _workspace: &mut Workspace,
        rel: &str,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        EMULATED.with(|e| e.borrow_mut().push(rel.to_string()));
        true
    }

    /// The toolbar's Emulate button hands the OPEN world's worktree-rel
    /// path to `ggo_common`'s emulator registry (the emu panel's `init`
    /// registers the real handler there; this test registers a recorder).
    #[gpui::test]
    async fn test_emulate_button_routes_the_open_world_to_the_registry(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        cx.update(|cx| ggo_common::register_world_emulator(cx, recording_emulator));
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldPanel>(cx)
                .expect("init() adds the panel")
        });
        panel.update(cx, |panel, _| {
            panel.root_override = Some(dir.path().to_path_buf())
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("worlds/test.toml", window, cx)
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| panel.emulate_impl(window, cx));
        cx.run_until_parked();

        EMULATED.with(|e| {
            assert_eq!(
                e.borrow().as_slice(),
                ["worlds/test.toml"],
                "the open world's rel path reaches the registered emulator"
            );
        });
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

    // ----------------------------------------- context-menu file ops (G2)

    /// A workspace with `init()` run -- so the REAL contributor is in the
    /// registry -- and the panel pointed at the real-fs `root` fixture.
    /// Same division of labour as [`routed_project`]: the fake-fs worktree
    /// supplies a worktree id and rel paths for the menu's predicate, while
    /// the panel does its actual file work under `root`.
    async fn menu_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        Entity<Workspace>,
        Entity<WorldPanel>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        let project = routed_project(cx, root, true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldPanel>(cx)
                .expect("init() adds the panel")
        });
        let root = root.to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));
        (workspace, panel, worktree_id, cx)
    }

    /// Load `rel` into the workspace's own panel and leave it Ready.
    fn open_in_menu_panel(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext, rel: &str) {
        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            panel.load_rel_path(rel, cx);
        });
        cx.run_until_parked();
    }

    /// The menu entry is offered for a world file and for NOTHING else --
    /// the same `**/worlds/**/*.toml` rule the open interceptor uses, so a
    /// root `Cargo.toml`, a `.til`, and the `worlds` DIRECTORY itself all
    /// leave upstream's menu exactly as it was.
    #[gpui::test]
    async fn test_context_menu_offers_delete_world_only_for_world_files(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, worktree_id, cx) = menu_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };

        assert_eq!(
            contributed("worlds/test.toml", false, cx),
            1,
            "a world file must get its Delete World entry"
        );
        assert_eq!(
            contributed("Cargo.toml", false, cx),
            0,
            "a .toml outside worlds/ is not a world"
        );
        assert_eq!(
            contributed("hero.til", false, cx),
            0,
            "another panel's file type is not a world"
        );
        assert_eq!(
            contributed("worlds", true, cx),
            0,
            "the worlds DIRECTORY is not a world file"
        );
    }

    /// Cancel is the fail-safe answer: the file survives and the panel is
    /// untouched. Driven through the entry's OWN handler
    /// ([`delete_world_handler`]), which is what the contributed
    /// `ContextMenuEntry` runs -- `ContextMenuEntry::handler` is private,
    /// so this is the only way to fire the real thing.
    #[gpui::test]
    async fn test_delete_world_cancel_keeps_the_file_and_the_panel(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "worlds/test.toml");

        let handler = delete_world_handler(workspace.downgrade(), "worlds/test.toml".to_string());
        cx.update(|window, cx| handler(window, cx));
        assert_eq!(
            cx.pending_prompt(),
            Some((
                "Delete the world \"worlds/test\" (worlds/test.toml)?".to_string(),
                "This cannot be undone.".to_string(),
            )),
            "the prompt must name the world AND the file it will unlink"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert!(
            dir.path().join("worlds/test.toml").is_file(),
            "Cancel must leave the file on disk"
        );
        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("Cancel must leave the panel Ready");
            };
            assert_eq!(open.source_rel, "worlds/test.toml");
        });
    }

    /// Confirm unlinks the file, and because that file was the OPEN
    /// document the panel drops back to Empty -- it must not keep showing
    /// a world you can still edit, undo and "save" into nothing.
    #[gpui::test]
    async fn test_delete_world_confirm_removes_the_file_and_clears_the_panel(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "worlds/test.toml");

        let handler = delete_world_handler(workspace.downgrade(), "worlds/test.toml".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(
            !dir.path().join("worlds/test.toml").exists(),
            "Delete must unlink the file"
        );
        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(panel.state, ViewerState::Empty),
                "the open document's file is gone, so the panel must clear"
            );
            let stems: Vec<&str> = panel.worlds.iter().map(|w| w.stem.as_str()).collect();
            assert_eq!(
                stems,
                ["worlds/sub"],
                "the listing must lose the deleted world"
            );
        });
    }

    /// The prompt must SAY when the file being deleted is the open
    /// document and it has unsaved edits -- and must say it only then, not
    /// whenever the panel happens to be dirty. It deliberately does NOT
    /// offer to save: deleting the file makes the edit moot, and a "Save"
    /// here would write bytes about to be unlinked (ggo-ide's delete never
    /// dirty-guarded either).
    #[gpui::test]
    async fn test_delete_world_prompt_names_unsaved_edits(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "worlds/test.toml");
        dirty_the_world(&panel, cx);

        // A different world, while the OPEN one is dirty: those edits are
        // not at stake, so the detail must not claim they are.
        let other = delete_world_handler(workspace.downgrade(), "worlds/sub.toml".to_string());
        cx.update(|window, cx| other(window, cx));
        assert_eq!(
            cx.pending_prompt().map(|(_, detail)| detail),
            Some("This cannot be undone.".to_string()),
            "another world's deletion must not warn about THIS one's edits"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        let handler = delete_world_handler(workspace.downgrade(), "worlds/test.toml".to_string());
        cx.update(|window, cx| handler(window, cx));
        assert_eq!(
            cx.pending_prompt().map(|(_, detail)| detail),
            Some("This cannot be undone. Unsaved edits to it will be lost.".to_string()),
        );
        assert!(
            !cx.pending_prompt()
                .is_some_and(|(msg, _)| msg.contains("save")),
            "the prompt must warn, not offer a save into a file it is about to unlink"
        );
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(!dir.path().join("worlds/test.toml").exists());
    }

    /// Deleting a DIFFERENT world leaves the open document alone but still
    /// re-enumerates, so `+ Instance` stops offering a world that no longer
    /// exists (an instance stem that resolves to nothing renders a
    /// placeholder, silently).
    #[gpui::test]
    async fn test_delete_world_refreshes_instance_candidates(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "worlds/test.toml");
        panel.update(cx, |panel, _cx| {
            assert!(
                panel
                    .instance_candidates()
                    .contains(&"worlds/sub".to_string()),
                "worlds/sub starts out offered"
            );
        });

        let handler = delete_world_handler(workspace.downgrade(), "worlds/sub.toml".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("deleting another world must not disturb the open one");
            };
            assert_eq!(open.source_rel, "worlds/test.toml");
            assert!(
                !panel
                    .instance_candidates()
                    .contains(&"worlds/sub".to_string()),
                "a deleted world must stop being an + Instance candidate"
            );
        });
    }
}
