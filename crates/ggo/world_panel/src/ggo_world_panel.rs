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

mod audio_budget;
mod bags;
mod canvas;
mod inspector;
mod live;
mod loader;
mod world_canvas_item;
mod world_dock;
pub use live::CanvasMode;
pub use world_canvas_item::WorldCanvasItem;
pub use world_dock::{OpenMode, WorldDock};

use emerald_editor_link::{CartBlobKind, EditCommand, EditorMode, GestureKind};
use live::{LiveStatus, LiveView};

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use editor::{Editor, EditorEvent};
use futures::channel::oneshot;
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, EntityId, FocusHandle, Focusable, IntoElement,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Render, RenderImage, ScrollWheelEvent, Styled, Subscription, Task, WeakEntity, Window, actions,
    div, px,
};
use serde_json::Value;
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, Divider, DropdownMenu, PopoverMenu, ToggleState};
use workspace::{SplitDirection, Workspace};

use ggo_map_panel::PaintSession;
use ggo_map_panel::loader::map_stem;
use ggo_map_panel::paint_ui::{self, PaintHost as _};
use ggo_worldlib::backgrounds::MergedBackground;
use ggo_worldlib::drag_ops::{self, View};
use ggo_worldlib::merge_candidates::merge_candidates;
use ggo_worldlib::render::{
    AssetLoads, DEVICE_SCREEN_H, DEVICE_SCREEN_W, DrawItem, DrawKind, Loadable, RgbaImage,
    Selection, active_camera_origin, build_draw_list_multi, hit_test, items_in_rect, world_label,
};
use ggo_worldlib::schemas::{ComponentSchema, FieldKind, defaults_for};
use ggo_worldlib::sprites::map_doc::{MapDocStore, Stamp};
use ggo_worldlib::sprites::tileset_doc::TILE_PX;
use ggo_worldlib::sprites::{io, palette565};
use ggo_worldlib::world_doc::{WorldDocStore, WorldOp};
use ggo_worldlib::world_file::write_world;
use ggo_worldlib::world_file::{
    self, BACKGROUND_LAYER_COUNT, Background, WorldEntity, fragment_to_toml, parse_fragment,
};
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
        /// Selects every entity and instance in the world.
        SelectAll,
        /// Clears the selection.
        ClearSelection,
        /// Copies the selection to the clipboard as world-file TOML.
        Copy,
        /// Pastes entities/instances from the clipboard.
        Paste,
        /// Duplicates the selection.
        Duplicate,
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
        NudgeDownTile,
        /// Draws the canvas with the design renderer, ending any live
        /// session.
        ToggleDesign,
        /// Draws the canvas with the viewer cart running the open world.
        ToggleLive,
        /// Switches the canvas between the design renderer and the viewer
        /// cart.
        ToggleCanvasMode
    ]
);

const GGO_WORLD_PANEL_KEY: &str = "GGOWorldPanel";

/// The panel's key-dispatch context (`.key_context`), which the
/// [`bind_panel_keys`] bindings are scoped to.
const KEY_CONTEXT: &str = "GgoWorldPanel";

/// Fixed default width until the dock grows real settings persistence,
/// and the canvas's fallback size before a first layout.
const DEFAULT_WIDTH: Pixels = px(360.);

/// Inspector column width inside the panel.
const INSPECTOR_WIDTH: Pixels = px(220.);
/// The entity/instance list beside the inspector.
const LIST_WIDTH: Pixels = px(140.);
/// The paint column, which takes the list+inspector's place while a map is
/// under the brush -- wider than the inspector because the tileset strip
/// inside it is a picking surface, not a field.
const PAINT_WIDTH: Pixels = px(280.);
/// Paste/duplicate offset when the cursor is not over the canvas: one tile.
const PASTE_OFFSET_PX: f64 = 16.0;

pub fn init(cx: &mut App) {
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
        let dock = cx.new(|cx| WorldDock::new(weak_workspace, cx));
        workspace.add_panel(dock, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorldDock>(window, cx);
        });
    })
    .detach();
}

/// Empty-state text. The panel has no picker of its own by design (F4 X1):
/// worlds arrive by clicking a `**/worlds/**/*.toml` in the project panel.
const EMPTY_MESSAGE: &str = "Open a world file from the project panel";

/// The phrase [`WorldPanel::remote_read`] answers with while a load is
/// still in flight. Public because the agent socket host retries on it
/// (and only on it) -- a substring match across crates that would rot
/// silently if either side reworded independently.
pub const WORLD_STILL_LOADING: &str = "still loading";

/// The status line's text while the canvas is set to Live but no session
/// exists -- a windowless load (the close prompt's reload, the MCP
/// `world_open`) reaches the Ready state without ever entering Live.
const LIVE_IDLE: &str = "Live · not running";

/// The suffix a stateful `debug_selector` ends in. Toggled state lives in
/// the SELECTOR because `toggle_state` and `ToggleState` change only how a
/// control paints, and a test resolving it by id could never tell the two
/// apart.
fn toggle_suffix(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

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
#[cfg(test)]
const META_SPRITE: &str = "MetaSprite";
#[cfg(test)]
const SPRITE_COMPONENT: &str = "Sprite";

/// How many stem suggestions render under a focused Asset field.
const STEM_SUGGESTION_CAP: usize = 8;

/// Every `<asset root>`-relative stem with extension `ext`, sorted --
/// the completion feed for an Asset field. Recursive (a project's
/// `sprites/ui/icons/` layout must surface), with worldlib's walker
/// limits: dotdirs, `target`/`node_modules`/`dist` skipped, depth capped
/// at its `MAX_SCAN_DEPTH`.
fn list_asset_stems(root: &Path, ext: &str) -> Vec<String> {
    // worldlib's walker, not a private one: it skips dotdirs, `target`,
    // `node_modules` and caps depth, and this runs on the render thread's
    // focus-change path against a whole asset root.
    let mut stems: Vec<String> = ggo_worldlib::sprites::io::list_all_files(root)
        .into_iter()
        .filter_map(|rel| {
            let path = Path::new(&rel);
            path.extension()
                .and_then(|e| e.to_str())
                .filter(|e| e.eq_ignore_ascii_case(ext))?;
            Some(path.with_extension("").to_string_lossy().replace('\\', "/"))
        })
        .collect();
    stems.sort();
    stems
}

/// The worktree-relative path of the file an Asset field names, or
/// `None` when the stem is empty, the file is missing, or the asset root
/// lies outside the worktree (nothing the workspace could open).
#[cfg(test)]
fn asset_rel_for_stem(
    project_root: &Path,
    asset_root: &Path,
    stem: &str,
    ext: &str,
) -> Option<String> {
    if stem.is_empty() {
        return None;
    }
    let abs = inspector::asset_abs_path(asset_root, stem, ext)?;
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

/// Where the add-layer flow puts a world's generated background map. The
/// leading `worlds/` is dropped but nesting is kept, so two worlds with
/// the same basename in different subdirectories cannot collide.
fn background_map_rel(world_stem: &str, layer: u8) -> String {
    let stem = world_stem.strip_prefix("worlds/").unwrap_or(world_stem);
    format!("maps/{stem}.bg{layer}.map")
}

/// The sparse `(x, y, cell)` writes that would make the top-left `w x h`
/// of a `doc_w x doc_h` map read as `cells` -- what a cart layer readback
/// changed, and nothing else.
///
/// The readback's dimensions are the cart's SHADOW, which it clamped the
/// map to: a cell past the document's own map is dropped rather than
/// resizing it, since the cart never saw the rest of the file and cannot
/// be the authority on what is out there.
fn cell_writes(
    doc_cells: &[u16],
    doc_w: u16,
    doc_h: u16,
    w: u16,
    h: u16,
    cells: &[u16],
) -> Vec<(i32, i32, u16)> {
    let mut writes = Vec::new();
    for y in 0..h.min(doc_h) {
        for x in 0..w.min(doc_w) {
            let from = usize::from(y) * usize::from(w) + usize::from(x);
            let into = usize::from(y) * usize::from(doc_w) + usize::from(x);
            let (Some(cell), Some(held)) = (cells.get(from), doc_cells.get(into)) else {
                continue;
            };
            if cell != held {
                writes.push((i32::from(x), i32::from(y), *cell));
            }
        }
    }
    writes
}

/// Patch a `.map` that has no paint session open with the cells the cart
/// handed back, writing only if they actually differ -- an untouched
/// `.map` rewritten would wake the emu panel's watch-mode repack over no
/// edit at all.
fn patch_map_on_disk(root: &Path, rel: &str, w: u16, h: u16, cells: &[u16]) -> Result<(), String> {
    let map = io::open_map(root, rel).map_err(|e| e.to_string())?;
    let writes = cell_writes(&map.cells, map.w, map.h, w, h, cells);
    if writes.is_empty() {
        return Ok(());
    }
    let mut store = MapDocStore::new(map.til_path, map.pal_path, map.w, map.h, map.cells);
    store.apply(ggo_worldlib::sprites::map_doc::MapOp::SetCells(writes));
    io::save_map(root, rel, &store.state()).map_err(|e| e.to_string())
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
    // A SECOND click on the world that is already open AND active splits
    // its `.toml` out to a right pane (world view left, toml text right,
    // toml focused). The first click never opens the toml.
    //
    // The tab is found by what its OWN panel has open, not by asking the
    // dock what it is showing: with a tab per world, the dock's active
    // panel says nothing about where the clicked world lives.
    let canvas_for_rel = workspace
        .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
        .find(|item| item.read(cx).panel().read(cx).open_rel_path_now() == Some(rel.as_str()));
    let second_click_canvas = canvas_for_rel.filter(|canvas| {
        workspace
            .active_item(cx)
            .is_some_and(|item| item.item_id() == canvas.entity_id())
    });
    if let Some(canvas) = second_click_canvas {
        let panes: Vec<_> = workspace.panes().to_vec();
        let canvas_pane = panes
            .iter()
            .find(|pane| {
                pane.read(cx)
                    .items()
                    .any(|item| item.item_id() == canvas.entity_id())
            })
            .cloned();
        // Where does the toml already live, if anywhere?
        let toml_location = panes.iter().find_map(|pane| {
            pane.read(cx)
                .items()
                .find(|item| item.project_path(cx).as_ref() == Some(path))
                .map(|item| (pane.clone(), item.item_id()))
        });
        match (toml_location, canvas_pane) {
            // The toml sits as a tab in the CANVAS's own pane: activating
            // it there would just swap the world view away. MOVE it out
            // into a fresh right split instead.
            (Some((pane, item_id)), Some(canvas_pane)) if pane == canvas_pane => {
                let new_pane =
                    workspace.split_pane(canvas_pane.clone(), SplitDirection::Right, window, cx);
                workspace::move_item(&canvas_pane, &new_pane, item_id, 0, true, window, cx);
            }
            // Already open in some OTHER pane: focus it there.
            (Some((pane, _)), _) => {
                let open_toml =
                    workspace.open_path(path.clone(), Some(pane.downgrade()), true, window, cx);
                window
                    .spawn(cx, async move |_| {
                        if let Err(e) = open_toml.await {
                            log::error!("GGO: failed to focus the world's toml: {e}");
                        }
                    })
                    .detach();
            }
            // Not open anywhere: open it in a pane BESIDE the canvas --
            // an existing other pane if one is there, else a fresh right
            // split (never stacking extra panes).
            (None, canvas_pane) => {
                let other = panes
                    .iter()
                    .find(|pane| canvas_pane.as_ref() != Some(*pane))
                    .cloned();
                let target = match other {
                    Some(pane) => pane,
                    None => {
                        let active = workspace.active_pane().clone();
                        workspace.split_pane(active, SplitDirection::Right, window, cx)
                    }
                };
                let open_toml =
                    workspace.open_path(path.clone(), Some(target.downgrade()), true, window, cx);
                window
                    .spawn(cx, async move |_| {
                        if let Err(e) = open_toml.await {
                            log::error!("GGO: failed to open the world's toml split: {e}");
                        }
                    })
                    .detach();
            }
        }
        return true;
    }

    // FIRST click: the dock reveals itself and opens the world -- a tab
    // per world, whose panel owns that document, with the dock showing
    // whichever tab is active (see [`WorldDock::open_world`]).
    ggo_common::open_in_panel(
        workspace,
        window,
        cx,
        move |dock: &mut WorldDock, window, cx| {
            dock.open_world(&rel, window, cx);
        },
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
    ggo_common::panel_entry_handler(
        workspace.clone(),
        move |dock: &Entity<WorldDock>, window, cx| {
            // The tab that HAS this world open is the one that must run
            // the delete: `delete_world` clears the document it deletes,
            // and only that panel's document is the deleted file. Any
            // panel can run the prompt otherwise -- it re-derives its own
            // root from `rel` -- so the active tab's does, and a workspace
            // with no world open at all gets a throwaway one.
            let showing_it: Vec<_> = dock
                .read(cx)
                .open_panels()
                .into_iter()
                .filter(|panel| panel.read(cx).open_rel_path_now() == Some(rel.as_str()))
                .collect();
            let panels = if showing_it.is_empty() {
                let panel = dock.read(cx).active().unwrap_or_else(|| {
                    let workspace = workspace.clone();
                    cx.new(|cx| WorldPanel::new(Some(workspace), cx))
                });
                vec![panel]
            } else {
                showing_it
            };
            for panel in panels {
                let rel = rel.clone();
                panel
                    .update(cx, |panel, cx| panel.delete_world(rel, window, cx))
                    .detach();
            }
        },
    )
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
    /// Canvas-relative cursor position while the pointer is over the
    /// canvas -- where a paste lands. `None` once it leaves.
    hover: Option<[f64; 2]>,
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
    /// The primary (last-selected) item's start position: the one the
    /// snap applies to; the rest of the set follows by the same delta.
    start_pos: [f64; 2],
    start_world: [f64; 2],
    /// Every selected item's start position, in selection order.
    starts: Vec<(Selection, [f64; 2])>,
    /// Whether a move event has actually displaced the primary. A click
    /// that merely SELECTS also arms a drag (so the next move can pick it
    /// up), and treating that as an edit would re-send the whole world
    /// blob on every selection click -- which resets the cart's runtime
    /// state and snaps every moving entity back.
    moved: bool,
}

/// An in-flight rubber-band selection on empty canvas, in world coords.
#[derive(Clone)]
struct Marquee {
    start: [f64; 2],
    current: [f64; 2],
    /// Shift held at mouse-down: the band ADDS to the selection.
    additive: bool,
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

/// Which `.map` paint mode is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaintTarget {
    /// A `[[background]]` slot of the OPEN document, by layer.
    BgSlot(u8),
    /// A `Tilemap` entity's map, by entity index -- the same frame
    /// `Selection::Entity` indexes in.
    TilemapEntity(usize),
}

/// The `Tilemap` component's fields, when `entity` has one naming a
/// non-empty stem. This is what makes an entity a paint target, so every
/// entry point ("Paint tiles", double-click) gates on it and
/// [`WorldPanel::paint_target_rel`] reads the anchor out of the same map.
fn tilemap_fields(entity: &WorldEntity) -> Option<&serde_json::Map<String, Value>> {
    let fields = entity.components.get("Tilemap")?.as_object()?;
    let stem = fields.get("stem")?.as_str()?;
    (!stem.is_empty()).then_some(fields)
}

/// What the canvas gesture does. `Entities` is the world editor proper;
/// `Paint` is the in-world map editor (spec 2026-08-29), which takes over
/// the canvas gestures, Escape and undo/redo for as long as it is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum EditMode {
    #[default]
    Entities,
    Paint(PaintTarget),
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
    /// The load's `[[instance]]` background sets, kept so an edit to THIS
    /// world's slots can re-run the merge without re-reading every
    /// instance world file -- see `loader::LoadedWorld`'s field doc.
    instance_backgrounds: HashMap<String, Vec<Background>>,
    schemas: Vec<ComponentSchema>,
    /// One gpui `RenderImage` (BGRA) per composed worldlib image, built
    /// once at load time -- see `canvas::build_image_cache`.
    images: Arc<HashMap<usize, Arc<RenderImage>>>,
    view: Rc<RefCell<ViewShared>>,
    /// The selection SET, in selection order; the last entry is the
    /// primary -- what the inspector edits and what a drag snaps.
    selected: Vec<Selection>,
    marquee: Option<Marquee>,
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
    /// Callers waiting for a save that the cart has not answered yet --
    /// the workspace `Item::save` a close prompt awaits. Resolved (with
    /// the write's own result) the moment the save settles; dropping them
    /// unresolved is what a closed world reports as a failure, since a
    /// caller that never hears back can never let the tab go.
    save_waiters: Vec<oneshot::Sender<Result<(), String>>>,
    /// Why the popout Emulate could not launch (failed build or spawn),
    /// shown on the toolbar next to `save_error`.
    popout_error: Option<String>,
    /// What a copy or paste could not do (bad clipboard text, a refused
    /// instance), shown on the toolbar; cleared by the next successful paste.
    clipboard_error: Option<String>,
    /// Region bytes per audio stem, filled off-thread -- see
    /// [`audio_budget`]. Cleared by [`WorldPanel::refresh_worlds`].
    audio_sizes: audio_budget::AudioSizes,
    audio_size_generation: u64,
    _audio_size_task: Option<Task<()>>,
    /// The open palette color picker, if any -- at most one, anchored to
    /// one Color565 field.
    color_picker: Option<ColorPicker>,
    /// The focused Asset field's completion feed, if any -- at most one.
    /// Recomputed only when focus moves onto a DIFFERENT asset field
    /// ([`WorldPanel::refresh_stem_completion`]), so the directory walk
    /// runs once per focus, not per frame.
    stem_completion: Option<StemCompletion>,
    mode: EditMode,
    /// One session per `.map` REL edited since this world opened. Keyed by
    /// rel rather than by target: a map reachable from two targets (a
    /// background slot and a `Tilemap` entity) is ONE document with one
    /// undo history. Sessions outlive the mode -- leaving paint mode must
    /// not throw away undo history the user can still reach by re-entering
    /// -- and die with the `OpenWorld`.
    sessions: HashMap<String, PaintSession>,
    /// The in-flight session load, if any. Replacing it cancels the
    /// previous load, which is how a fast switch between two targets
    /// resolves to the one the user asked for last.
    _session_loading: Option<Task<()>>,
    /// Why paint mode could not open, shown on the toolbar. Cleared by the
    /// next entry attempt.
    paint_error: Option<String>,
    /// True between the CANVAS's own primary-down and its matching up
    /// while painting -- the retired standalone editor's `painting` flag,
    /// ported for the same reason. Paint mode has no `edit_drag` to stand
    /// in for "a gesture is armed", so without this flag any left-held
    /// move over the canvas paints: a drag begun on the tile strip, on a
    /// palette slider, or in another pane stamps cells the moment it
    /// crosses the canvas, and it does so with no `begin_gesture` behind
    /// it (stale `paint_erase`, ops that never fold into one undo entry).
    paint_gesture: bool,
    /// The tileset strip's on-screen bounds, recorded at prepaint so the
    /// strip's mouse handlers can map window coords to a tile
    /// (`ggo_map_panel::paint_ui::strip_cell_at`).
    strip_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// The terrain editor's name input and the two resize inputs. Both are
    /// gpui entities, so they are the HOST's even though the widgets that
    /// draw them are the paint library's; both are created lazily on the
    /// first paint-mode render.
    terrain_name: Option<Entity<Editor>>,
    resize: Option<paint_ui::ResizeFields>,
    /// The live session mirroring this document into the viewer cart, if
    /// Live mode got one. Kept (with `LiveStatus::Failed`) after a
    /// fallback so the toolbar can still say what went wrong.
    live: Option<LiveView>,
    /// How many entities each `[[instance]]` contributes once flattened,
    /// in `[[instance]]` order -- the counts the live index map is built
    /// from. Computed off the UI thread (the walk reads every instanced
    /// world file): at load time by `loader::load_world`, and after that
    /// by [`WorldPanel::refresh_instance_counts`].
    instance_counts: Vec<usize>,
    /// The instance stems `instance_counts` was computed for. A document
    /// edit that leaves this list alone (moving an instance, editing an
    /// entity) cannot change the counts, so this is what decides whether a
    /// recount is owed.
    counted_instances: Vec<String>,
    /// The in-flight recount, if any. Replacing it cancels the previous
    /// one, which is how a burst of instance edits resolves to the last.
    _instance_counts_task: Option<Task<()>>,
    /// Why Live mode is not showing, on the toolbar next to `save_error`.
    /// Set on every fallback to Design, cleared when a session starts.
    live_error: Option<String>,
    /// Bumped by every [`Self::note_doc_changed`]. Read only by the live
    /// session's failed-encode backoff, which has to tell "the same world
    /// the encoder already refused" from "a world that has moved since".
    doc_generation: u64,
}

/// What an Asset field's row shows -- see [`WorldPanel::asset_field_view`].
pub(crate) struct AssetFieldView {
    pub(crate) stem: String,
    pub(crate) status: inspector::AssetStatus,
    /// Worktree-relative path of the resolving file, for the jump.
    pub(crate) rel: Option<String>,
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
        let loaded_instance_stems = loaded
            .store
            .state()
            .instances
            .iter()
            .map(|instance| instance.world.clone())
            .collect();
        let loaded_instance_counts = loaded.instance_counts;
        OpenWorld {
            listing,
            source_rel,
            root,
            store: loaded.store,
            sprite_loads: loaded.sprite_loads,
            map_loads: loaded.map_loads,
            meta_sprite_loads: loaded.meta_sprite_loads,
            merged: loaded.merged,
            instance_backgrounds: loaded.instance_backgrounds,
            schemas: loaded.schemas,
            images: Arc::new(images),
            view: Rc::new(RefCell::new(ViewShared {
                zoom: canvas::ZOOM_DEFAULT,
                pan: None,
                last_bounds: None,
                drag: None,
                hover: None,
            })),
            selected: Vec::new(),
            marquee: None,
            snap: false,
            grid: true,
            edit_drag: None,
            nudge_gesture: None,
            gesture_counter: 0,
            inspector: Vec::new(),
            save_error: None,
            save_waiters: Vec::new(),
            popout_error: None,
            clipboard_error: None,
            audio_sizes: HashMap::new(),
            audio_size_generation: 0,
            _audio_size_task: None,
            color_picker: None,
            stem_completion: None,
            mode: EditMode::default(),
            sessions: HashMap::new(),
            _session_loading: None,
            paint_error: None,
            paint_gesture: false,
            strip_bounds: Rc::new(RefCell::new(None)),
            terrain_name: None,
            resize: None,
            live: None,
            instance_counts: loaded_instance_counts,
            counted_instances: loaded_instance_stems,
            _instance_counts_task: None,
            live_error: None,
            doc_generation: 0,
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

/// The images `old` held that `new` does not -- what a rebuild retires.
fn retired_by_rebuild(
    old: &HashMap<usize, Arc<RenderImage>>,
    new: &HashMap<usize, Arc<RenderImage>>,
) -> Vec<Arc<RenderImage>> {
    old.iter()
        .filter(|(key, _)| !new.contains_key(*key))
        .map(|(_, image)| image.clone())
        .collect()
}

/// The list column's rows: every entity (`#i <first non-Transform
/// component>[ · stem]` -- any component's `stem`, so Tilemap and Sfx
/// rows read as usefully as sprites) then every instance (`⧉ <stem>`).
fn entity_list_rows(state: &ggo_worldlib::world_doc::WorldState) -> Vec<(Selection, String)> {
    let mut rows = Vec::with_capacity(state.entities.len() + state.instances.len());
    for (i, entity) in state.entities.iter().enumerate() {
        let name = entity
            .components
            .keys()
            .find(|k| k.as_str() != "Transform")
            .cloned()
            .unwrap_or_else(|| "Entity".to_string());
        let stem = entity
            .components
            .get(&name)
            .and_then(|c| c.get("stem"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| format!(" · {s}"))
            .unwrap_or_default();
        rows.push((Selection::Entity(i), format!("#{i} {name}{stem}")));
    }
    for (i, instance) in state.instances.iter().enumerate() {
        rows.push((
            Selection::Instance(i),
            format!("⧉ {}", world_label(&instance.world)),
        ));
    }
    rows
}

/// The op that moves `primary` to `primary_pos` and every other item in
/// `starts` by the same delta: the single-item ops when the set is one
/// (unchanged undo behaviour), `MoveMany` otherwise. `starts` holds each
/// item's position at the start of the gesture, so a coalesced drag
/// stays anchored to where it began.
fn move_ops(
    starts: &[(Selection, [f64; 2])],
    primary: Selection,
    primary_pos: [f64; 2],
    gesture: Option<String>,
) -> WorldOp {
    let primary_start = starts
        .iter()
        .find(|(t, _)| *t == primary)
        .map(|(_, p)| *p)
        .unwrap_or(primary_pos);
    let delta = [
        primary_pos[0] - primary_start[0],
        primary_pos[1] - primary_start[1],
    ];
    if starts.len() <= 1 {
        return match primary {
            Selection::Entity(entity) => WorldOp::MoveEntity {
                entity,
                pos: primary_pos,
                gesture,
            },
            Selection::Instance(index) => WorldOp::MoveInstance {
                index,
                pos: primary_pos,
                gesture,
            },
        };
    }
    WorldOp::MoveMany {
        moves: starts
            .iter()
            .map(|(target, start)| (*target, [start[0] + delta[0], start[1] + delta[1]]))
            .collect(),
        gesture,
    }
}

/// How many selected instances actually exist (a stale index after an
/// undo/redo restructure is not something to prompt about).
fn removable_instances(
    selected: &[Selection],
    state: &ggo_worldlib::world_doc::WorldState,
) -> usize {
    selected
        .iter()
        .filter(|s| matches!(s, Selection::Instance(i) if *i < state.instances.len()))
        .count()
}

/// One batch removing every selected item that still exists, entities
/// and instances each in descending index order so earlier removals
/// never shift a later index. `None` when nothing removable is selected.
fn remove_selection_ops(
    selected: &[Selection],
    state: &ggo_worldlib::world_doc::WorldState,
) -> Option<WorldOp> {
    let mut entities: Vec<usize> = selected
        .iter()
        .filter_map(|s| match s {
            Selection::Entity(i) if *i < state.entities.len() => Some(*i),
            _ => None,
        })
        .collect();
    let mut instances: Vec<usize> = selected
        .iter()
        .filter_map(|s| match s {
            Selection::Instance(i) if *i < state.instances.len() => Some(*i),
            _ => None,
        })
        .collect();
    entities.sort_unstable_by(|a, b| b.cmp(a));
    entities.dedup();
    instances.sort_unstable_by(|a, b| b.cmp(a));
    instances.dedup();
    let ops: Vec<WorldOp> = entities
        .into_iter()
        .map(|index| WorldOp::RemoveEntity { index })
        .chain(
            instances
                .into_iter()
                .map(|index| WorldOp::RemoveInstance { index }),
        )
        .collect();
    (!ops.is_empty()).then_some(WorldOp::Batch(ops))
}

/// The current paint-ordered draw list -- built fresh per use (render,
/// hit test, tests), same per-frame-of-change cadence as ggo-ide.
fn draw_items(open: &OpenWorld) -> Vec<DrawItem> {
    build_draw_list_multi(
        &open.store.state(),
        &open.merged,
        &open.selected,
        &open.sprite_loads,
        &open.map_loads,
        &open.meta_sprite_loads,
    )
}

/// Software-composite a draw list into a BGRA canvas whose top-left is
/// world point `origin`. Images blit with source alpha; the gizmo kinds
/// (`Marker`, `Placeholder`, `InstanceOrigin`, `Text`) draw as flat
/// boxes -- the agent's picture is of the LAYOUT, not the editor's
/// chrome, so `SelectionOutline` is skipped. Items paint in draw-list
/// order, which `build_draw_list_multi` has already sorted by z.
pub fn composite_scene(items: &[DrawItem], origin: [f64; 2], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (i64::from(width), i64::from(height));
    let mut canvas = vec![0u8; (w * h * 4) as usize];
    let mut put = |x: i64, y: i64, rgba: [u8; 4]| {
        if x < 0 || y < 0 || x >= w || y >= h || rgba[3] == 0 {
            return;
        }
        let i = ((y * w + x) * 4) as usize;
        let a = u32::from(rgba[3]);
        for (channel, src) in [(2usize, rgba[0]), (1, rgba[1]), (0, rgba[2])] {
            let dst = u32::from(canvas[i + channel]);
            canvas[i + channel] = ((u32::from(src) * a + dst * (255 - a)) / 255) as u8;
        }
        canvas[i + 3] = 255;
    };
    for item in items {
        let x0 = (item.x - origin[0]).round() as i64;
        let y0 = (item.y - origin[1]).round() as i64;
        match &item.kind {
            DrawKind::Image { image } => {
                for sy in 0..i64::from(image.h) {
                    for sx in 0..i64::from(image.w) {
                        let s = ((sy * i64::from(image.w) + sx) * 4) as usize;
                        // A decoded image whose buffer is shorter than
                        // `w * h * 4` draws what it has rather than
                        // panicking the whole composite.
                        let Some(px) = image.rgba.get(s..s + 4) else {
                            break;
                        };
                        put(x0 + sx, y0 + sy, [px[0], px[1], px[2], px[3]]);
                    }
                }
            }
            DrawKind::SelectionOutline => {}
            DrawKind::Text { .. }
            | DrawKind::Marker
            | DrawKind::Placeholder { .. }
            | DrawKind::InstanceOrigin => {
                let color = match &item.kind {
                    DrawKind::Text { .. } => [255, 255, 255, 160],
                    DrawKind::Placeholder { .. } => [255, 64, 64, 200],
                    _ => [96, 200, 255, 160],
                };
                for dy in 0..item.h.round() as i64 {
                    for dx in 0..item.w.round() as i64 {
                        put(x0 + dx, y0 + dy, color);
                    }
                }
            }
        }
    }
    canvas
}

/// Stamp the canvas bounds on the shared view and, on the very first
/// layout, center the camera. DESIGN's prepaint only: Live has its own
/// framing (a centered integer-scaled frame at the cart's camera) and
/// stamps the bounds itself, because centering the design pan from a Live
/// layout would move the view the user left behind in Design.
fn layout_camera(
    view: &Rc<RefCell<ViewShared>>,
    canvas_bounds: Bounds<Pixels>,
    world_center: [f64; 2],
) -> (f64, [f64; 2]) {
    let mut v = view.borrow_mut();
    v.last_bounds = Some(canvas_bounds);
    let zoom = v.zoom;
    if v.pan.is_none() {
        v.pan = Some(canvas::centering_pan(
            f64::from(canvas_bounds.size.width),
            f64::from(canvas_bounds.size.height),
            zoom,
            world_center,
        ));
    }
    (zoom, v.pan.unwrap_or([0.0, 0.0]))
}

/// What one turn of the live session did: whether anything the panel draws
/// moved, and the reason the session must fall back to the design view, if
/// any.
#[derive(Default)]
struct LiveStep {
    changed: bool,
    failure: Option<LiveFailure>,
}

/// Why a session must fall back to the design view.
struct LiveFailure {
    reason: String,
    /// The cart itself reported [`ggo_common::ViewerState::Stopped`]: the
    /// run is already over, so there is nothing left to ask to end.
    /// Everything else leaves a cart RUNNING that this fallback has to
    /// stop, because nothing will be polling it.
    cart_stopped: bool,
}

/// A transport failure on the link: the port or the emulator is gone, not
/// a datagram lost on the wire.
fn link_failed(error: std::io::Error) -> String {
    format!("viewer link failed: {error}")
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

impl OpenWorld {
    /// The last-selected item, if any.
    fn primary(&self) -> Option<Selection> {
        self.selected.last().copied()
    }

    /// Make `target` the primary: appended if new, moved to the end if
    /// already selected.
    fn select_primary(&mut self, target: Selection) {
        self.selected.retain(|s| *s != target);
        self.selected.push(target);
    }

    fn toggle_selected(&mut self, target: Selection) {
        if self.selected.contains(&target) {
            self.selected.retain(|s| *s != target);
        } else {
            self.selected.push(target);
        }
    }

    /// Drop selection entries that no longer index anything -- after an
    /// undo/redo restructure. Surviving indices keep pointing at whatever
    /// now sits there, which is the same rule a single selection had.
    fn prune_selection(&mut self) {
        let state = self.store.state();
        self.selected.retain(|target| match *target {
            Selection::Entity(i) => i < state.entities.len(),
            Selection::Instance(i) => i < state.instances.len(),
        });
    }

    /// Re-derive the cart-index -> selection map from the document and
    /// the last instance counts. Called when either input moves: a world
    /// blob going out, or a recount landing.
    fn rebuild_index_map(&mut self) {
        let entities = self.store.state().entities.len();
        if let Some(live) = self.live.as_mut() {
            live.index_map = live::IndexMap::new(entities, &self.instance_counts);
        }
    }

    /// The document changed in a way the cart has to be told about --
    /// every path that reaches `store.apply`/`undo`/`redo`. A live session
    /// re-sends the whole world blob on its next free tick rather than
    /// trying to describe the edit: the encoder is the only thing that
    /// knows the flattened order the cart indexes in.
    fn note_doc_changed(&mut self) {
        self.doc_generation = self.doc_generation.wrapping_add(1);
        if let Some(live) = self.live.as_mut() {
            live.resend_world();
        }
    }

    /// [`Self::note_doc_changed`] for a step whose BEFORE state is known
    /// -- undo, redo, and the ops that funnel through `apply_op`. A step
    /// the cart can be TOLD about -- a move, a component the inspector
    /// committed, an entity appended or dropped off the end -- is replayed
    /// as those commands instead of a whole world resend: a reload takes
    /// the cart's rows away until the blob has landed and been redrawn,
    /// which is a visible blink for an undo that only put a sprite back
    /// where it was. Everything else -- a background slot, an
    /// `[[instance]]`, a removed component, a removal that shifts the
    /// document's indices -- still re-sends, because the flattened order
    /// the cart indexes in is the encoder's to describe.
    ///
    /// A step the store folded away -- an op that set a field to the value
    /// it already held -- is not a document change at all, and re-sending
    /// the world for one would reset a running cart over a blur.
    fn note_doc_stepped(&mut self, before: &ggo_worldlib::world_doc::WorldState) {
        let after = self.store.state();
        if before.entities == after.entities
            && before.instances == after.instances
            && before.backgrounds == after.backgrounds
        {
            return;
        }
        self.doc_generation = self.doc_generation.wrapping_add(1);
        // A step that changed how many DIRECT entities the document has
        // re-flattens every cart index: the map has to follow it here,
        // because a replayed step never pushes a world blob and the blob
        // push is the only other place that rebuilds it.
        // `refresh_instance_counts` early-returns on an unchanged instance
        // list, so nothing else would. Without this the entity `+ Entity`
        // just appended has no cart index at all, and every row the cart
        // publishes for it is dropped on the floor.
        if before.entities.len() != after.entities.len() {
            self.rebuild_index_map();
        }
        let Some(live) = self.live.as_mut() else {
            return;
        };
        match live.plan_replay(before, &after) {
            Some(edits) => live.queue_edits(edits),
            None => live.resend_world(),
        }
    }

    /// How big the document is, for the Live lookups. Takes the state
    /// the caller already has: `WorldDocStore::state` deep-clones the
    /// document, so asking per row would clone it per row.
    fn doc_counts(state: &ggo_worldlib::world_doc::WorldState) -> live::DocCounts {
        live::DocCounts {
            entities: state.entities.len(),
            instances: state.instances.len(),
        }
    }

    /// One turn of the live session, on the poll task's wake.
    ///
    /// `wall` is the executor's clock and is used for exactly one thing:
    /// the build deadline, which cannot be measured on the cart's clock
    /// because a cart that is still being built has never framed. Every
    /// other deadline here runs on `LiveView::cart_now`.
    fn live_step(&mut self, wall: Instant) -> LiveStep {
        // Field-precise borrow on purpose: the steps below read the
        // document (`store`, `merged`, `root`, `view`) while the session
        // is borrowed mutably.
        // A greeting's re-push of the tool and mode happens under the
        // session borrow, while the status row is only settled at the end
        // of the tick: carried out rather than written straight to
        // `live_error`, which the push block below would clear on its way
        // past.
        let mut connect_error: Option<String> = None;
        let Some(live) = self.live.as_mut() else {
            return LiveStep::default();
        };
        if matches!(live.status, LiveStatus::Failed(_)) {
            return LiveStep::default();
        }
        let status_before = live.status.clone();
        let frame_before = live.frame.as_ref().map(|(number, _)| *number);
        let failed = |reason: String| LiveStep {
            changed: true,
            failure: Some(LiveFailure {
                reason,
                cart_stopped: false,
            }),
        };
        let now = live.advance_cart_clock();
        match live.endpoint.state() {
            ggo_common::ViewerState::Building => {
                // A rebuild retires whatever the previous run presented:
                // the emu panel hands that atlas tile back, so a frame
                // kept across the rebuild would be re-inserted by the next
                // paint. The boot screen covers the gap.
                let dropped = live.frame.take().is_some();
                // Back to square one, however far the session had got: the
                // cart that greeted us is being replaced by one that never
                // has. Without this the `Running` arm below sees a session
                // that is already past `Building`, never greets the new
                // cart, and the world is never re-sent to it.
                let regressed = live.status != LiveStatus::Building;
                live.status = LiveStatus::Building;
                // The deadline times out THIS build, and a rebuild is a
                // new one. Left on the boot's baseline, any session that
                // has been live longer than the deadline would fail its
                // rebuild's very first poll instead of re-greeting.
                if regressed {
                    live.started = wall;
                }
                return if wall.duration_since(live.started) >= live::BUILD_DEADLINE {
                    failed("viewer cart build timed out".to_string())
                } else {
                    LiveStep {
                        changed: dropped || regressed,
                        failure: None,
                    }
                };
            }
            ggo_common::ViewerState::Stopped(reason) => {
                return LiveStep {
                    changed: true,
                    failure: Some(LiveFailure {
                        reason,
                        cart_stopped: true,
                    }),
                };
            }
            ggo_common::ViewerState::Running => {}
        }
        if matches!(live.status, LiveStatus::Building) {
            live.status = LiveStatus::Connecting;
            live.connect_since = now;
            live.last_hello = now;
            // Every baseline fresh, including this one: a rebuild can
            // reach here with the PREVIOUS cart's outstanding stale
            // re-greeting still recorded, which the deadline below would
            // read as "the new cart has already stopped answering".
            live.stale_hello = None;
            if let Err(error) = live.mailbox.hello() {
                return failed(link_failed(error));
            }
        }
        let mut changed = match live.mailbox.poll(now) {
            Ok(changed) => changed,
            Err(error) => return failed(link_failed(error)),
        };
        if let Some(version) = live.mailbox.proto_version_mismatch() {
            return failed(format!(
                "viewer cart predates the link protocol (cart v{version}); rebuild it"
            ));
        }
        if live.mailbox.is_connected() {
            if live.status != LiveStatus::Connected {
                live.status = LiveStatus::Connected;
                live.stale_hello = None;
                // A greeting resets the cart's whole view, so everything
                // this document owns has to go out again -- including the
                // world handshake. Without this an `Acked(N)` from before
                // the greeting could flip to `Loaded` on the post-reset
                // `frame_seq` (which restarts at 0 and climbs again) even
                // though the re-send has not landed, or never left because
                // the same tick's encode failed.
                live.world_dirty = true;
                live.world_retry_at = None;
                live.world_sync = live::WorldSync::Sending;
                live.layers_dirty.mark_all();
                live.forget_input();
                live.forget_gestures();
                // Where the cart starts looking is the document's own
                // framing -- the camera origin the Design renderer frames
                // -- not wherever the design pan happens to sit.
                live.pending_camera = Some(active_camera_origin(&self.store.state()));
                // A greeting resets the cart to Edit and to its select
                // tool, and nothing else re-applies either: the mailbox
                // re-sends only the (deprecated) system mask. Without this
                // a re-greeted session shows a rail the cart is not
                // obeying.
                //
                // A re-push that fails is the same defect `set_live_mode`
                // reports: the rail keeps showing a tool and a mode the
                // cart is not running, so it goes on the status row too.
                if live.tool != 0
                    && let Err(error) = live.mailbox.set_tool(live.tool)
                {
                    log::warn!("GGO: live tool: {error}");
                    connect_error = Some(format!("live update: {error}"));
                }
                if live.mode != EditorMode::Edit {
                    match live.mailbox.set_mode(live.mode) {
                        Ok(()) => live.mode_push_failed = false,
                        Err(error) => {
                            live.mode_push_failed = true;
                            log::warn!("GGO: live mode: {error}");
                            connect_error = Some(format!("live update: {error}"));
                        }
                    }
                }
            }
            // `is_connected` never goes false on its own; a cart that was
            // reset or unplugged is only visible as a session that stopped
            // framing (Phase 1 contracts).
            if live.mailbox.is_stale(now, live::STALE_AFTER) {
                live.status = LiveStatus::Connecting;
                live.stale_hello = Some(now);
                live.connect_since = now;
                live.last_hello = now;
                if let Err(error) = live.mailbox.hello() {
                    return failed(link_failed(error));
                }
            }
        } else if let Some(greeted_at) = live.stale_hello {
            if now.duration_since(greeted_at) >= live::STALE_AFTER {
                return failed("viewer cart stopped answering".to_string());
            }
        } else if now.duration_since(live.connect_since) >= live::CONNECT_DEADLINE {
            return failed("viewer cart never answered".to_string());
        }
        if !live.mailbox.is_connected() && now.duration_since(live.last_hello) >= live::HELLO_RETRY
        {
            live.last_hello = now;
            if let Err(error) = live.mailbox.hello() {
                return failed(link_failed(error));
            }
        }
        let sync_before = live.world_sync;
        live.advance_world_sync();
        changed |= live.world_sync != sync_before;
        // The cart groups its own selection and drags, so it needs the
        // document's `[[instance]]` spans -- and it needs them for the
        // world it is actually holding. Sent on the edge into `Loaded`,
        // which is the one moment the index map and the cart's flattened
        // table are known to describe the same world.
        if sync_before != live::WorldSync::Loaded && live.loaded() {
            let ranges = live.index_map.instance_ranges();
            if let Err(error) = live.mailbox.groups(&ranges) {
                log::warn!("GGO: live groups: {error}");
            }
        }
        // The pointer is never held back, not even mid-transfer: the cart
        // reads its press and release edges off consecutive samples, so a
        // sample deferred until a blob finished would leave a button stuck
        // down for the length of the transfer. One per tick -- a tick is a
        // cart frame. Both this and the commands below are fire-and-forget
        // (`LinkMailbox::pointer`), so a send that fails is logged and
        // superseded rather than retried, and neither touches the status
        // row the pushes further down own.
        if live.status == LiveStatus::Connected {
            if let Some(pointer) = live.take_pointer()
                && let Err(error) = live.mailbox.pointer(
                    pointer.device.0,
                    pointer.device.1,
                    pointer.buttons,
                    pointer.modifiers,
                    pointer.snap,
                )
            {
                log::warn!("GGO: live pointer: {error}");
            }
            // Commands DO wait for a free receive queue: four blob chunks
            // and a command are five datagrams into a queue four deep, and
            // a command is one discrete request -- losing it swallows the
            // keypress outright, where a lost pointer sample is superseded
            // by the next one.
            if !live.mailbox.busy() {
                for command in std::mem::take(&mut live.pending_commands) {
                    if let Err(error) = live.mailbox.command(command) {
                        log::warn!("GGO: live command: {error}");
                    }
                }
                // ONE command per tick: the cart commits it into its
                // single command slot and stops reading datagrams until
                // the frame after has acked it, so the rest of a burst
                // would only fill a four-deep queue. A transform the
                // mailbox refuses (an index past the cart's cap) is
                // dropped rather than retried, and never recorded as
                // sent: the row it would have suppressed is the cart's
                // own after all. Every other refusal falls back on the
                // world blob, which can always say what a command could
                // not.
                if let Some(edit) = live.pending_edits.pop_front() {
                    match edit {
                        live::CartEdit::Transform { index, x, y } => {
                            match live.mailbox.set_transform(index, x, y) {
                                Ok(()) => live.note_transform_sent((index, x, y)),
                                Err(error) => log::warn!("GGO: live set_transform: {error}"),
                            }
                        }
                        live::CartEdit::Component { index, bag, .. } => {
                            if let Err(error) = live.mailbox.set_component(index, &bag) {
                                log::warn!("GGO: live set_component: {error}");
                                live.resend_world();
                            }
                        }
                        live::CartEdit::Despawn { index } => {
                            if let Err(error) = live.mailbox.despawn(index) {
                                log::warn!("GGO: live despawn: {error}");
                                live.resend_world();
                            }
                        }
                        live::CartEdit::Spawn { entity, bags } => {
                            let bodies: Vec<&[u8]> =
                                bags.iter().map(std::vec::Vec::as_slice).collect();
                            match live.mailbox.spawn(&bodies) {
                                Ok(true) => live.note_spawn_sent(entity),
                                // The transfer queue is full and the spawn
                                // was NOT sent: it goes back to the front
                                // and is asked for again next tick.
                                // Unreachable while this whole flush is
                                // gated on `!busy()` -- kept because the
                                // gate is the caller's, not `spawn`'s, and
                                // a silently dropped spawn is exactly what
                                // the pending queue exists to prevent.
                                Ok(false) => {
                                    drop(bodies);
                                    live.pending_edits
                                        .push_front(live::CartEdit::Spawn { entity, bags });
                                }
                                Err(error) => {
                                    log::warn!("GGO: live spawn: {error}");
                                    live.resend_world();
                                }
                            }
                        }
                    }
                }
            }
        }
        // One update per tick, and none while a blob is in flight: the
        // cart's APP receive queue is four datagrams deep and a transfer
        // already fills it.
        if live.status == LiveStatus::Connected && !live.mailbox.busy() {
            // A world the panel already failed to push is not re-attempted
            // until the document moves or [`live::ENCODE_RETRY`] of cart
            // time passes: `world_dirty` alone re-encodes -- which walks
            // every instanced world file -- on every frame the cart draws.
            // The layer and camera pushes are not held up by it; they do
            // not go through the encoder.
            let world_ready = live.world_dirty
                && match live.world_retry_at {
                    None => true,
                    Some((generation, retry_at)) => {
                        generation != self.doc_generation || now >= retry_at
                    }
                };
            // Whether this tick had something to push at all: a tick that
            // sends nothing must neither set nor clear the status row.
            let acted = world_ready
                || live.layers_dirty.any()
                || !live.layer_queue.is_empty()
                || live.pending_camera.is_some();
            // A layer or camera push that worked must not take a world
            // failure off the status row while that world is still unsent.
            let world_held_back = live.world_dirty && !world_ready;
            let mut live_error = connect_error.take();
            if world_ready {
                // follow-up: this encode is on the UI thread; it belongs on
                // a background task with the blob applied on a later tick.
                let pushed = live::encode_world(&self.store, &self.root).and_then(|blob| {
                    live.mailbox.load_world(&blob)?;
                    Ok(())
                });
                match pushed {
                    // A world the encoder rejects is a document problem,
                    // not a link problem: say so on the status row and stay
                    // live on whatever the cart already has. `world_dirty`
                    // STAYS set, so the edit that fixes the document sends
                    // it -- clearing it here would leave the session
                    // showing a world it silently failed to send, with
                    // nothing to re-arm it.
                    Err(error) => {
                        live_error = Some(format!("live update: {error}"));
                        live.world_retry_at = Some((
                            self.doc_generation,
                            now.checked_add(live::ENCODE_RETRY).unwrap_or(now),
                        ));
                    }
                    Ok(()) => {
                        live.world_dirty = false;
                        live.world_retry_at = None;
                        // The rows the cart has published are the PREVIOUS
                        // world's until it republishes.
                        live.world_sync = live::WorldSync::Sending;
                        // The blob places every row itself, and its
                        // flattening may not be the one these indices
                        // were computed against.
                        live.forget_edits();
                        live.index_map = live::IndexMap::new(
                            self.store.state().entities.len(),
                            &self.instance_counts,
                        );
                    }
                }
            } else if live.layers_dirty.any() || !live.layer_queue.is_empty() {
                // A dirty slot's payload is re-read and moved to the BACK
                // of whatever is still queued. Re-queueing only the dirty
                // slots is what lets a continuous stroke -- which
                // re-dirties its own slot every tick -- get past the front
                // of the queue at all; dropping the slot's stale copy
                // first is what keeps the bytes that do go out the ones on
                // disk now.
                if live.layers_dirty.any() {
                    let dirty = live.layers_dirty.take();
                    let loads = live::layer_loads(&self.root, &self.merged);
                    live.layer_queue
                        .retain(|queued| !dirty.contains(&queued.layer));
                    live.layer_queue
                        .extend(loads.into_iter().filter(|load| dirty.contains(&load.layer)));
                }
                if let Some(load) = live.layer_queue.pop_front()
                    && let Err(error) = live.mailbox.load_layer(
                        load.layer,
                        load.base,
                        load.budget,
                        &load.map_bytes,
                        &load.tileset_stem,
                    )
                {
                    live_error = Some(format!("live layer {}: {error}", load.layer));
                }
            } else if let Some([x, y]) = live.pending_camera.take() {
                if let Err(error) = live.mailbox.set_camera(live::to_raw(x), live::to_raw(y)) {
                    // Owed again: a camera the cart never heard would
                    // otherwise leave the picture where it was with
                    // nothing left to re-send it.
                    live.pending_camera = Some([x, y]);
                    live_error = Some(format!("live camera: {error}"));
                }
            }
            // Assigned even when it is `None`: a push that succeeded after
            // an earlier failure has to take the message off the toolbar.
            let keep_error = world_held_back && live_error.is_none();
            // `live_error.is_some()` as well as `acted`: a greeting's
            // re-push can fail on a tick that had nothing else to send.
            if (acted || live_error.is_some()) && !keep_error && live_error != self.live_error {
                self.live_error = live_error;
                changed = true;
            }
        }

        let Some(live) = self.live.as_mut() else {
            return LiveStep {
                changed,
                failure: None,
            };
        };
        let rows = live::rows_from(live.mailbox.entities());
        changed |= rows != live.rows;
        live.rows = rows;
        let camera_before = live.camera;
        live.camera = live
            .mailbox
            .camera()
            .map(|(x, y)| [live::from_raw(x), live::from_raw(y)]);
        changed |= live.camera != camera_before;
        // Cloning the `Arc` only -- the emu panel owns dropping the image
        // it replaces (see `LinkEndpoint::frame`).
        live.frame = live
            .endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        changed |= live.frame.as_ref().map(|(number, _)| *number) != frame_before;
        changed |= live.status != status_before;
        changed |= self.mirror_cart_state();
        // After the mirror, and outside its session borrow: settling a
        // save applies document ops and writes files.
        changed |= self.settle_cart_save();
        LiveStep {
            changed,
            failure: None,
        }
    }

    /// Fold what the cart reported this tick into the document: the
    /// selection it is showing, its rubber band, the gesture edges that
    /// bracket a drag, the rows it moved and the entities it despawned.
    /// Returns whether any of it changed something the panel draws.
    ///
    /// In Live the cart owns every interaction, so this is the only path
    /// that turns an edit into a document op -- and every op it applies
    /// goes through [`Self::apply_mirror_op`], which does NOT arm the
    /// world resend. Pushing the blob back for a move the cart just made
    /// would reload the world under the gesture that produced it.
    fn mirror_cart_state(&mut self) -> bool {
        let Some(live) = self.live.as_mut() else {
            return false;
        };
        if live.status != LiveStatus::Connected {
            return false;
        }
        let mut changed = false;

        // Gesture edges are read before the rows are folded and the ENDS
        // applied after, because the cart emits `Begin` ahead of the
        // frame's rows and `End` behind them: the rows of both frames
        // belong INSIDE the gesture (`wire.rs`, "Frames"). A `Begin` while
        // one is open NESTS (a Nudge mid-drag), so the edges drive a stack
        // and an `End` pops its own id wherever it sits; one whose `Begin`
        // this session never saw never reaches here at all -- the mailbox
        // drops it.
        let mut ended: Vec<u32> = Vec::new();
        for (kind, id) in live.mailbox.take_gestures() {
            match kind {
                GestureKind::Begin => live.begin_gesture(id),
                GestureKind::End => ended.push(id),
            }
        }
        // Drained even when they cannot be folded: a despawn held back
        // while a world blob is in flight would be applied against the
        // NEXT world's index map, which describes a different document.
        let removed = live.mailbox.take_removed();
        // Drained for the same reason, and because a queue nobody empties
        // is a queue that grows: the cart publishes a component's whole
        // new value on every change it makes, its own and the host's.
        let component_changes = live.mailbox.take_component_changes();
        let added = live.mailbox.take_added();
        // The two cart -> host blobs the host asks for. A blob of another
        // kind is dropped rather than left in the queue behind them --
        // nothing requests one, so one arriving is a cart the host does
        // not understand, and a queue nobody empties is a queue that
        // grows. The NEWEST answer per kind wins: a layer re-asked for
        // mid-transfer comes back twice, the second copy the later one.
        let mut snapshot: Option<Vec<u8>> = None;
        let mut layer_blobs: Vec<(u32, Vec<u8>)> = Vec::new();
        while let Some((kind, index, body)) = live.mailbox.take_blob() {
            match kind {
                CartBlobKind::Snapshot => snapshot = Some(body),
                CartBlobKind::Layer => {
                    layer_blobs.retain(|(slot, _)| *slot != index);
                    layer_blobs.push((index, body));
                }
                CartBlobKind::Entity | CartBlobKind::Component => {}
            }
        }
        let mut snapshot_failed = false;
        let mut layer_failed = false;
        while let Some((kind, _)) = live.mailbox.take_failed_blob() {
            snapshot_failed |= kind == CartBlobKind::Snapshot;
            layer_failed |= kind == CartBlobKind::Layer;
        }
        // A save in flight collects the bytes here rather than draining
        // the queue a second time; `settle_cart_save` (one level up, out
        // of this borrow) is what turns a complete set into a write.
        if let Some(save) = live.saving.as_mut() {
            save.note_blobs(
                snapshot.as_deref(),
                &layer_blobs,
                snapshot_failed,
                layer_failed,
            );
        }

        let marquee = live.mailbox.marquee().map(|band| band.map(f64::from));
        if marquee != live.marquee {
            live.marquee = marquee;
            changed = true;
        }

        // The cart's rows only describe the open document while the world
        // handshake is settled: `world_sync` says the blob that went out
        // has been drawn, and `world_dirty` says another one is still
        // owed -- which is the window a mirrored despawn opens, where the
        // document's indices have shifted and the cart's have not.
        let synced = live.loaded() && !live.world_dirty;
        // Play is the game's: it moves its own entities, and folding that
        // would rewrite the document from a play-through.
        let fold = synced && live.mode == EditorMode::Edit;
        if !fold {
            live.mirror_rows.clear();
        }

        // Only on a change: the cart republishes the same selection after
        // every greeting, and re-applying it would fight nothing but cost
        // a document clone per frame.
        let selection = (synced && live.mailbox.selection() != live.cart_selection).then(|| {
            live.cart_selection = live.mailbox.selection().to_vec();
            let mut mapped: Vec<Selection> = Vec::new();
            for index in &live.cart_selection {
                if let Some(target) = live.index_map.selection_of(*index)
                    && !mapped.contains(&target)
                {
                    mapped.push(target);
                }
            }
            mapped
        });

        // One entry per document item whose row MOVED since the last tick,
        // in cart order: what it stands for, where it is now and where the
        // mirror last saw it. Movement, not disagreement, is what the
        // mirror folds -- a row that merely differs from the document is
        // the host's own edit still on its way to the cart, and re-writing
        // the document from it would undo that edit every frame.
        //
        // An instance's members all name the same document item, so only
        // the FIRST -- the primary -- speaks for it, and a row the mirror
        // has no baseline for (the first tick of a freshly loaded world)
        // is only recorded.
        let mut rows: Vec<(Selection, [f64; 2], [f64; 2])> = Vec::new();
        let mut removed_entities: Vec<usize> = Vec::new();
        // Cart indices whose row arrived exactly where an undo's replayed
        // `SetTransform` put it, retired once seen.
        let mut replays_landed: Vec<u32> = Vec::new();
        if fold {
            for row in &live.rows {
                let Some(target) = live.index_map.selection_of(row.index) else {
                    continue;
                };
                if rows.iter().any(|(seen, _, _)| *seen == target) {
                    continue;
                }
                let Some(before) = live
                    .mirror_rows
                    .iter()
                    .find(|previous| previous.index == row.index)
                    .map(|previous| [previous.x, previous.y])
                else {
                    continue;
                };
                if before == [row.x, row.y] {
                    continue;
                }
                // A row that MOVED and had a replay owed for it is that
                // replay landing -- the host's own undo coming back, not
                // cart-side motion: folding it would write the undone
                // delta into the document a second time and open an undo
                // entry for the undo. An unmoved row answers nothing yet,
                // which is why this is asked after the movement test and
                // not before it: the cart applies a transform on the frame
                // AFTER it takes it, and retiring the entry on the tick it
                // went out would leave the answer to fold as motion.
                //
                // The position is compared in RAW units -- what went out
                // and what the cart stored, since a world pixel is not a
                // value the wire's fixed point can always name exactly.
                // Landing somewhere else is the cart having adjusted the
                // position (a snap, an edit system that moved it on),
                // which folds like any other motion; either way the entry
                // is retired, because holding it would suppress an honest
                // fold at that exact position for the rest of the session.
                let replayed = live
                    .replayed_rows
                    .iter()
                    .find(|(index, _, _)| *index == row.index)
                    .copied();
                if let Some(replayed) = replayed {
                    replays_landed.push(row.index);
                    if replayed == (row.index, live::to_raw(row.x), live::to_raw(row.y)) {
                        continue;
                    }
                }
                rows.push((target, [row.x, row.y], before));
            }
            live.replayed_rows
                .retain(|(index, _, _)| !replays_landed.contains(index));
            live.mirror_rows = live.rows.clone();
            removed_entities = removed
                .iter()
                .filter_map(|index| match live.index_map.selection_of(*index) {
                    Some(Selection::Entity(entity)) => Some(entity),
                    // A despawned instance MEMBER leaves the `[[instance]]`
                    // alone: the document owns the whole subtree through
                    // the instanced world file, and half an instance is not
                    // something it can express.
                    _ => None,
                })
                .collect();
        }
        // What the cart says each changed component now holds, decoded
        // against ITS schema table -- the only one whose field hashes the
        // bags were written with. Every entry is a whole component value,
        // not a diff, and the ops below drop the fields that already read
        // that way in the document (the cart echoes the host's own
        // `set_component` back once, and a Transform lands here as well as
        // in the rows).
        let mut component_reports: Vec<(usize, String, serde_json::Map<String, Value>)> =
            Vec::new();
        if fold {
            for (index, bag) in &component_changes {
                // An instance member's components belong to the instanced
                // world file, which the document does not edit through
                // this world: only its transform mirrors, and the rows
                // above are where that happens.
                let Some(Selection::Entity(entity)) = live.index_map.selection_of(*index) else {
                    continue;
                };
                let Some((component, fields)) = bags::fields_from_bag(live.mailbox.schemas(), bag)
                else {
                    continue;
                };
                component_reports.push((entity, component, fields));
            }
        }

        // Every entity the cart numbered since the last tick. One answers
        // a spawn the host asked for; anything else is a cart-side spawn
        // whose components only a snapshot can tell the document.
        let mut resend_after_spawn = false;
        let mut ask_for_snapshot = false;
        for index in added {
            match live.claim_added(index) {
                // The cart hands out one past its highest tracked index
                // while the document appended at its own end. They agree
                // in the world this replay is allowed in ([`plan_replay`]
                // refuses an instanced one), but a cart whose own systems
                // hold indices the document never had can still disagree
                // -- and then only a reload puts the two flattenings back
                // together.
                live::AddedEntity::Spawned { doc_entity, index } => {
                    resend_after_spawn |= index as usize != doc_entity;
                }
                live::AddedEntity::Echo => {}
                live::AddedEntity::Unknown(index) => {
                    // Gated on `fold`, like the rows: in Play the entities
                    // are the GAME's, and an `EntityAdded` nobody asked for
                    // is reachable there -- the cart's own Duplicate
                    // command runs in either mode. An entity a
                    // play-through spawned is not a document entity, so the
                    // report is dropped rather than folded in.
                    //
                    // Only this arm is gated. A spawn the HOST asked for is
                    // still claimed: dropping its answer would leave the
                    // pending spawn to expire and take the optimistic
                    // entity back out from under a cart that did create it.
                    if !fold {
                        continue;
                    }
                    if !live.added_unknown.contains(&index) {
                        live.added_unknown.push(index);
                    }
                    ask_for_snapshot = true;
                }
            }
        }
        if resend_after_spawn {
            live.resend_world();
        }
        if ask_for_snapshot && !live.snapshot_pending {
            match live.mailbox.request_snapshot() {
                Ok(()) => live.snapshot_pending = true,
                // Nothing else re-arms it, so the entity the cart added
                // stays outside the document until the world is re-sent.
                Err(error) => {
                    log::warn!("GGO: live snapshot: {error}");
                    live.added_unknown.clear();
                    live.resend_world();
                }
            }
        }
        // The bags of every cart-side spawn this snapshot answers for,
        // in the order the cart numbered them.
        let mut snapshot_adds: Vec<(u32, serde_json::Map<String, Value>)> = Vec::new();
        if let Some(body) = snapshot {
            live.snapshot_pending = false;
            let wanted = std::mem::take(&mut live.added_unknown);
            let parsed = emerald_editor_link::parse_snapshot(&body).unwrap_or_default();
            for index in wanted {
                let Some((_, bags)) = parsed.iter().find(|(slot, _)| *slot == index) else {
                    continue;
                };
                let mut components = serde_json::Map::new();
                for bag in bags {
                    if let Some((name, fields)) = bags::fields_from_bag(live.mailbox.schemas(), bag)
                    {
                        components.insert(name, Value::Object(fields));
                    }
                }
                snapshot_adds.push((index, components));
            }
        } else if snapshot_failed && live.snapshot_pending {
            // The bytes are not coming. Asking again would race the same
            // failure; the world blob describes the whole table instead.
            live.snapshot_pending = false;
            live.added_unknown.clear();
            live.resend_world();
        }
        // Spawns the cart never answered: the protocol has no refusal
        // edge, so the budget running out IS the refusal.
        let expired_spawns = live.expire_spawns();

        // Motion nobody asked for -- a user edit system animating an
        // entity -- gets a synthetic gesture for as long as it lasts, so a
        // burst is one undo entry rather than one per cart frame.
        live.track_auto_gesture(!rows.is_empty());
        // Every op this tick folds shares one gesture id, so the store
        // amends a single undo entry for the whole cart-side drag.
        let gesture = live.gesture_tag();
        // Applied after the tag: the cart emits `End` behind the frame's
        // rows, so those rows are still the closing gesture's.
        for id in ended {
            live.end_gesture(id);
        }

        // Nothing to fold: the document is not cloned for a tick that has
        // no report to apply, and a connected cart publishes a frame of
        // unchanged rows every 16 ms.
        if selection.is_none()
            && rows.is_empty()
            && removed_entities.is_empty()
            && component_reports.is_empty()
            && snapshot_adds.is_empty()
            && expired_spawns.is_empty()
        {
            return changed;
        }

        let state = self.store.state();
        if let Some(mut mapped) = selection {
            let counts = OpenWorld::doc_counts(&state);
            mapped.retain(|target| counts.contains(*target));
            if mapped != self.selected {
                self.selected = mapped;
                changed = true;
            }
        }

        let mut moves: Vec<(Selection, [f64; 2])> = Vec::new();
        for (target, pos, before) in rows {
            match target {
                // A direct entity's row IS its `Transform.pos`, so the
                // document simply takes the position the cart published.
                Selection::Entity(index) => {
                    if let Some(current) = inspector::entity_pos(&state, index)
                        && current != pos
                    {
                        moves.push((target, pos));
                    }
                }
                // An instance member has no position of its own in the
                // document -- only the `[[instance]]` does -- so what moves
                // the instance is how far its primary member travelled.
                Selection::Instance(index) => {
                    let Some(instance) = state.instances.get(index) else {
                        continue;
                    };
                    let delta = [pos[0] - before[0], pos[1] - before[1]];
                    moves.push((
                        target,
                        [instance.pos[0] + delta[0], instance.pos[1] + delta[1]],
                    ));
                }
            }
        }
        let moved = match moves.len() {
            0 => None,
            // A group moves as ONE op: the store seals a gesture whenever
            // the target changes, so a member-per-op drag would land in as
            // many undo entries as it moved items.
            1 => moves.pop().map(|(target, pos)| match target {
                Selection::Entity(entity) => WorldOp::MoveEntity {
                    entity,
                    pos,
                    gesture,
                },
                Selection::Instance(index) => WorldOp::MoveInstance {
                    index,
                    pos,
                    gesture,
                },
            }),
            _ => Some(WorldOp::MoveMany { moves, gesture }),
        };
        if let Some(op) = moved {
            self.apply_mirror_op(op);
            changed = true;
        }

        if !component_reports.is_empty() {
            // Against the document as the row fold just LEFT it, not the
            // clone taken above: a `Transform` bag arrives alongside the
            // row that already moved the entity, and re-reading is what
            // makes that report a no-op instead of an undo entry of its
            // own. The cart's own echo of a host `set_component` lands the
            // same way.
            let now = self.store.state();
            let mut ops: Vec<WorldOp> = Vec::new();
            for (entity, component, fields) in component_reports {
                let Some(held) = now.entities.get(entity) else {
                    continue;
                };
                let Some(held) = held.components.get(&component).and_then(Value::as_object) else {
                    // A component the document's entity does not hold at
                    // all: one of the cart's own edit systems inserted it.
                    ops.push(WorldOp::AddComponent {
                        entity,
                        name: component,
                        defaults: fields,
                    });
                    continue;
                };
                for (field, value) in fields {
                    // `SetField` writes over an existing key only, so a
                    // field the document's copy does not name is dropped
                    // rather than silently lost inside a no-op op -- the
                    // two sides' schemas can differ by a cart rebuild.
                    let Some(current) = held.get(&field) else {
                        continue;
                    };
                    if live::same_value(current, &value) {
                        continue;
                    }
                    ops.push(WorldOp::SetField {
                        entity,
                        component: component.clone(),
                        field,
                        value,
                    });
                }
            }
            // One undo entry for the whole tick's report, the way a group
            // move folds into one.
            if let Some(op) = Self::one_step(ops) {
                self.apply_mirror_op(op);
                changed = true;
            }
        }

        // A spawn the cart never answered. There is no refusal edge on the
        // wire, so the budget running out IS the refusal, and the
        // optimistic entity comes back out -- highest first, so the
        // indices below it do not shift under the next removal.
        if !expired_spawns.is_empty() {
            let entities = self.store.state().entities.len();
            let ops: Vec<WorldOp> = expired_spawns
                .iter()
                .filter(|entity| **entity < entities)
                .map(|entity| WorldOp::RemoveEntity { index: *entity })
                .collect();
            if let Some(op) = Self::one_step(ops) {
                self.apply_mirror_op(op);
                self.prune_selection();
                // A mirrored op never goes through `note_doc_stepped`, so
                // the entity count it just changed has to re-flatten the
                // map here.
                self.rebuild_index_map();
            }
            self.live_error = Some("add entity refused by the cart".to_string());
            changed = true;
        }

        for (index, components) in snapshot_adds {
            self.apply_mirror_op(WorldOp::AddEntity { components });
            // The document appends; the cart numbered the entity itself.
            // When those disagree the flattenings do too, and only a
            // reload makes them describe each other again.
            let appended = self.store.state().entities.len().saturating_sub(1);
            if index as usize != appended
                && let Some(live) = self.live.as_mut()
            {
                live.resend_world();
            }
            // Same as above: without this the entity the cart added has no
            // cart index on this side, and every row it publishes for it is
            // dropped.
            self.rebuild_index_map();
            changed = true;
        }

        removed_entities.sort_unstable();
        removed_entities.dedup();
        let entities = state.entities.len();
        // Descending, and one op: a `RemoveEntity` shifts every index above
        // it, so the highest goes first and the whole despawn undoes in a
        // single step.
        let removals: Vec<WorldOp> = removed_entities
            .iter()
            .rev()
            .filter(|index| **index < entities)
            .map(|index| WorldOp::RemoveEntity { index: *index })
            .collect();
        let removal = match removals.len() {
            0 => None,
            1 => removals.into_iter().next(),
            _ => Some(WorldOp::Batch(removals)),
        };
        if let Some(op) = removal {
            self.apply_mirror_op(op);
            self.prune_selection();
            // The one mirrored edit that DOES owe the cart a blob: removing
            // a document entity shifts every index above it, while the
            // cart's own table keeps the despawned slot empty. The two
            // flattenings no longer describe each other, and only a reload
            // makes them agree again.
            self.note_doc_changed();
            changed = true;
        }
        changed
    }

    /// One op for a list the store has to undo in a single step. A single
    /// op is passed through as itself: `Batch` seals whatever gesture is
    /// open, and a lone mirrored op has no reason to.
    fn one_step(mut ops: Vec<WorldOp>) -> Option<WorldOp> {
        match ops.len() {
            0 => None,
            1 => ops.pop(),
            _ => Some(WorldOp::Batch(ops)),
        }
    }

    /// Apply one op the CART reported to the document, without the world
    /// resend [`Self::note_doc_changed`] arms: the cart is already showing
    /// this edit, and pushing the blob back would reload the world under
    /// the gesture that produced it.
    fn apply_mirror_op(&mut self, op: WorldOp) {
        self.store.apply(op);
        // The document DID move, even though the cart is not being told:
        // the failed-encode backoff reads this counter to tell a document
        // that has changed since its encode was refused from one that has
        // not.
        self.doc_generation = self.doc_generation.wrapping_add(1);
    }

    /// `to_doc()` -> `write_world` -> `mark_saved`, then the same for every
    /// dirty paint session. Synchronous by choice: world files are small
    /// TOML, and the async save ggo-ide uses is an iced task-architecture
    /// artifact, not an op-flow semantic (writing then `mark_saved` in one
    /// step also avoids the marked-depth race a mid-flight edit would
    /// cause).
    ///
    /// Every failure is attempted past, so one unwritable `.map` cannot
    /// strand the others, but the FIRST error is what `save_error` reports
    /// -- the world write's if it failed, since a document whose own TOML
    /// did not land is the more serious of the two. `carried` is a failure
    /// the caller already had (a `.map` the cart's cells could not be
    /// written into): reported only if these writes all succeed, for the
    /// same reason.
    fn write_now(&mut self, carried: Option<String>) {
        // `self.root`, NOT the worktree root: the doc must be written
        // back where it was read from (see the `OpenWorld::root` doc).
        let mut error = match write_world(&self.root, &self.listing.rel_path, &self.store.to_doc())
        {
            Ok(()) => {
                self.store.mark_saved();
                None
            }
            Err(e) => Some(e.to_string()),
        };
        let mut written: Vec<String> = Vec::new();
        for session in self.sessions.values_mut() {
            // Clean sessions are skipped rather than rewritten: an
            // untouched `.map` has nothing to write, and touching it would
            // wake the emu panel's watch-mode repack over no edit at all.
            if !session.dirty() {
                continue;
            }
            match session.save() {
                Ok(()) => written.push(session.rel_path.clone()),
                Err(e) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                }
            }
        }
        // A live session reads its layer payloads from DISK, and a paint
        // session holds its cells in memory until it is saved -- so the
        // write is the first moment a stroke can reach the cart, and
        // nothing else re-arms the slots it touched.
        if !written.is_empty() {
            let slots: Vec<u8> = self
                .merged
                .iter()
                .filter(|background| {
                    written
                        .iter()
                        .any(|rel| *rel == format!("{}.map", background.stem))
                })
                .map(|background| background.layer)
                .collect();
            if let Some(live) = self.live.as_mut() {
                for slot in slots {
                    live.layers_dirty.mark(slot);
                }
            }
            // A `Tilemap` ENTITY's map is not a cart layer: it rides the
            // world blob, so its save is a document change.
            self.note_doc_changed();
        }
        self.save_error = error.or(carried);
        self.settle_save_waiters();
    }

    /// Whether this world's save is waiting on the cart's answer.
    fn save_pending(&self) -> bool {
        self.live.as_ref().is_some_and(|live| live.saving.is_some())
    }

    /// Hand every caller waiting on a save the result `save_error` now
    /// says it had. A waiter is resolved exactly once: the receiver is
    /// gone with the sender.
    fn settle_save_waiters(&mut self) {
        let result = match &self.save_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        };
        for waiter in std::mem::take(&mut self.save_waiters) {
            // The receiver is dropped whenever the caller stopped caring
            // (the tab closed under the save), which is not a failure of
            // this save.
            waiter.send(result.clone()).ok();
        }
    }

    /// Fail a save outright: the file is left exactly as it was, the
    /// document stays dirty, and the reason goes on the status row.
    fn fail_save(&mut self, reason: String) {
        self.save_error = Some(reason);
        self.settle_save_waiters();
    }

    /// Turn a cart save that has all its bytes -- or has run out of time
    /// or of transfers -- into a write. Returns whether anything the panel
    /// draws moved.
    ///
    /// Runs after the mirror, on the tick the last blob lands: the mirror
    /// may have folded a report out of the very same frame, and the
    /// snapshot is what settles the disagreement in the cart's favour.
    fn settle_cart_save(&mut self) -> bool {
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        let Some(save) = live.saving.as_ref() else {
            return false;
        };
        let outcome = if let Some(reason) = save.failure() {
            Err(reason.to_string())
        } else if !live.loaded() || live.world_dirty {
            // The cart's indices stopped describing this document part-way
            // through: whatever it is about to hand back is the previous
            // world's, and folding it in would write one world's entities
            // over another's.
            Err("the world was re-sent to the cart mid-save".to_string())
        } else if save.timed_out(live.mailbox.frame_seq()) {
            Err("the cart did not answer in time".to_string())
        } else {
            match save.decode(live.mailbox.schemas()) {
                Some(decoded) => decoded,
                None => return false,
            }
        };
        if let Some(live) = self.live.as_mut() {
            live.saving = None;
        }
        match outcome {
            Ok(cart) => {
                self.fold_cart_entities(&cart);
                let carried = self.write_cart_layers(&cart);
                self.write_now(carried);
            }
            Err(reason) => self.fail_save(reason),
        }
        true
    }

    /// Rebuild the document's DIRECT entities from the cart's snapshot.
    ///
    /// Instance members are skipped: their components live in the
    /// instanced world file, which this document does not edit (only the
    /// `[[instance]]`'s own position is the document's, and the row mirror
    /// already keeps that). A cart index the flattening does not name at
    /// all is an entity one of the cart's own systems spawned, and it is
    /// APPENDED.
    ///
    /// Nothing is ever REMOVED here. A component the schema table cannot
    /// convert (one with a `Struct` field) decodes to nothing at all, so
    /// "the snapshot did not mention it" cannot be told from "the cart no
    /// longer has it" -- and dropping the document's own value on that
    /// guess would lose it on every save.
    fn fold_cart_entities(&mut self, cart: &live::CartSave) {
        let state = self.store.state();
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let mut ops: Vec<WorldOp> = Vec::new();
        let mut appended: Vec<&Vec<(String, serde_json::Map<String, Value>)>> = Vec::new();
        for (index, components) in &cart.entities {
            let entity = match live.index_map.selection_of(*index) {
                Some(Selection::Entity(entity)) => entity,
                Some(Selection::Instance(_)) => continue,
                None => {
                    appended.push(components);
                    continue;
                }
            };
            let Some(held) = state.entities.get(entity) else {
                continue;
            };
            for (component, fields) in components {
                let Some(current) = held.components.get(component).and_then(Value::as_object)
                else {
                    ops.push(WorldOp::AddComponent {
                        entity,
                        name: component.clone(),
                        defaults: fields.clone(),
                    });
                    continue;
                };
                for (field, value) in fields {
                    // `SetField` writes over an existing key only, the same
                    // rule the per-frame component mirror follows: the two
                    // sides' schemas can differ by a cart rebuild, and a
                    // field the document does not name is not this save's
                    // to invent.
                    let Some(held) = current.get(field) else {
                        continue;
                    };
                    if live::same_value(held, value) {
                        continue;
                    }
                    ops.push(WorldOp::SetField {
                        entity,
                        component: component.clone(),
                        field: field.clone(),
                        value: value.clone(),
                    });
                }
            }
        }
        // After every edit to the entities that already exist: an append
        // shifts nothing, but planning against a document that had already
        // grown would read the new entity's index as an old one's.
        for components in appended {
            let components = components
                .iter()
                .map(|(name, fields)| (name.clone(), Value::Object(fields.clone())))
                .collect();
            ops.push(WorldOp::AddEntity { components });
        }
        // One undo entry for the whole hand-back, the way a tick's worth
        // of mirrored reports folds into one.
        if let Some(op) = Self::one_step(ops) {
            self.apply_mirror_op(op);
        }
    }

    /// Write the cart's layer cells back into the `.map` files behind the
    /// background slots. Returns the first failure, if any.
    ///
    /// A slot the cart answered `0 x 0` for has NO map on the cart, which
    /// is not the same as an empty one: the file it would have come from
    /// is left exactly as it is. The cells are written into the top-left
    /// of whatever the document's map is, because the dimensions the cart
    /// reports are its own shadow's -- what it clamped the map to, which
    /// can be smaller than the file.
    fn write_cart_layers(&mut self, cart: &live::CartSave) -> Option<String> {
        let mut error: Option<String> = None;
        for (slot, w, h, cells) in &cart.layers {
            if *w == 0 || *h == 0 {
                continue;
            }
            let Some(background) = self.merged.iter().find(|merged| merged.layer == *slot) else {
                continue;
            };
            let rel = format!("{}.map", background.stem);
            // A session is the map's live document (undo history and all),
            // so its cells go through an op and the ordinary session save
            // writes them. Only a map with no session open is patched on
            // disk here.
            let patched = match self.sessions.get_mut(&rel) {
                Some(session) => {
                    // A DIRTY session holds a stroke the cart has never
                    // been told about: the panel ships a painted slot as a
                    // whole `load_layer` on save, so until the stroke has
                    // been written the cart's shadow is the PRE-stroke map
                    // and folding it in would erase the user's paint. The
                    // stroke reaches the cart a moment later, out of
                    // `write_now`'s re-arm of the slot.
                    if session.dirty() {
                        continue;
                    }
                    let state = session.store.state();
                    let writes = cell_writes(&state.cells, state.w, state.h, *w, *h, cells);
                    if !writes.is_empty() {
                        session.apply(ggo_worldlib::sprites::map_doc::MapOp::SetCells(writes));
                    }
                    continue;
                }
                None => patch_map_on_disk(&self.root, &rel, *w, *h, cells),
            };
            if let Err(reason) = patched
                && error.is_none()
            {
                error = Some(reason);
            }
        }
        error
    }

    /// The current position of every selected item that still exists.
    fn selected_positions(&self) -> Vec<(Selection, [f64; 2])> {
        let state = self.store.state();
        self.selected
            .iter()
            .filter_map(|target| {
                let pos = match *target {
                    Selection::Entity(i) => inspector::entity_pos(&state, i),
                    Selection::Instance(i) => state.instances.get(i).map(|inst| inst.pos),
                }?;
                Some((*target, pos))
            })
            .collect()
    }
}

pub struct WorldPanel {
    focus_handle: FocusHandle,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    worlds: Vec<WorldListing>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    /// Runs `emd pack-ggo` for the popout Emulate; swapped for a fake in
    /// tests, same seam as `ggo_emu_panel`'s.
    proc_runner: ggo_common::ProcRunner,
    /// Launches the standalone `ggo-emu` on the built cartridge.
    emu_launcher: ggo_common::DetachedLauncher,
    _popout_task: Option<Task<()>>,
    /// Images the image cache no longer holds, waiting to be handed back
    /// to the window atlas -- two-stage like the emu pane's frame buffer
    /// (see that crate's module doc: gpui never frees atlas tiles on its
    /// own). `retired_images` is this render's batch; a render moves it
    /// to `retired_previous` and drops what was there.
    retired_images: Vec<Arc<RenderImage>>,
    retired_previous: Vec<Arc<RenderImage>>,
    /// Copied CELLS. Panel-level (not on `OpenWorld`, and not on a
    /// session) so it survives switching paint targets and reopening the
    /// mode, mirroring the retired map panel's own clipboard. Separate from
    /// the system clipboard the world's entity copy uses: a `Stamp` has no
    /// text form, and pasting cells into an editor tab would be nonsense.
    cell_clipboard: Option<Stamp>,
    /// Which renderer the canvas draws. Sticky across worlds within a
    /// session: a fallback to Design is what turns Live off, and the user
    /// turning it back on is what turns it on again.
    canvas_mode: CanvasMode,
    /// Which tool the user picked, sticky for the same reason
    /// [`Self::canvas_mode`] is: leaving Live to look at the design view
    /// should not silently hand the pointer back to the select tool.
    /// Seeded into every new [`LiveView`] by [`Self::start_live`].
    live_tool: u8,
    /// The link to the viewer cart THIS panel booted. One per panel, so
    /// each world tab drives its own viewer run: sharing one cart between
    /// tabs would make whichever world was greeted last the only one the
    /// cart is showing.
    live_endpoint: Option<Arc<ggo_common::LinkEndpoint>>,
    /// Set between [`Self::enter_live`] arming its deferred boot and that
    /// boot landing. The canvas has to know: for those renders the mode
    /// is Live and there is no session yet, and painting the design
    /// renderer through them flashes the wrong picture at the user.
    live_boot_pending: bool,
    /// The dock this panel's tab was opened from, where the sticky Live
    /// choices live. Held so the user's choices survive closing the last
    /// world tab, which takes every panel with it.
    dock: Option<WeakEntity<WorldDock>>,
}

impl WorldPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        // The last chance to hand the image cache's atlas tiles back: no
        // render follows a release. Same hook the emu pane uses.
        cx.on_release(|this, cx| {
            // The viewer cart outlives nothing here: with the panel gone
            // there is no one left to poll the link.
            if let Some(endpoint) = this.live_endpoint.take() {
                endpoint.request_stop();
            }
            this.retire_open_images();
            for image in this
                .retired_previous
                .drain(..)
                .chain(this.retired_images.drain(..))
            {
                cx.drop_image(image, None);
            }
        })
        .detach();
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            root_override: None,
            project_root: None,
            worlds: Vec::new(),
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
            proc_runner: ggo_common::system_proc_runner(),
            emu_launcher: ggo_common::system_detached_launcher(),
            _popout_task: None,
            retired_images: Vec::new(),
            retired_previous: Vec::new(),
            cell_clipboard: None,
            canvas_mode: CanvasMode::Live,
            live_tool: 0,
            live_endpoint: None,
            live_boot_pending: false,
            dock: None,
        }
    }

    /// Which renderer this panel's canvas is showing.
    pub fn canvas_mode(&self) -> CanvasMode {
        self.canvas_mode
    }

    /// Adopt the dock that opened this tab, so the sticky Live choices
    /// this panel makes outlive it.
    pub(crate) fn set_dock(&mut self, dock: WeakEntity<WorldDock>) {
        self.dock = Some(dock);
    }

    /// Hand the user's Live choices back to the dock, which is what seeds
    /// the next world tab with them. Called only by the paths the USER
    /// drives; a fallback to Design leaves the sticky choice alone.
    fn note_sticky_live(&self, cx: &mut App) {
        let (mode, tool) = (self.canvas_mode, self.live_tool);
        if let Some(dock) = self.dock.as_ref() {
            dock.update(cx, |dock, _| dock.note_sticky(mode, tool)).ok();
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
        // Audio sizes are re-derived on the next render: a file imported
        // since the panel was last shown gets its new size.
        if let ViewerState::Ready(open) = &mut self.state {
            open.audio_sizes.clear();
            // Same reason: the stem feed is otherwise only rebuilt when
            // the focused field changes.
            open.stem_completion = None;
        }
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
    /// document's derived asset root while one is loaded, else
    /// `<worktree>/assets` when that is where the worlds live, else the
    /// worktree root.
    fn asset_root(&self) -> Option<PathBuf> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.root.clone()),
            _ => {
                let project_root = self.project_root.clone()?;
                // No clicked path has named a root yet, so apply the rule
                // [`split_world_path`] would have applied to a click on
                // `assets/worlds/x.toml`. Without it a project with the
                // usual asset-root layout enumerates NO worlds until a
                // human opens one by hand -- which is exactly the state
                // `remote_list` answers in.
                let assets = project_root.join("assets");
                Some(if assets.join("worlds").is_dir() {
                    assets
                } else {
                    project_root
                })
            }
        }
    }

    /// Load the project-relative world path `rel`, prompting FIRST if the
    /// open world has unsaved edits -- Cancel leaves the current document
    /// loaded and dirty and abandons the open.
    ///
    /// Callers outside this crate open worlds through
    /// [`WorldDock::open_world`], not through here: the dock's tab list is
    /// keyed on the rel it opened each panel for, so a world loaded into a
    /// panel behind the dock's back would leave that key stale and get a
    /// second tab on the next open. The dock itself calls this once, on
    /// the fresh panel it just made for `rel`.
    ///
    /// Everything after the guard runs on a spawned task, deliberately: the
    /// interceptor calls this from INSIDE the workspace's own update, and
    /// [`Self::refresh_worlds`] has to read that same workspace entity.
    pub(crate) fn open_rel_path(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        cx.spawn_in(window, async move |this, cx| {
            if !proceed.await {
                return;
            }
            // `spawn_in` + `cx.update` (rather than `spawn` +
            // `update_in`): entering Live mode when the load lands needs
            // the `Window` this call came from -- the booter runs inside a
            // `Workspace` update -- and `update_in` cannot supply it,
            // because it resolves the window off the ENTITY, which is not
            // always the window's own (the headless tests build a panel
            // outside any window).
            cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    this.refresh_worlds(cx);
                    this.load_rel_path(&rel, Some(window), cx);
                })
                .ok();
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
                    this.retire_open_images();
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
    /// Re-read `rel` from disk, DISCARDING any unsaved edits -- the
    /// "Don't Save" answer to the tab's close prompt. Unlike
    /// `open_rel_path` this asks nothing: the user already answered.
    pub(crate) fn reload_from_disk(&mut self, rel: &str, cx: &mut Context<Self>) {
        self.load_rel_path(rel, None, cx);
    }

    /// Put a fresh panel into `Loading` for `rel` before its deferred
    /// load runs, so a reader that arrives first (the MCP host's
    /// `world_read`) waits instead of seeing an empty panel.
    pub(crate) fn mark_loading(&mut self, rel: &str, cx: &mut Context<Self>) {
        if let Some((_, listing)) = split_world_path(rel) {
            self.state = ViewerState::Loading { stem: listing.stem };
            cx.notify();
        }
    }

    /// A load that never started must not leave the panel in the
    /// `Loading` state [`Self::mark_loading`] put it in: `remote_read`
    /// waits on that state (see [`WORLD_STILL_LOADING`]) and would wait
    /// forever. Only touches a panel that IS waiting -- an abandoned load
    /// leaves an already-open world alone.
    fn abandon_load(&mut self, reason: String, cx: &mut Context<Self>) {
        if matches!(self.state, ViewerState::Loading { .. }) {
            self.state = ViewerState::Error(reason);
            cx.notify();
        }
    }

    /// `window` is what a completed load needs to enter Live mode (see
    /// [`Self::enter_live`]); a windowless load stays in Design whatever
    /// [`Self::canvas_mode`] says, because there is no window to boot a
    /// viewer cart in. Every tab now loads WITH a window -- the dock
    /// defers [`Self::open_rel_path`] into one -- so this is the headless
    /// tests' path and the reload behind a close prompt.
    fn load_rel_path(&mut self, rel: &str, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let Some((asset_root_rel, listing)) = split_world_path(rel) else {
            self.abandon_load(format!("{rel} is not a world file"), cx);
            return;
        };
        let Some(project_root) = self.project_root.clone() else {
            self.abandon_load(format!("no project root to load {rel} against"), cx);
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
        self.retire_open_images();
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
        let finish = move |this: &mut Self,
                           window: Option<&mut Window>,
                           cx: &mut Context<Self>,
                           result: Result<_, String>| {
            if this.load_generation != generation {
                return;
            }
            this.retire_open_images();
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
            if let Some(window) = window
                && this.canvas_mode == CanvasMode::Live
            {
                this.enter_live(window, cx);
            }
            cx.notify();
        };
        self._load_task = Some(match window {
            Some(window) => cx.spawn_in(window, async move |this, cx| {
                let result = load.await;
                cx.update(|window, cx| {
                    this.update(cx, |this, cx| finish(this, Some(window), cx, result))
                        .ok();
                })
                .ok();
            }),
            None => cx.spawn(async move |this, cx| {
                let result = load.await;
                this.update(cx, |this, cx| finish(this, None, cx, result))
                    .ok();
            }),
        });
    }

    // ------------------------------------------------------------ editing

    /// Apply one op to the open world's store and repaint. Every editor
    /// mutation funnels through here (or the drag/undo/redo paths, which
    /// notify themselves), so the draw list -- rebuilt per render --
    /// always reflects the store.
    fn apply_op(&mut self, op: WorldOp, cx: &mut Context<Self>) {
        // Play is the GAME's: the running cart has moved on from whatever
        // the document says, and every op here arms a resend that would
        // reload the play-through out from under it. Guarded centrally
        // rather than per caller -- the layers rail reached the store
        // through here with nothing in its way. The two paths that
        // deliberately bypass this one stay bypassed: the mirror
        // (`apply_mirror_op`, which is the cart reporting its own edit)
        // and Delete (forwarded to the cart, which ignores it in Play).
        if self.live_playing() {
            return;
        }
        if let ViewerState::Ready(open) = &mut self.state {
            let before = open.store.state();
            open.store.apply(op);
            open.note_doc_stepped(&before);
            cx.notify();
        }
        self.refresh_instance_counts(cx);
    }

    /// Toolbar "add entity": a fresh entity with just a Transform
    /// skeleton -- schema defaults (`defaults_for`, per the M7 brief;
    /// builtins guarantee a Transform schema) with `pos` overridden to
    /// the current view center, ggo-ide's `WorldMsg::AddEntity` -- then
    /// select it.
    fn add_entity_impl(&mut self, cx: &mut Context<Self>) {
        // The toolbar stays on screen in paint mode and in Play, so its
        // buttons are reachable: gate them like every other entity
        // mutation.
        if self.in_paint_mode() || self.live_playing() {
            return;
        }
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
        // Through the step diff, not `note_doc_changed`: in Live the new
        // entity is a `spawn` on the cart, and the document holds it
        // optimistically until the cart answers with the index it gave it.
        let before = open.store.state();
        open.store.apply(WorldOp::AddEntity { components });
        open.note_doc_stepped(&before);
        open.selected = vec![Selection::Entity(open.store.state().entities.len() - 1)];
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
        if self.in_paint_mode()
            || self.live_playing()
            || !self.instance_candidates().contains(&stem)
        {
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
        open.selected = vec![Selection::Instance(index)];
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
        open.note_doc_changed();
        Self::replace_images(open, &mut self.retired_images);
        cx.notify();
        // The instance LIST moved and this path never went through
        // `apply_op`, so the flattened counts the live index map is built
        // from have to be re-asked for here.
        self.refresh_instance_counts(cx);
    }

    /// Layers rail "add": bind layer `layer` of the OPEN world to
    /// `maps/<stem>.bg<layer>.map`, writing that map first if it is not
    /// on disk yet -- blank and already bound to the picked tileset, so
    /// the slot is paintable with no follow-up bind.
    ///
    /// The write is deliberately conditional. Undo unlinks the slot but
    /// (like every `SetBackground`) leaves the file alone, so redo -- and
    /// a deliberate re-add of a layer the user cleared earlier -- must
    /// re-LINK the existing map rather than blank whatever was painted
    /// into it.
    fn add_background_impl(&mut self, layer: u8, til_rel: String, cx: &mut Context<Self>) {
        /// Side of a freshly generated background map, in tiles -- the
        /// value the retired map editor's "New Map…" used (ggo-ide's
        /// `NEW_MAP_DEFAULT_DIM`) for the same "new map" idea.
        const NEW_BG_DIM: u16 = 16;

        // Guarded here as well as in `apply_op`: this one writes a `.map`
        // to disk before it ever reaches the store, and a layer push
        // behind the refused op would re-send the background to a cart
        // that is running the game with it already loaded.
        if self.live_playing() {
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let map_rel = background_map_rel(&open.listing.stem, layer);
        // Absent and present-but-unopenable are different answers, and
        // only the first one may be written: blanking a corrupt `.map`
        // would destroy exactly the painted work the re-link promise above
        // exists to keep. `io::IoError` carries no `io::ErrorKind`, so the
        // existence probe -- not the open error -- is the honest
        // discriminator; a file that exists but will not decode is
        // reported and left alone, slot unlinked.
        let map_full = open.root.join(&map_rel);
        let write_result = if map_full.exists() {
            io::open_map(&open.root, &map_rel).map(|_| ())
        } else {
            io::save_new_bound_map(&open.root, &map_rel, NEW_BG_DIM, NEW_BG_DIM, &til_rel)
        };
        if let Err(e) = write_result {
            open.save_error = Some(e.to_string());
            cx.notify();
            return;
        }
        self.apply_op(
            WorldOp::SetBackground {
                layer,
                map: Some(map_rel),
            },
            cx,
        );
        self.refresh_backgrounds(cx);
    }

    /// Layers rail "clear": unlink `layer`. Never deletes the `.map` --
    /// other worlds may share it, and unlink is undoable where deletion
    /// would not be (`WorldOp::SetBackground`'s own contract).
    fn clear_background_impl(&mut self, layer: u8, cx: &mut Context<Self>) {
        // As in `add_background_impl`: the refresh below is not an op and
        // would push the layer again on its own.
        if self.live_playing() {
            return;
        }
        self.apply_op(WorldOp::SetBackground { layer, map: None }, cx);
        self.refresh_backgrounds(cx);
    }

    /// Re-run the background merge and refresh what the canvas paints
    /// from it. Every path that can change a `[[background]]` slot --
    /// add, clear, undo, redo -- ends here, because `merged` (not the
    /// document's own slot list) is what the draw list reads, and a newly
    /// linked stem has no composed image until it is asked for.
    fn refresh_backgrounds(&mut self, cx: &mut Context<Self>) {
        // A slot change can pull the map out from under paint mode (a
        // clear, or an undo of the add that linked it). Fall back to
        // entity editing rather than sit in a mode whose target no longer
        // exists -- one that would swallow the next undo without being
        // able to act on it.
        if self.in_paint_mode() && self.active_paint_target().is_none() {
            self.exit_paint_mode(cx);
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.merged = loader::merged_backgrounds(&open.store.state(), &open.instance_backgrounds);
        loader::fill_missing_background_loads(&open.root, &open.merged, &mut open.map_loads);
        if let Some(live) = open.live.as_mut() {
            live.layers_dirty.mark_all();
        }
        Self::replace_images(open, &mut self.retired_images);
        cx.notify();
    }

    /// The open document's own `[[background]]` slots (base world only --
    /// merged instance slots are not this document's to edit).
    fn backgrounds_now(&self) -> Vec<Background> {
        match &self.state {
            ViewerState::Ready(open) => open.store.state().backgrounds,
            _ => Vec::new(),
        }
    }

    // --------------------------------------------------------- paint mode

    /// The `.map` a paint target names and the world-space pixel its
    /// top-left cell sits at, or `None` when the target no longer resolves
    /// (slot cleared, entity deleted, `Tilemap` component removed).
    ///
    /// Resolved fresh on every use rather than captured at entry: an undo
    /// can unlink the slot out from under an active paint session, and a
    /// captured rel would keep painting a map the document no longer
    /// references.
    fn paint_target_rel(&self, target: &PaintTarget) -> Option<(String, [f64; 2])> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let state = open.store.state();
        match *target {
            PaintTarget::BgSlot(layer) => {
                let background = state.backgrounds.iter().find(|bg| bg.layer == layer)?;
                // Through `stem()`, so a slot written without the
                // extension resolves to the same rel as one with it.
                let stem = background.stem();
                (!stem.is_empty()).then(|| (format!("{stem}.map"), [0.0, 0.0]))
            }
            PaintTarget::TilemapEntity(index) => {
                let fields = tilemap_fields(state.entities.get(index)?)?;
                let stem = fields.get("stem")?.as_str()?;
                let pos = inspector::entity_pos(&state, index)?;
                // `render.rs::push_tilemap_item`'s arithmetic, which is
                // what puts the composed image on the canvas -- the cell
                // grid has to start where the pixels do.
                let field = |name: &str| fields.get(name).and_then(Value::as_f64).unwrap_or(0.0);
                let anchor = [
                    pos[0] + field("col") * TILE_PX as f64,
                    pos[1] + field("row") * TILE_PX as f64,
                ];
                Some((format!("{stem}.map"), anchor))
            }
        }
    }

    /// The active paint target's rel + anchor, `None` in entity mode.
    fn active_paint_target(&self) -> Option<(String, [f64; 2])> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let EditMode::Paint(target) = open.mode else {
            return None;
        };
        self.paint_target_rel(&target)
    }

    /// Paint mode is active, which the world editor's own mutations gate
    /// on: the canvas gestures, Escape, undo/redo and every
    /// entity-mutating action belong to the map under the brush while it
    /// is (spec: "entity manipulation is disabled" while painting).
    ///
    /// Gating each action rather than clearing `open.selected` on entry is
    /// deliberate: the selection has to survive the mode so that leaving
    /// it puts the user back exactly where they were. Which means the
    /// KEYBOARD paths need the gate too -- delete and the arrow keys reach
    /// a live selection with no canvas gesture involved.
    fn in_paint_mode(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.mode != EditMode::Entities)
    }

    /// Open `target` for painting: reuse the cached session if this world
    /// has already loaded that map, otherwise load it off-thread and enter
    /// when it lands. Returns whether the target resolved to a map at all
    /// -- `false` is an empty slot or a non-`Tilemap` entity, which is a
    /// refusal, not a failure.
    ///
    /// Entry is deliberately NOT optimistic: the mode flips only once
    /// there is a session behind it, so a missing or unreadable `.map`
    /// leaves the world editor exactly as it was, with the reason on the
    /// toolbar.
    fn enter_paint_mode(&mut self, target: PaintTarget, cx: &mut Context<Self>) -> bool {
        // Painting is an edit of the world the cart is running, and in
        // Play there is no world of the document's to edit.
        if self.live_playing() {
            return false;
        }
        let Some((rel, _)) = self.paint_target_rel(&target) else {
            return false;
        };
        let Some(project_root) = self.project_root.clone() else {
            // Same discipline as a failed load: a refusal the user cannot
            // see is a refusal they will retry forever.
            if let ViewerState::Ready(open) = &mut self.state {
                open.paint_error = Some("no project folder is open".to_string());
                cx.notify();
            }
            return false;
        };
        // A world load in flight (or since) invalidates this entry -- the
        // same guard `load_rel_path` uses, so a session cannot install
        // over a world it was never loaded for.
        let generation = self.load_generation;
        let ViewerState::Ready(open) = &mut self.state else {
            return false;
        };
        open.paint_error = None;
        if open.sessions.contains_key(&rel) {
            open.mode = EditMode::Paint(target);
            Self::end_canvas_gestures(open);
            // Unconditionally, not only on a fresh load: what the canvas
            // must show is the SESSION's pixels, and a re-entry follows an
            // arbitrary amount of editing the map's load entry may predate.
            self.refresh_paint_image(&rel, cx);
            return true;
        }

        let root = open.root.clone();
        let load = cx.background_spawn({
            let rel = rel.clone();
            async move { PaintSession::load(&root, &rel, &project_root) }
        });
        open._session_loading = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                // The target can die (slot cleared, the add undone, the
                // entity deleted) or be re-pointed at a DIFFERENT map
                // while the load is in flight. Either way the mode the
                // user asked for no longer exists, so the session is
                // installed -- it is a document, not a mode, and its rel
                // is still its rel -- but the panel stays in Entities.
                // Silently: the mode was withdrawn by the user's own
                // later edit, which is not an error to report.
                let still_wanted = this
                    .paint_target_rel(&target)
                    .is_some_and(|(now, _)| now == rel);
                let ViewerState::Ready(open) = &mut this.state else {
                    return;
                };
                let entered = match result {
                    Ok(session) => {
                        open.sessions.insert(rel.clone(), session);
                        if still_wanted {
                            open.mode = EditMode::Paint(target);
                            Self::end_canvas_gestures(open);
                        }
                        still_wanted
                    }
                    Err(e) => {
                        open.paint_error = Some(e);
                        false
                    }
                };
                if entered {
                    this.refresh_paint_image(&rel, cx);
                }
                cx.notify();
            })
            .ok();
        }));
        true
    }

    /// Back to entity editing. The sessions stay: their undo history and
    /// unsaved edits are the document's, not the mode's.
    fn exit_paint_mode(&mut self, cx: &mut Context<Self>) {
        // Leaving mid-stroke (Escape, or the slot cleared out from under
        // the brush) has to close the session's undo entry first: an
        // abandoned stroke stays open in the store, and the next ops --
        // a paste, a resize, the next gesture -- would fold into the
        // PREVIOUS gesture's entry. Before the mode flips, because
        // `end_paint_gesture` resolves its target through the mode.
        if matches!(&self.state, ViewerState::Ready(open) if open.paint_gesture) {
            self.end_canvas_paint(cx);
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.mode = EditMode::Entities;
        open.paint_error = None;
        cx.notify();
    }

    /// Drop any in-flight entity gesture -- entering paint mode mid-drag
    /// must not leave a marquee drawn or a placement drag armed to resume
    /// on the next move event.
    fn end_canvas_gestures(open: &mut OpenWorld) {
        open.edit_drag = None;
        open.marquee = None;
        open.nudge_gesture = None;
    }

    /// Recompose the session's live document into the map load map and the
    /// image cache, keyed by STEM -- so a map drawn in several places (a
    /// background slot and a `Tilemap` entity at once) updates everywhere
    /// from one compose.
    fn refresh_paint_image(&mut self, rel: &str, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // `None` is an unbound map: it has no pixels to show, and blanking
        // the load entry would replace the placeholder the canvas already
        // draws for it with nothing.
        let Some((rgba, w, h)) = open.sessions.get(rel).and_then(PaintSession::live_rgba) else {
            return;
        };
        open.map_loads.insert(
            map_stem(rel).to_string(),
            Loadable::Ready(RgbaImage {
                rgba: rgba.into(),
                w,
                h,
            }),
        );
        Self::replace_images(open, &mut self.retired_images);
        cx.notify();
    }

    /// Apply the active tool at canvas-relative `local` px. `press` starts
    /// a gesture, carrying the shift modifier the terrain tool reads as
    /// "erase" (read once, at the gesture's start, so a drag keeps
    /// erasing); `None` continues the gesture already in flight.
    fn paint_at_local(&mut self, local: [f64; 2], press: Option<bool>, cx: &mut Context<Self>) {
        let Some((rel, anchor)) = self.active_paint_target() else {
            return;
        };
        let Some(view) = self.canvas_view() else {
            return;
        };
        let world = drag_ops::screen_to_world(local[0], local[1], &view);
        let cell = canvas::paint_cell_at(world, anchor);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(session) = open.sessions.get_mut(&rel) else {
            return;
        };
        if let Some(shift) = press {
            // Everything one gesture paints folds into ONE undo entry, and
            // starts from no pending rect or selection.
            session.begin_gesture(shift);
        }
        // An inert gesture (unbound tileset, terrain tool with no terrain)
        // is skipped rather than repainted: `paint_at` would return false
        // anyway, and a notify per drag move buys nothing.
        if !session.can_paint() {
            return;
        }
        let painted = session.paint_at(cell);
        // Only a BACKGROUND slot is something the cart holds as a layer; a
        // `Tilemap` entity's map rides along with the world blob.
        if painted
            && let EditMode::Paint(PaintTarget::BgSlot(slot)) = open.mode
            && let Some(live) = open.live.as_mut()
        {
            live.layers_dirty.mark(slot);
        }
        if painted {
            self.refresh_paint_image(&rel, cx);
        } else {
            cx.notify();
        }
    }

    /// Disarm the canvas paint gesture and close its undo entry -- every
    /// way a stroke can end (mouse-up, a release this element never saw,
    /// the cursor leaving the canvas, leaving the mode) goes through here,
    /// so the flag and the store's open stroke can never disagree.
    fn end_canvas_paint(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.paint_gesture = false;
        }
        self.end_paint_gesture(cx);
    }

    /// Gesture release while painting: a rect-fill preview becomes its op,
    /// a select drag settles into the session's cell selection.
    fn end_paint_gesture(&mut self, cx: &mut Context<Self>) {
        let Some((rel, _)) = self.active_paint_target() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(session) = open.sessions.get_mut(&rel) else {
            return;
        };
        if session.end_gesture() {
            self.refresh_paint_image(&rel, cx);
        } else {
            cx.notify();
        }
    }

    /// Escape while painting: drop the cell selection if there is one,
    /// otherwise leave the mode -- the standalone editor's escape
    /// semantics, one level up (spec 2026-08-29).
    fn escape_paint_mode(&mut self, cx: &mut Context<Self>) {
        let cleared = self
            .active_paint_target()
            .and_then(|(rel, _)| match &mut self.state {
                ViewerState::Ready(open) => open.sessions.get_mut(&rel),
                _ => None,
            })
            .is_some_and(PaintSession::clear_selection);
        if cleared {
            cx.notify();
            return;
        }
        self.exit_paint_mode(cx);
    }

    /// Undo/redo while painting drives the TARGET MAP's history, not the
    /// world's (spec: undo is mode-scoped; there is no unified timeline).
    /// Returns whether paint mode consumed the step.
    fn step_paint_history(
        &mut self,
        step: fn(&mut MapDocStore) -> bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.in_paint_mode() {
            return false;
        }
        let project_root = self.project_root.clone();
        let Some((rel, _)) = self.active_paint_target() else {
            // The target died under us -- hand the step back to the world
            // store rather than swallow it into a dead mode.
            self.exit_paint_mode(cx);
            return false;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return true;
        };
        let stepped = open
            .sessions
            .get_mut(&rel)
            .is_some_and(|session| session.step_history(step, project_root.as_deref()));
        if stepped {
            self.refresh_paint_image(&rel, cx);
        }
        true
    }

    /// Stamp the cell clipboard down. Where it lands: the cell under the
    /// cursor when the pointer is over the canvas, else the selection's
    /// top-left, else the map's origin -- `ggo_map_panel`'s own paste
    /// rule, restated against the world camera.
    fn paste_cells(&mut self, cx: &mut Context<Self>) {
        let Some(stamp) = self.cell_clipboard.clone() else {
            return;
        };
        let Some(at) = self.paste_cell_origin() else {
            return;
        };
        self.update_paint_session(cx, |session| session.paste_stamp(stamp, at));
    }

    fn paste_cell_origin(&self) -> Option<(i32, i32)> {
        let (rel, anchor) = self.active_paint_target()?;
        let view = self.canvas_view();
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let hover = open.view.borrow().hover;
        let under_cursor = hover.zip(view).map(|(hover, view)| {
            let world = drag_ops::screen_to_world(hover[0], hover[1], &view);
            canvas::paint_cell_at(world, anchor)
        });
        Some(
            under_cursor
                .or_else(|| {
                    open.sessions
                        .get(&rel)
                        .and_then(|session| session.selection)
                        .map(|(x0, y0, _, _)| (x0, y0))
                })
                .unwrap_or((0, 0)),
        )
    }

    /// Create the paint column's two editor entities (the terrain name and
    /// the resize inputs) on the first paint-mode render, and keep the
    /// resize inputs in step with the target map afterwards. Switching
    /// targets re-seeds them through the same sync, so the fields always
    /// describe the map actually under the brush.
    fn ensure_paint_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dims) = self.paint_session().map(|session| {
            let state = session.store.state();
            (state.w, state.h)
        }) else {
            return;
        };
        let existing = match &self.state {
            ViewerState::Ready(open) => open.resize.clone(),
            _ => return,
        };
        let made = paint_ui::ensure_resize_fields(existing.as_ref(), dims.0, dims.1, window, cx);
        let name = match &self.state {
            ViewerState::Ready(open) => open.terrain_name.is_none(),
            _ => false,
        }
        .then(|| cx.new(|cx| Editor::single_line(window, cx)));
        if let ViewerState::Ready(open) = &mut self.state {
            if let Some(made) = made {
                open.resize = Some(made);
            }
            if let Some(name) = name {
                open.terrain_name = Some(name);
            }
        }
    }

    /// The image the paint target draws with, which the canvas holds at
    /// full strength while it dims the rest ([`canvas::Scene::paint_focus`]).
    fn paint_focus_key(&self) -> Option<usize> {
        let (rel, _) = self.active_paint_target()?;
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        match open.map_loads.get(map_stem(&rel)) {
            Some(Loadable::Ready(image)) => Some(canvas::image_key(image)),
            _ => None,
        }
    }

    /// Delete the selected entity or instance (toolbar button and the
    /// `DeleteSelected` action). Bounds-guarded: a selection gone stale
    /// against an undo/redo restructure is a no-op, not a panic.
    /// Delete the selection -- after a confirm when it holds instances
    /// (ggo-ide confirms instance removal; an entity delete is one undo
    /// away and gets no prompt).
    fn delete_selected_impl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Paint mode's delete blanks the selected CELLS -- never the
        // world's entities. With no cell selection it is a no-op, which is
        // the point: the world's delete must not leak through.
        if self.in_paint_mode() {
            self.update_paint_session(cx, PaintSession::delete_selection);
            return;
        }
        // The cart despawns its own selection and reports what went; the
        // confirm below guards a DOCUMENT edit the host is not making.
        if self.live_active() {
            self.live_command(EditCommand::Delete);
            return;
        }
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let instances = removable_instances(&open.selected, &open.store.state());
        if instances == 0 {
            self.delete_selected_now(cx);
            return;
        }
        let confirm = ggo_common::confirm_destructive(
            &format!(
                "Remove {instances} instance{}?",
                if instances == 1 { "" } else { "s" }
            ),
            "Remove",
            false,
            window,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if confirm.await {
                this.update(cx, |this, cx| this.delete_selected_now(cx))
                    .ok();
            }
        })
        .detach();
    }

    fn delete_selected_now(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(batch) = remove_selection_ops(&open.selected, &open.store.state()) else {
            return;
        };
        open.store.apply(batch);
        open.note_doc_changed();
        open.selected.clear();
        open.edit_drag = None;
        open.nudge_gesture = None;
        cx.notify();
        self.refresh_instance_counts(cx);
    }

    fn select_all_impl(&mut self, cx: &mut Context<Self>) {
        if self.in_paint_mode() {
            return;
        }
        if self.live_active() {
            self.live_command(EditCommand::SelectAll);
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        open.selected = (0..state.entities.len())
            .map(Selection::Entity)
            .chain((0..state.instances.len()).map(Selection::Instance))
            .collect();
        open.edit_drag = None;
        open.nudge_gesture = None;
        cx.notify();
    }

    fn clear_selection_impl(&mut self, cx: &mut Context<Self>) {
        if self.in_paint_mode() {
            self.escape_paint_mode(cx);
            return;
        }
        if self.live_active() {
            self.live_command(EditCommand::ClearSelection);
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.selected.clear();
        open.marquee = None;
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
        // Painting takes the entity selection out of play (see
        // [`Self::in_paint_mode`]) without clearing it, so the arrows keep
        // their look-around meaning there rather than moving an entity the
        // user can't even see selected. Checked BEFORE Live, the way
        // [`Self::delete_selected_impl`] checks it: a `Nudge` under the
        // brush would move the cart's selection out from under it.
        let painting = self.in_paint_mode();
        // In Live the arrows are otherwise the cart's, selection or not:
        // it owns what is selected and where its camera looks, and the
        // host has no camera of its own left to move.
        if !painting && self.live_active() {
            self.live_command(EditCommand::Nudge {
                dx: delta[0] as i32,
                dy: delta[1] as i32,
            });
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // Nothing selected: the arrows look around instead -- pan the
        // camera opposite the content (arrow right slides content left).
        // `delta`'s sign is the world direction of the keypress, so the pan
        // step reuses it; the magnitude is the camera's own (screen px, not
        // the nudge's world px). No pan before first layout: nothing to do.
        let Some(selection) = open.primary().filter(|_| !painting) else {
            let step = if tile {
                CAMERA_PAN_STEP_PX * 4.0
            } else {
                CAMERA_PAN_STEP_PX
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
            let look = [sign(delta[0]) * step, sign(delta[1]) * step];
            let mut view = open.view.borrow_mut();
            let Some(pan) = view.pan else {
                return;
            };
            view.pan = Some([pan[0] - look[0], pan[1] - look[1]]);
            drop(view);
            cx.notify();
            return;
        };
        let positions = open.selected_positions();
        // A selection gone stale against an undo/redo restructure, or an
        // entity with no Transform, has nothing to move.
        let Some((_, pos)) = positions.iter().find(|(target, _)| *target == selection) else {
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
        open.store
            .apply(move_ops(&positions, selection, next, Some(gesture)));
        open.note_doc_changed();
        cx.notify();
    }

    /// The schema extension of `component.field`, when it is an Asset
    /// field.
    fn asset_field_ext(&self, component: &str, field: &str) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        match inspector::field_kind(&open.schemas, component, field) {
            Some(FieldKind::Asset(ext)) => Some(ext.clone()),
            _ => None,
        }
    }

    /// The committed stem of `component.field` on entity `entity_ix`.
    fn asset_field_stem(&self, entity_ix: usize, component: &str, field: &str) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        open.store
            .state()
            .entities
            .get(entity_ix)?
            .components
            .get(component)?
            .get(field)?
            .as_str()
            .map(str::to_string)
    }

    /// Everything an Asset field's row needs, from ONE stat: the committed
    /// stem, whether it resolves, and (when it does) the worktree-relative
    /// path the jump opens.
    pub(crate) fn asset_field_view(
        &self,
        entity_ix: usize,
        component: &str,
        field: &str,
    ) -> Option<AssetFieldView> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let ext = self.asset_field_ext(component, field)?;
        let stem = self.asset_field_stem(entity_ix, component, field)?;
        let status = inspector::asset_status(&open.root, &stem, &ext);
        let abs = inspector::asset_abs_path(&open.root, &stem, &ext);
        let rel = match (status, abs, self.project_root.as_ref()) {
            (inspector::AssetStatus::Resolves, Some(abs), Some(project_root)) => {
                abs.strip_prefix(project_root).ok().map(|rel| {
                    rel.to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                })
            }
            _ => None,
        };
        Some(AssetFieldView { stem, status, rel })
    }

    #[cfg(test)]
    pub(crate) fn asset_field_status(
        &self,
        entity_ix: usize,
        component: &str,
        field: &str,
    ) -> Option<inspector::AssetStatus> {
        self.asset_field_view(entity_ix, component, field)
            .map(|view| view.status)
    }

    #[cfg(test)]
    pub(crate) fn asset_field_rel(
        &self,
        entity_ix: usize,
        component: &str,
        field: &str,
    ) -> Option<String> {
        self.asset_field_view(entity_ix, component, field)?.rel
    }

    /// Open worktree-relative `rel` wherever its extension is claimed --
    /// through the same path-open interceptor registry the project panel
    /// uses, so a `.spr` lands in the sprite tab, a `.til` in the tileset
    /// editor, a `.map` in the map panel and a `.adp` in the audio tab
    /// with no dependency on any of them. Anything unclaimed opens the
    /// ordinary way.
    ///
    /// Deferred, like `emulate_impl`: this runs from a click inside THIS
    /// panel's update, and an interceptor that reveals a dock panel
    /// (`.map` -> `open_in_panel::<MapPanel>` -> `Dock::activate_panel`)
    /// calls `set_active(false)` on the dock's current panel -- usually
    /// this one -- which would nest an update of an entity already being
    /// updated and panic.
    pub(crate) fn goto_asset(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                let Some(worktree_id) = workspace
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).id())
                else {
                    return;
                };
                let Some(path) = ggo_common::inline_project_path(worktree_id, &rel) else {
                    log::warn!("asset jump: {rel} is not a worktree-relative path");
                    return;
                };
                if workspace.intercept_path_open(&path, window, cx) {
                    return;
                }
                workspace
                    .open_path(path, None, true, window, cx)
                    .detach_and_log_err(cx);
            });
        });
    }

    /// Seal any in-flight nudge run, so the NEXT nudge starts a fresh undo
    /// entry rather than amending the last one. Called from every path
    /// that changes what a nudge would even be moving.
    fn end_nudge_run(&mut self) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.nudge_gesture = None;
        }
    }

    /// Restore the default camera framing. In Design that is
    /// `canvas::reset_camera`'s pair, with the `None` pan letting the next
    /// prepaint re-center on the active camera (see that function's doc);
    /// in Live it is the fitted scale and the document's camera origin.
    fn reset_view_impl(&mut self, cx: &mut Context<Self>) {
        // Live has its own framing: back to the fitted scale, with the
        // cart pointed at the document's camera origin again.
        if self.live_active() {
            let origin = match &self.state {
                ViewerState::Ready(open) => active_camera_origin(&open.store.state()),
                _ => return,
            };
            if let ViewerState::Ready(open) = &mut self.state
                && let Some(live) = open.live.as_mut()
            {
                live.scale = None;
                live.pending_camera = Some(origin);
            }
            cx.notify();
            return;
        }
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

    #[cfg(test)]
    fn step_zoom(&mut self, dir: i32, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let zoom = open.view.borrow().zoom;
        self.set_zoom(canvas::zoom_step(zoom, dir), cx);
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        // Play is the game's: its entities have moved on from whatever the
        // document says, and stepping the history would push a world the
        // play-through has left behind.
        if self.live_playing() {
            return;
        }
        if self.step_paint_history(MapDocStore::undo, cx) {
            return;
        }
        self.end_nudge_run();
        // The undo stack is opaque about WHAT it reversed, so the cheap
        // honest test is to compare the slot list across the step: at
        // most four entries, and only a real change pays for the re-merge
        // and the recompose.
        let backgrounds_before = self.backgrounds_now();
        if let ViewerState::Ready(open) = &mut self.state {
            let before = open.store.state();
            if open.store.undo() {
                open.prune_selection();
                open.note_doc_stepped(&before);
                cx.notify();
            }
        }
        self.refresh_instance_counts(cx);
        if self.backgrounds_now() != backgrounds_before {
            self.refresh_backgrounds(cx);
        }
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        if self.live_playing() {
            return;
        }
        if self.step_paint_history(MapDocStore::redo, cx) {
            return;
        }
        self.end_nudge_run();
        let backgrounds_before = self.backgrounds_now();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let before = open.store.state();
        if !open.store.redo() {
            return;
        }
        open.prune_selection();
        open.note_doc_stepped(&before);
        // A redone add-instance comes back from the undo stack as it was
        // snapshotted -- unresolved -- so it would render as a placeholder
        // until reload. Resolve whatever has neither a subtree nor an
        // error, and refresh the loads/images for it.
        let unresolved: Vec<String> = open
            .store
            .state()
            .instances
            .iter()
            .filter(|instance| instance.resolved.is_none() && instance.error.is_none())
            .map(|instance| instance.world.clone())
            .collect();
        if !unresolved.is_empty() {
            for stem in unresolved {
                let result = loader::resolve_instance(&open.root, &stem);
                open.store.set_instances_resolved(&stem, &result, false);
            }
            Self::refresh_asset_images(open, &mut self.retired_images);
        }
        cx.notify();
        self.refresh_instance_counts(cx);
        if self.backgrounds_now() != backgrounds_before {
            self.refresh_backgrounds(cx);
        }
    }

    /// Write the open world.
    ///
    /// In Live the CART owns the world -- its own edit systems move
    /// entities, rewrite text, paint cells -- so the save first asks it
    /// for a snapshot and for the source cells of every loaded background
    /// slot, and only writes once those have landed
    /// ([`OpenWorld::settle_cart_save`]). Everywhere else, and in every
    /// Live session whose rows do not describe this document yet, the
    /// document IS the truth and the write is immediate.
    fn save_impl(&mut self, cx: &mut Context<Self>) {
        // Play is the game's: its entities are where the play-through put
        // them, and a snapshot taken mid-run would persist that over the
        // world the user authored.
        if self.live_playing() {
            if let ViewerState::Ready(open) = &mut self.state {
                open.fail_save("stop the game to save".to_string());
                cx.notify();
            }
            return;
        }
        if self.cart_owns_the_world() {
            self.begin_cart_save(cx);
            return;
        }
        self.write_document_now(cx);
    }

    /// Write the document as it stands, without asking the cart. The path
    /// every save took before Live could hand a world back.
    fn write_document_now(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.write_now(None);
        cx.notify();
    }

    /// Whether a save must ask the cart for the world before it writes:
    /// the canvas is showing Live, the session is connected, and its rows
    /// describe THIS document. A session that has not finished the world
    /// handshake holds some other world's entities, and the document is
    /// the only truth there is until it has.
    fn cart_owns_the_world(&self) -> bool {
        self.live_active()
            && match &self.state {
                ViewerState::Ready(open) => open.live.as_ref().is_some_and(|live| {
                    live.status == LiveStatus::Connected && live.loaded() && !live.world_dirty
                }),
                _ => false,
            }
    }

    /// Ask the cart for its world and for every loaded layer's cells. The
    /// write happens on the tick the last of them lands.
    fn begin_cart_save(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // A second Save while the first is still in flight is the same
        // request: re-asking would only make the cart stream the world
        // twice, and the waiters already queued settle on the one answer.
        if open.save_pending() {
            return;
        }
        // Only slots the document has a map behind: an unlinked slot has
        // no `.map` for the cells to be written into, and the cart answers
        // it `0 x 0` anyway.
        let slots: Vec<u8> = open
            .merged
            .iter()
            .map(|background| background.layer)
            .filter(|layer| usize::from(*layer) < BACKGROUND_LAYER_COUNT)
            .collect();
        let Some(live) = open.live.as_mut() else {
            return;
        };
        match live.begin_save(slots) {
            Ok(()) => open.save_error = None,
            // Nothing is waiting on an answer that was never asked for, so
            // this is the whole save: the file is untouched and the
            // document stays dirty.
            Err(reason) => open.fail_save(reason),
        }
        cx.notify();
    }

    /// Save, and hand back a task that settles when the write has landed
    /// or failed -- what `WorldCanvasItem::save` (and so the pane's close
    /// prompt) awaits. In Design that is immediate; in Live the cart has
    /// to answer first.
    pub(crate) fn save_task(&mut self, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        self.save_impl(cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return Task::ready(Ok(()));
        };
        if !open.save_pending() {
            return Task::ready(match &open.save_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            });
        }
        let (sender, receiver) = oneshot::channel();
        open.save_waiters.push(sender);
        cx.background_spawn(async move {
            match receiver.await {
                Ok(result) => result,
                // The sender went with the open world: the tab was closed
                // or another world loaded over this one, and the write
                // this caller asked for never happened.
                Err(_) => Err("the world was closed before the save landed".to_string()),
            }
        })
    }

    /// Save on behalf of a "Save" answer to the unsaved-edits prompt,
    /// reporting whether the write actually landed (a failed write must not
    /// let the caller discard the document). Shared by
    /// [`Self::open_rel_path`]'s reload prompt and
    /// [`Self::save_if_open_and_dirty`].
    ///
    /// The DOCUMENT write, not the cart round trip [`Self::save_impl`]
    /// takes in Live: both callers need their answer synchronously, and a
    /// "did it land?" that could only be answered several cart frames
    /// later would have to be reported as "no" -- which is a refusal to
    /// close, or a refused build, over a save that was going to succeed.
    fn save_for_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.write_document_now(cx);
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

    /// The panel has a loaded world (not Empty/Loading/Error).
    ///
    /// `test-support` only, for `ggo_smoke`'s world journeys: the panel
    /// state machine is crate-private, and a smoke test that asserted
    /// "Ready" by poking at rendered text would pass on a panel that had
    /// silently failed to load.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_is_ready(&self) -> bool {
        matches!(self.state, ViewerState::Ready(_))
    }

    /// The stems of the worlds this panel last enumerated -- the set
    /// `AddInstance` offers. Test-only, and deliberately NOT refreshing:
    /// what it is for is proving that something else refreshed it.
    #[cfg(test)]
    pub(crate) fn test_world_stems(&self) -> Vec<String> {
        self.worlds.iter().map(|world| world.stem.clone()).collect()
    }

    /// Give the panel a viewer link without booting a cart, so a test can
    /// prove the panel's release stops the viewer.
    #[cfg(test)]
    pub(crate) fn test_set_live_endpoint(&mut self, endpoint: Arc<ggo_common::LinkEndpoint>) {
        self.live_endpoint = Some(endpoint);
    }

    /// The open document has unsaved edits. `test-support` only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_is_dirty(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.store.state().dirty)
    }

    /// The open document's own `[[background]]` slots, in document order.
    ///
    /// `test-support` only, for `ggo_smoke`'s layers-rail journey: what
    /// the rail edits is the base world's slot list, and a smoke test
    /// that asserted on the rail's rendered labels would pass on a panel
    /// that had linked the wrong layer.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_backgrounds(&self) -> Vec<Background> {
        self.backgrounds_now()
    }

    /// Enter paint mode on background layer `layer`, reporting whether the
    /// slot resolved to a map (the session may still be loading -- pump
    /// the executor, then read [`Self::test_paint_mode_rel`]).
    ///
    /// `test-support` only, for `ggo_smoke`'s map-edit journeys: the rail
    /// button that carries this in the UI is a `Button` inside a rendered
    /// row, and a journey that clicked it by id would still need this to
    /// tell "entered" from "refused".
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_enter_paint_bg(&mut self, layer: u8, cx: &mut Context<Self>) -> bool {
        self.enter_paint_mode(PaintTarget::BgSlot(layer), cx)
    }

    /// Enter paint mode on entity `index`'s `Tilemap` map, reporting
    /// whether it resolved to one (the session may still be loading --
    /// pump the executor, then read [`Self::test_paint_mode_rel`]).
    ///
    /// `test-support` only, the entity counterpart of
    /// [`Self::test_enter_paint_bg`]: the UI paths that carry this are a
    /// context-menu entry and a canvas double-click, neither of which can
    /// tell a caller "entered" from "refused".
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_enter_paint_entity(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        self.enter_paint_mode(PaintTarget::TilemapEntity(index), cx)
    }

    /// The `.map` paint mode is editing, `None` in entity mode.
    /// `test-support` only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_paint_mode_rel(&self) -> Option<String> {
        self.active_paint_target().map(|(rel, _)| rel)
    }

    /// The cached session for `rel`, if this world has loaded one.
    /// `test-support` only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_paint_session(&self, rel: &str) -> Option<&PaintSession> {
        match &self.state {
            ViewerState::Ready(open) => open.sessions.get(rel),
            _ => None,
        }
    }

    /// The WINDOW point that world-pixel position `world` is drawn at, or
    /// `None` before the canvas element has laid out (no bounds, no pan).
    ///
    /// `test-support` only, for `ggo_smoke`'s paint journeys: a smoke test
    /// clicks the real canvas with real mouse events, which means it needs
    /// the canvas element's on-screen origin AND the live camera -- both
    /// crate-private, and both moving targets (the first layout centres the
    /// active camera, so the world origin is nowhere near the canvas
    /// origin). A journey that guessed at either would silently click a
    /// different cell than the one it names.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_canvas_point(&self, world: [f64; 2]) -> Option<gpui::Point<Pixels>> {
        let view = self.canvas_view()?;
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let bounds = open.view.borrow().last_bounds?;
        let local = drag_ops::world_to_screen(world[0], world[1], &view);
        Some(gpui::point(
            bounds.origin.x + px(local[0] as f32),
            bounds.origin.y + px(local[1] as f32),
        ))
    }

    /// How many `[[entity]]` blocks the open document holds.
    /// `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_entity_count(&self) -> usize {
        match &self.state {
            ViewerState::Ready(open) => open.store.state().entities.len(),
            _ => 0,
        }
    }

    /// Entity `index`'s `Transform.pos`, or `None` when the entity does
    /// not exist or has no (numeric, 2-element) Transform.
    ///
    /// The coordinate type is the document's own: WORLD PIXELS as
    /// `[f64; 2]`, not an integer pair -- `Transform.pos` is a TOML float
    /// array (Q16.16-snapped on write by `world_file::write_world`), and
    /// both the drag and the nudge paths do their arithmetic in `f64`.
    /// `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_entity_position(&self, index: usize) -> Option<[f64; 2]> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        inspector::entity_pos(&open.store.state(), index)
    }

    /// How many entities/instances are selected. Selection is an ordered
    /// `Vec<Selection>` whose LAST element is the primary, so this counts
    /// the whole set, not just the primary. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_selected_count(&self) -> usize {
        match &self.state {
            ViewerState::Ready(open) => open.selected.len(),
            _ => 0,
        }
    }

    // ------------------------------------------------------------ agent socket

    /// Every world in the project, `(stem, worktree-relative path)`,
    /// refreshed now.
    pub fn remote_list(&mut self, cx: &mut Context<Self>) -> Vec<(String, String)> {
        self.refresh_worlds(cx);
        self.worlds
            .iter()
            .map(|world| (world.stem.clone(), self.worktree_rel(&world.rel_path)))
            .collect()
    }

    /// The worktree-relative form of an asset-root-relative world path.
    /// The two frames differ under an asset root (`worlds/main.toml`
    /// against `<worktree>/assets` is `assets/worlds/main.toml` to the
    /// explorer), and every path that crosses this panel's edges -- what
    /// a click carries, what [`Self::load_rel_path`] splits the root back
    /// out of, what [`Self::open_rel_path_now`] reports -- is the
    /// worktree-relative one. An asset root outside the worktree has no
    /// worktree-relative form; the asset-root-relative path is handed
    /// back unchanged there, which is what the panel itself would have
    /// used before any world was opened.
    fn worktree_rel(&self, asset_rel: &str) -> String {
        let prefix = self
            .project_root
            .as_ref()
            .zip(self.asset_root())
            .and_then(|(project_root, asset_root)| {
                asset_root.strip_prefix(project_root).ok().map(|rel| {
                    rel.to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                })
            })
            .unwrap_or_default();
        if prefix.is_empty() {
            asset_rel.to_string()
        } else {
            format!("{prefix}/{asset_rel}")
        }
    }

    /// The listing entry `world` names -- a stem (`worlds/arena`), its
    /// asset-root-relative path, or its worktree-relative one -- as the
    /// worktree-relative path to open, or the reason there is none.
    ///
    /// Public for the MCP host: it resolves the name the agent typed
    /// against the project BEFORE handing the rel path to
    /// [`WorldDock::open_world`], and any panel can answer (the listing
    /// only needs the project root), including a throwaway one when no
    /// world tab is open yet.
    pub fn remote_resolve(
        &mut self,
        world: &str,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        self.refresh_worlds(cx);
        self.worlds
            .iter()
            .map(|listing| (listing, self.worktree_rel(&listing.rel_path)))
            .find(|(listing, rel)| {
                listing.stem == world || listing.rel_path == world || rel == world
            })
            .map(|(_, rel)| rel)
            .ok_or_else(|| {
                let stems: Vec<&str> = self.worlds.iter().map(|w| w.stem.as_str()).collect();
                format!("no world {world}; the project has {stems:?}")
            })
    }

    /// The open world drawn to pixels. Default framing is the device
    /// screen (320x240) at the active camera -- what the board shows on
    /// boot; `full` frames the whole scene's bounding box instead.
    /// `(width, height, BGRA)`.
    pub fn remote_screenshot(&self, full: bool) -> Result<(u32, u32, Vec<u8>), String> {
        let ViewerState::Ready(open) = &self.state else {
            // Not Ready: `remote_read` already words every such state
            // (nothing open, still loading, load failed) for the caller.
            return Err(self
                .remote_read()
                .err()
                .unwrap_or_else(|| "no world open".to_string()));
        };
        let items = draw_items(open);
        let (origin, width, height) = if full {
            let mut min = [f64::INFINITY; 2];
            let mut max = [f64::NEG_INFINITY; 2];
            for item in items
                .iter()
                .filter(|item| !matches!(item.kind, DrawKind::SelectionOutline))
            {
                min = [min[0].min(item.x), min[1].min(item.y)];
                max = [max[0].max(item.x + item.w), max[1].max(item.y + item.h)];
            }
            if !min[0].is_finite() {
                return Err("the world draws nothing".to_string());
            }
            let width = ((max[0] - min[0]).ceil() as u32).clamp(1, 4096);
            let height = ((max[1] - min[1]).ceil() as u32).clamp(1, 4096);
            (min, width, height)
        } else {
            (
                active_camera_origin(&open.store.state()),
                DEVICE_SCREEN_W as u32,
                DEVICE_SCREEN_H as u32,
            )
        };
        Ok((
            width,
            height,
            composite_scene(&items, origin, width, height),
        ))
    }

    /// The open world as authored. `Err` while nothing is open or a load
    /// is still in flight, so the caller can wait and ask again.
    pub fn remote_read(&self) -> Result<serde_json::Value, String> {
        let open = match &self.state {
            ViewerState::Ready(open) => open,
            ViewerState::Empty => return Err("no world open — world_open first".to_string()),
            ViewerState::Loading { stem } => {
                return Err(format!("{stem} is {WORLD_STILL_LOADING}"));
            }
            ViewerState::Error(error) => {
                return Err(format!("the open world failed to load: {error}"));
            }
        };
        let state = open.store.state();
        let entities: Vec<serde_json::Value> = state
            .entities
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                serde_json::json!({
                    "index": index,
                    "pos": inspector::entity_pos(&state, index),
                    "components": serde_json::Value::Object(entity.components.clone()),
                })
            })
            .collect();
        let instances: Vec<serde_json::Value> = state
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| {
                serde_json::json!({
                    "index": index,
                    "world": instance.world,
                    "pos": instance.pos,
                    "background_priority": instance.background_priority,
                    "error": instance.error,
                })
            })
            .collect();
        let backgrounds: Vec<serde_json::Value> = state
            .backgrounds
            .iter()
            .map(|background| serde_json::json!({ "layer": background.layer, "map": background.map }))
            .collect();
        let selected: Vec<serde_json::Value> = open
            .selected
            .iter()
            .map(|selection| match selection {
                Selection::Entity(index) => serde_json::json!({ "entity": index }),
                Selection::Instance(index) => serde_json::json!({ "instance": index }),
            })
            .collect();
        Ok(serde_json::json!({
            "stem": open.listing.stem,
            "rel_path": open.source_rel,
            "dirty": state.dirty,
            "entities": entities,
            "instances": instances,
            "backgrounds": backgrounds,
            "selected": selected,
        }))
    }

    /// The open world's display path when it has unsaved edits, else
    /// `None`. Drives both the close guard and (indirectly) the title's
    /// dirty dot.
    fn dirty_world_name(&self) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        // Paint sessions count: a `.map` edited through this world is part
        // of the same document as far as the tab dot, the close prompt and
        // the emulate gate are concerned (spec 2026-08-29).
        let dirty = open.store.state().dirty || open.sessions.values().any(PaintSession::dirty);
        // The CLICKED path, not the asset-root-relative one: the prompt has
        // to name the file the way the user sees it in the explorer.
        dirty.then(|| open.source_rel.clone())
    }

    /// The current camera transform, if the canvas has laid out.
    ///
    /// In Live this is the CART's transform, not the design pan/zoom: the
    /// hit test, the marquee, the drag anchors and the double-click all go
    /// through here, so branching once is what keeps every gesture in the
    /// space the picture is drawn in.
    fn canvas_view(&self) -> Option<View> {
        if self.live_active() {
            return self.live_camera().map(|(view, _, _)| view);
        }
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

    /// The canvas size the element last laid out at, canvas-relative px.
    ///
    /// In Live this stamp comes from the Live prepaint closure alone (see
    /// [`Self::render_canvas`]) -- Live does not run `layout_camera`,
    /// which would also centre the DESIGN pan -- so a gesture before the
    /// first Live layout has no transform and does nothing.
    fn live_canvas_size(&self) -> Option<[f64; 2]> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let bounds = open.view.borrow().last_bounds?;
        Some([f64::from(bounds.size.width), f64::from(bounds.size.height)])
    }

    /// [`Self::live_camera_for`] at the canvas's own size.
    fn live_camera(&self) -> Option<(View, [f64; 4], u32)> {
        self.live_camera_for(self.live_canvas_size()?)
    }

    /// The Live transform for a canvas of `size` (canvas-relative px): the
    /// device frame centered at the effective scale, offset by the cart's
    /// camera. Returns `(view, frame bounds, scale)`.
    ///
    /// The camera is the cart's own REPORT, falling back to the origin:
    /// the cart owns the camera, so the picture and the outlines move
    /// together off the same report.
    fn live_camera_for(&self, size: [f64; 2]) -> Option<(View, [f64; 4], u32)> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let live = open.live.as_ref()?;
        Some(live::geometry(size, live.scale, live.overlay_camera()))
    }

    /// The integer scale the Live picture is drawn at on a canvas of
    /// `size`: the user's choice, else whatever fits.
    fn live_scale(&self, size: [f64; 2]) -> u32 {
        match &self.state {
            ViewerState::Ready(open) => open
                .live
                .as_ref()
                .and_then(|live| live.scale)
                .unwrap_or_else(|| live::fit_scale(size[0], size[1])),
            _ => live::fit_scale(size[0], size[1]),
        }
    }

    /// The cart's modifier bits for a mouse event: 0 shift, 1 ctrl/cmd,
    /// 2 alt (`EditorInput.modifiers`).
    fn modifier_bits(modifiers: &Modifiers) -> u8 {
        u8::from(modifiers.shift)
            | (u8::from(modifiers.secondary()) << 1)
            | (u8::from(modifiers.alt) << 2)
    }

    /// The cart's button bit for a mouse button: 0 left, 1 middle, 2
    /// right. `None` for the buttons the cart has no bit for.
    fn button_bit(button: MouseButton) -> Option<u8> {
        match button {
            MouseButton::Left => Some(1),
            MouseButton::Middle => Some(2),
            MouseButton::Right => Some(4),
            _ => None,
        }
    }

    /// The held-button mask after `button` went down (or up). gpui reports
    /// one button per event, so the mask is the panel's to carry.
    fn live_buttons_after(&mut self, button: MouseButton, down: bool) -> u8 {
        let bit = Self::button_bit(button);
        let ViewerState::Ready(open) = &mut self.state else {
            return 0;
        };
        let Some(live) = open.live.as_mut() else {
            return 0;
        };
        if let Some(bit) = bit {
            if down {
                live.pointer_buttons |= bit;
            } else {
                live.pointer_buttons &= !bit;
            }
        }
        live.pointer_buttons
    }

    /// The held-button mask for a move event. gpui reports at most one
    /// held button on a move, and none at all once every button is up --
    /// which is how a release that landed off the canvas still reaches
    /// the cart instead of leaving a button stuck down there.
    fn live_buttons_moving(&mut self, pressed: Option<MouseButton>) -> u8 {
        let ViewerState::Ready(open) = &mut self.state else {
            return 0;
        };
        let Some(live) = open.live.as_mut() else {
            return 0;
        };
        if pressed.is_none() {
            live.pointer_buttons = 0;
        }
        live.pointer_buttons
    }

    /// The Live geometry a forwarded sample is placed through -- the
    /// device frame rect and the integer scale -- or `None` before the
    /// first Live layout has stamped the canvas.
    ///
    /// The SAME geometry the picture is painted through, so the device px
    /// the cart hit-tests are the pixel the user aimed at.
    fn live_pointer_geometry(&self) -> Option<([f64; 4], u32)> {
        let size = self.live_canvas_size()?;
        let (_, frame_rect, scale) = self.live_camera_for(size)?;
        Some((frame_rect, scale))
    }

    /// Whether the host believes `button` is held from a press that
    /// landed on this canvas.
    fn live_button_held(&self, button: MouseButton) -> bool {
        let Some(bit) = Self::button_bit(button) else {
            return false;
        };
        match &self.state {
            ViewerState::Ready(open) => open
                .live
                .as_ref()
                .is_some_and(|live| live.pointer_buttons & bit != 0),
            _ => false,
        }
    }

    /// Queue one pointer sample for the cart: canvas-relative `local` px
    /// through the inverse of the Live geometry, with the buttons and
    /// modifiers held as the cursor got there.
    ///
    /// The host interprets NOTHING here -- no hit test, no marquee, no
    /// drag, no camera. The cart runs the gesture and publishes what it
    /// did, and [`Self::mirror_cart_state`] is the only path that turns
    /// any of it into a document op.
    fn live_pointer(&mut self, local: [f64; 2], buttons: u8, modifiers: u8) {
        let Some((frame_rect, scale)) = self.live_pointer_geometry() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let snap = open.snap;
        let Some(live) = open.live.as_mut() else {
            return;
        };
        live.push_pointer(live::PointerState {
            device: live::device_from_canvas(local, frame_rect, scale),
            buttons,
            modifiers,
            snap,
        });
    }

    /// Queue one edit command for the cart. In Live every semantic edit --
    /// nudge, delete, duplicate, select all, clear -- runs against the
    /// CART's selection, which the document only mirrors; the host applies
    /// no op of its own and waits for the rows to come back.
    fn live_command(&mut self, command: EditCommand) {
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(live) = open.live.as_mut()
        {
            live.push_command(command);
        }
    }

    /// Whether a canvas gesture belongs to the cart rather than to the
    /// host. Paint mode is the exception, and only for the LEFT button:
    /// the brush is the host's in both modes
    /// (`canvas_primary_down_with`), while the middle drag that pans the
    /// cart's camera has to keep working under it. `None` is a move with
    /// nothing held, which no stroke can be.
    fn live_owns_button(&self, button: Option<MouseButton>) -> bool {
        self.live_active() && !(button == Some(MouseButton::Left) && self.in_paint_mode())
    }

    /// A mouse button went down over the canvas. Returns whether the cart
    /// took it, in which case the host does not hit-test.
    fn canvas_button_down(
        &mut self,
        local: [f64; 2],
        button: MouseButton,
        modifiers: &Modifiers,
    ) -> bool {
        if !self.live_owns_button(Some(button)) {
            return false;
        }
        // Claimed either way, but the mask only moves once there is a
        // transform to place the sample through: a press the cart never
        // heard must not leave the host believing a button is held.
        if self.live_pointer_geometry().is_none() {
            return true;
        }
        let buttons = self.live_buttons_after(button, true);
        self.live_pointer(local, buttons, Self::modifier_bits(modifiers));
        true
    }

    /// A mouse button came up over the canvas; see
    /// [`Self::canvas_button_down`].
    fn canvas_button_up(
        &mut self,
        local: [f64; 2],
        button: MouseButton,
        modifiers: &Modifiers,
    ) -> bool {
        if !self.live_owns_button(Some(button)) {
            return false;
        }
        // Claimed either way, but the mask only moves once there is a
        // transform to place the sample through: a press the cart never
        // heard must not leave the host believing a button is held.
        if self.live_pointer_geometry().is_none() {
            return true;
        }
        let buttons = self.live_buttons_after(button, false);
        self.live_pointer(local, buttons, Self::modifier_bits(modifiers));
        true
    }

    /// The cursor moved over the canvas; see [`Self::canvas_button_down`].
    fn canvas_pointer_move(
        &mut self,
        local: [f64; 2],
        pressed: Option<MouseButton>,
        modifiers: &Modifiers,
    ) -> bool {
        if !self.live_owns_button(pressed) {
            return false;
        }
        if self.live_pointer_geometry().is_none() {
            return true;
        }
        let buttons = self.live_buttons_moving(pressed);
        self.live_pointer(local, buttons, Self::modifier_bits(modifiers));
        true
    }

    /// A button came up somewhere the canvas is not. gpui delivers
    /// `on_mouse_up` only to the element under the cursor, so without this
    /// the cart would never hear the release that ended a drag off the
    /// canvas and would go on dragging until the pointer came back.
    ///
    /// Only for a button the host believes went down on THIS canvas: the
    /// out-of-bounds handler fires for every release anywhere in the
    /// window, and a click in another panel is not a sample the cart has
    /// any use for.
    fn canvas_button_up_out(
        &mut self,
        local: [f64; 2],
        button: MouseButton,
        modifiers: &Modifiers,
    ) {
        if self.live_button_held(button) {
            self.canvas_button_up(local, button, modifiers);
        }
    }

    /// The cursor left the canvas with a button still held: tell the cart
    /// it came up, at the last point the cart was told about.
    ///
    /// gpui stops delivering moves once the cursor is outside, so a
    /// gesture left open would freeze at the boundary and then jump to
    /// wherever the release landed. Ending it here is the same bargain
    /// the paint stroke strikes at the same boundary.
    fn live_release_held(&mut self) -> bool {
        if !self.live_active() {
            return false;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return false;
        };
        let snap = open.snap;
        open.live
            .as_mut()
            .is_some_and(|live| live.release_buttons(snap))
    }

    /// Window px -> canvas-relative px through the bounds the canvas
    /// element stamped at its last layout.
    fn canvas_local(&self, position: gpui::Point<Pixels>) -> Option<[f64; 2]> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let bounds = open.view.borrow().last_bounds?;
        Some([
            f64::from(position.x - bounds.origin.x),
            f64::from(position.y - bounds.origin.y),
        ])
    }

    /// One wheel notch in Live: the picture scale steps, and nothing goes
    /// to the cart -- the scale is the host's own.
    fn live_wheel(&mut self, dir: i32, size: [f64; 2], cx: &mut Context<Self>) {
        let current = self.live_scale(size);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(live) = open.live.as_mut() else {
            return;
        };
        let next = live::scale_step(current, dir);
        if live.scale == Some(next) {
            return;
        }
        live.scale = Some(next);
        cx.notify();
    }

    /// The screen rect the Live overlay outlines cart row `index` at, on a
    /// canvas of `size`.
    #[cfg(test)]
    fn test_live_row_screen_rect(&self, index: u32, size: [f64; 2]) -> Option<[f64; 4]> {
        let (view, _, _) = self.live_camera_for(size)?;
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let [x, y, w, h] = open
            .live
            .as_ref()?
            .rows
            .iter()
            .find(|row| row.index == index)?
            .drawn();
        let top_left = drag_ops::world_to_screen(x, y, &view);
        let bottom_right = drag_ops::world_to_screen(x + w, y + h, &view);
        Some([
            top_left[0],
            top_left[1],
            bottom_right[0] - top_left[0],
            bottom_right[1] - top_left[1],
        ])
    }

    /// The band the Live overlay paints right now, in world px as an
    /// origin and a size -- the same value `render_canvas` hands the
    /// scene.
    #[cfg(test)]
    fn test_live_marquee(&self) -> Option<[f64; 4]> {
        match &self.state {
            ViewerState::Ready(open) => open.live.as_ref().and_then(live::LiveView::marquee_rect),
            _ => None,
        }
    }

    /// The DESIGN view's pan, which no Live gesture may touch.
    #[cfg(test)]
    fn test_design_pan(&self) -> Option<[f64; 2]> {
        match &self.state {
            ViewerState::Ready(open) => open.view.borrow().pan,
            _ => None,
        }
    }

    /// Left-mouse down at canvas-relative `local` px: hit-test in world
    /// coords, update the selection, and arm a placement drag on a hit --
    /// ggo-ide's `PrimaryDown` arm (minus its empty-space pan; panning is
    /// middle-drag here).
    #[cfg(test)]
    fn canvas_primary_down(&mut self, local: [f64; 2], cx: &mut Context<Self>) {
        self.canvas_primary_down_with(local, false, cx);
    }

    /// The real handler: `shift` toggles membership instead of replacing
    /// the selection, and empty space starts a rubber-band (additive with
    /// shift) instead of just deselecting.
    fn canvas_primary_down_with(&mut self, local: [f64; 2], shift: bool, cx: &mut Context<Self>) {
        // Paint mode owns the canvas: entity hit-testing, the marquee and
        // placement drags are all off while a map is under the brush.
        if self.in_paint_mode() {
            if let ViewerState::Ready(open) = &mut self.state {
                open.paint_gesture = true;
            }
            self.paint_at_local(local, Some(shift), cx);
            return;
        }
        // In Live the cart owns every interaction: the press was
        // forwarded (`canvas_button_down`) and the host neither hit-tests
        // nor drags. The paint branch above still runs -- the brush is
        // the host's in both modes.
        if self.live_active() {
            return;
        }
        let Some(view) = self.canvas_view() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let world = drag_ops::screen_to_world(local[0], local[1], &view);
        let hit = hit_test(&draw_items(open), world[0], world[1]);
        // A click ends whatever nudge run was in flight, whether or not it
        // lands on the same item: the next arrow key starts a fresh undo
        // entry, not an amendment of one from before the click.
        open.nudge_gesture = None;
        open.marquee = None;
        match hit {
            Some(target) if shift => open.toggle_selected(target),
            // Clicking a member of the current set keeps the set (so the
            // whole group drags); anything else replaces it.
            Some(target) if open.selected.contains(&target) => open.select_primary(target),
            Some(target) => open.selected = vec![target],
            None => {
                if !shift {
                    open.selected.clear();
                }
                open.marquee = Some(Marquee {
                    start: world,
                    current: world,
                    additive: shift,
                });
            }
        }
        let starts = open.selected_positions();
        let primary_start = open
            .primary()
            .and_then(|p| starts.iter().find(|(t, _)| *t == p).map(|(_, pos)| *pos));
        // Only a click that leaves its target selected drags: a shift-click
        // that just REMOVED a member must not move the survivors.
        let clicked_is_selected = hit.is_some_and(|target| open.selected.contains(&target));
        open.edit_drag = match (clicked_is_selected, primary_start) {
            (true, Some(start_pos)) if !starts.is_empty() => {
                open.gesture_counter += 1;
                Some(EditDrag {
                    gesture_id: format!("drag-{}", open.gesture_counter),
                    start_pos,
                    start_world: world,
                    starts,
                    moved: false,
                })
            }
            _ => None,
        };
        cx.notify();
    }

    /// Second click of a double-click at canvas-relative `local` px: if a
    /// `Tilemap` entity is under it, that entity's map goes under the
    /// brush (spec: "Paint tiles" on a Tilemap entity, context menu or
    /// double-click). Reports whether it entered, so the caller can fall
    /// back to the ordinary select-and-arm-a-drag on anything else.
    fn canvas_double_click(&mut self, local: [f64; 2], cx: &mut Context<Self>) -> bool {
        // The same reason the single click bows out in Live: the entity
        // under the cursor is the cart's to name, not the host's.
        if self.in_paint_mode() || self.live_active() {
            return false;
        }
        let Some(view) = self.canvas_view() else {
            return false;
        };
        let index = {
            let ViewerState::Ready(open) = &self.state else {
                return false;
            };
            let world = drag_ops::screen_to_world(local[0], local[1], &view);
            match hit_test(&draw_items(open), world[0], world[1]) {
                Some(Selection::Entity(index)) => index,
                _ => return false,
            }
        };
        self.enter_paint_mode(PaintTarget::TilemapEntity(index), cx)
    }

    /// Continue the in-flight placement drag to canvas-relative `local`
    /// px: live `MoveEntity`/`MoveInstance` applies sharing the drag's
    /// gesture id (the store coalesces them into ONE undo entry) --
    /// ggo-ide's `Moved` arm, snap included.
    fn canvas_drag_to(&mut self, local: [f64; 2], cx: &mut Context<Self>) {
        if self.in_paint_mode() {
            // Re-fire the tool at every move, the way ggo-ide's map canvas
            // does: a brush drag is a stream of applications, not one.
            self.paint_at_local(local, None, cx);
            return;
        }
        // In Live the move was forwarded (`canvas_pointer_move`); the
        // drag it continues is the cart's. The paint branch above still
        // runs -- the brush is the host's in both modes.
        if self.live_active() {
            return;
        }
        let Some(view) = self.canvas_view() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let world = drag_ops::screen_to_world(local[0], local[1], &view);
        if let Some(marquee) = &mut open.marquee {
            marquee.current = world;
            cx.notify();
            return;
        }
        let Some(drag) = open.edit_drag.clone() else {
            return;
        };
        let Some(primary) = open.primary() else {
            return;
        };
        let pos = canvas::dragged_pos(drag.start_pos, drag.start_world, world, open.snap);
        // Applied even when `pos` is back at the start: `move_ops` writes
        // ABSOLUTE positions from the drag's own anchors, so the move that
        // returns to the origin is what RESTORES it. Skipping it would
        // leave the entity wherever the last off-origin move put it.
        open.store
            .apply(move_ops(&drag.starts, primary, pos, Some(drag.gesture_id)));
        // ...but a gesture that never displaced the primary is not an
        // edit. Latching, not tracking: a drag that wandered and came back
        // still moved the document through positions the cart saw, so the
        // release still owes it a re-sync.
        if let Some(armed) = open.edit_drag.as_mut() {
            armed.moved |= pos != drag.start_pos;
        }
        cx.notify();
    }

    /// Left button released over the canvas: a rubber-band settles into
    /// the selection (bbox overlap; additive keeps what was there), a
    /// placement drag simply ends.
    fn canvas_primary_up(&mut self, cx: &mut Context<Self>) {
        if self.in_paint_mode() {
            self.end_canvas_paint(cx);
            return;
        }
        // In Live the release was forwarded (`canvas_button_up`); the
        // band or drag it settles is the cart's. The paint branch above
        // still runs -- the brush is the host's in both modes.
        if self.live_active() {
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // Only a drag that actually MOVED something: an ordinary
        // selection click arms `edit_drag` too, and re-sending the world
        // blob for it would reset the cart's runtime state on every click.
        let moved = open.edit_drag.take().is_some_and(|drag| drag.moved);
        if moved {
            // The drag applied its ops straight to the store, so this is
            // where the finished move re-syncs the cart -- from the
            // document, which is the only thing that knows the flattened
            // order the world blob is encoded in.
            open.note_doc_changed();
        }
        let Some(marquee) = open.marquee.take() else {
            return;
        };
        let hits = items_in_rect(
            &draw_items(open),
            marquee.start[0],
            marquee.start[1],
            marquee.current[0],
            marquee.current[1],
        );
        if !marquee.additive {
            open.selected.clear();
        }
        for hit in hits {
            if !open.selected.contains(&hit) {
                open.selected.push(hit);
            }
        }
        cx.notify();
    }

    /// Middle-mouse pan handling for a move event. Returns true if the
    /// event belonged to an in-flight pan (handled or cancelled).
    ///
    /// DESIGN only: in Live the middle drag is forwarded to the cart like
    /// every other pointer sample, and the cart moves its own camera.
    fn handle_pan_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
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

    /// Cursor-anchored wheel zoom over the canvas: one ladder step per
    /// wheel notch, with the pan adjusted so the world point under the
    /// cursor stays under it ([`canvas::zoom_at`]). A zero delta, a ladder
    /// end, or a canvas that hasn't laid out yet is a no-op.
    fn wheel_zoom(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let dy = f32::from(event.delta.pixel_delta(px(20.)).y);
        if dy == 0.0 {
            return;
        }
        let dir = if dy > 0.0 { 1 } else { -1 };
        // Live has no zoom ladder: the picture is drawn at whole device
        // pixels, so the wheel steps that integer scale instead.
        if self.live_active() {
            if let Some(size) = self.live_canvas_size() {
                self.live_wheel(dir, size, cx);
            }
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let mut v = open.view.borrow_mut();
        let (Some(pan), Some(canvas_bounds)) = (v.pan, v.last_bounds) else {
            return;
        };
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
    }

    /// Canvas-relative position for an in-flight canvas gesture's move
    /// event; cancels the gesture when the left button is no longer held.
    ///
    /// Paint mode's armed state is [`OpenWorld::paint_gesture`], set by
    /// the canvas's OWN primary-down: a held-button move whose press
    /// landed anywhere else must not stamp cells on entry.
    fn edit_drag_local(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> Option<[f64; 2]> {
        let painting = self.in_paint_mode();
        let ViewerState::Ready(open) = &mut self.state else {
            return None;
        };
        let armed = if painting {
            open.paint_gesture
        } else {
            open.edit_drag.is_some() || open.marquee.is_some()
        };
        if !armed {
            return None;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            open.edit_drag = None;
            open.marquee = None;
            // The button came up somewhere this element got no mouse-up
            // for: close the stroke here rather than leave it open for the
            // next gesture's ops to fold into.
            if painting {
                self.end_canvas_paint(cx);
            }
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
        // The cart owns the entities while its game systems run, so the
        // fields describe a play-through: an edit committed into one would
        // be overwritten by the next frame and written to the file.
        let read_only = self.live_playing();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        let specs = inspector::selection_field_specs(open.primary(), &state, &open.schemas);
        let same_targets = open.inspector.len() == specs.len()
            && open
                .inspector
                .iter()
                .zip(&specs)
                .all(|(entry, spec)| entry.target == spec.target);

        if same_targets {
            for entry in &mut open.inspector {
                entry
                    .editor
                    .update(cx, |editor, _| editor.set_read_only(read_only));
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
                editor.set_read_only(read_only);
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

    /// The inspector's one commit point -- blur, `CommitField`, and a
    /// picked stem, never a keystroke. That is what keeps a Live session's
    /// `SetComponent` traffic down to one datagram per committed field:
    /// the cart interns every distinct string a bag names, so a commit per
    /// keystroke would leak an allocation per character typed. A future
    /// commit-on-input has to debounce (~300 ms idle) before it lands here.
    fn commit_editor(&mut self, editor_id: EntityId, cx: &mut Context<Self>) {
        // The inspector is read-only in Play, but a read-only editor still
        // takes focus -- and a click on the canvas blurs it, which commits.
        // `commit_field` builds an op from text nobody typed, and the whole
        // world would go back out to a running cart over that blur.
        if self.live_playing() {
            return;
        }
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
            // `state` is the document as it was BEFORE the op, which is how
            // a commit of unchanged text -- an op the store folds away --
            // is kept from re-sending the world over a blur.
            open.note_doc_stepped(&state);
            // A committed field can NAME an asset (a Sprite/Tilemap stem):
            // loads are otherwise only resolved at world-open and
            // instance-add, so a freshly named asset would not render
            // until a reload. `fill_missing_asset_loads` is a no-op when
            // the commit introduced no new load target.
            let state = open.store.state();
            loader::fill_missing_asset_loads(
                &open.root,
                &state,
                &mut open.sprite_loads,
                &mut open.map_loads,
                &mut open.meta_sprite_loads,
            );
            Self::replace_images(open, &mut self.retired_images);
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

    // ---------------------------------------------------------- live mode

    /// Start a live session for the open world by booting a viewer cart
    /// for it. One cart per panel, i.e. per world tab: the cart is
    /// greeted with THIS panel's document, so a shared one could only
    /// ever show whichever tab greeted it last.
    ///
    /// The boot goes through `ggo_common`'s viewer registry from inside a
    /// `Workspace` update, deferred out of this one exactly as
    /// [`Self::emulate_impl`] defers -- the workspace is leased while a
    /// panel listener runs, and the booter needs an update of its own on
    /// it. The booter opens NO emulator pane: it registers a headless
    /// `ViewerRun` that publishes frames into the endpoint.
    fn enter_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // Design's own gesture state is stranded by the switch: Live's
        // handlers forward the pointer to the cart instead of feeding
        // these, so a band or a placement drag armed before the switch
        // would sit on the canvas until Design came back and a stray
        // release settled it.
        open.edit_drag = None;
        open.marquee = None;
        // A `Failed` session is kept only for its message; it is not a
        // session, so re-entering Live must be able to start a fresh one.
        if open
            .live
            .as_ref()
            .is_some_and(|live| matches!(live.status, LiveStatus::Failed(_)))
        {
            open.live = None;
            open.live_error = None;
        }
        if open.live.is_some() {
            return;
        }
        let rel = open.source_rel.clone();
        // A run left over from this panel's PREVIOUS world has nothing to
        // do with the one now open, and nothing polls it any more.
        self.stop_live_endpoint();
        let Some(workspace) = self.workspace.clone() else {
            self.fall_back_to_design(
                "the world panel has no workspace to boot the viewer cart in".to_string(),
                cx,
            );
            return;
        };
        let this = cx.weak_entity();
        self.live_boot_pending = true;
        window.defer(cx, move |window, cx| {
            let endpoint = workspace.upgrade().and_then(|workspace| {
                workspace.update(cx, |workspace, cx| {
                    ggo_common::boot_viewer(workspace, &rel, window, cx)
                })
            });
            this.update(cx, |this, cx| {
                this.live_boot_pending = false;
                match endpoint {
                    Some(endpoint) => {
                        this.live_endpoint = Some(endpoint.clone());
                        this.start_live(endpoint, cx);
                    }
                    None => this.fall_back_to_design(
                        "no emulator pane is available to run the viewer cart".to_string(),
                        cx,
                    ),
                }
            })
            .ok();
        });
    }

    /// Attach a [`LiveView`] to the open world and start polling it.
    ///
    /// The loop races the endpoint's per-frame tick against a timer: ticks
    /// stop while the emulator is paused or between runs, and the mailbox's
    /// retries only happen inside a `poll` (Phase 2 review).
    fn start_live(&mut self, endpoint: Arc<ggo_common::LinkEndpoint>, cx: &mut Context<Self>) {
        // `background_executor().now()` rather than `Instant::now()`: it is
        // the clock gpui's test timers move, so the build deadline is
        // testable instead of being 120 s of real waiting.
        let now = cx.background_executor().now();
        let executor = cx.background_executor().clone();
        let ticks = endpoint.ticks();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let woke = smol::future::race(async { ticks.recv().await.ok() }, async {
                    executor.timer(live::POLL_INTERVAL).await;
                    Some(())
                })
                .await;
                if woke.is_none() {
                    break;
                }
                match this.update(cx, |this, cx| this.live_tick(cx)) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        });
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let mut live = LiveView::new(endpoint, now);
        live.poll = Some(poll);
        // Carried into the session before its greeting: the push itself
        // waits for the cart to answer (`live_step`), since a cart resets
        // to its select tool on every greeting.
        live.tool = self.live_tool;
        open.live = Some(live);
        open.live_error = None;
        if !self.live_tick(cx)
            && let ViewerState::Ready(open) = &mut self.state
            && let Some(live) = &mut open.live
        {
            // The session failed on its very first turn (a cart that was
            // already stopped, say): the loop just spawned has nothing to
            // wake for. Dropped from HERE, not from inside the loop.
            live.poll = None;
        }
    }

    /// Whether the canvas is showing the CART rather than the design
    /// renderer: Live mode with a session that has not failed. Every
    /// gesture branches on this, so the hit test and the picture under it
    /// can never disagree.
    fn live_active(&self) -> bool {
        self.canvas_mode == CanvasMode::Live
            && match &self.state {
                ViewerState::Ready(open) => open
                    .live
                    .as_ref()
                    .is_some_and(|live| !matches!(live.status, LiveStatus::Failed(_))),
                _ => false,
            }
    }

    /// What the Live tab shows INSTEAD of the canvas while the viewer is
    /// coming up: the cart is still being built, or it is running but has
    /// not both greeted and presented a frame. `None` once there is
    /// something to draw -- and always in Design, which is also where a
    /// failed session falls back to.
    pub(crate) fn live_loading_text(&self) -> Option<String> {
        if self.canvas_mode != CanvasMode::Live {
            return None;
        }
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let booting = || {
            let stem = std::path::Path::new(open.source_rel.as_str())
                .file_stem()
                .map_or_else(
                    || open.source_rel.clone(),
                    |stem| stem.to_string_lossy().into_owned(),
                );
            format!("Booting viewer for {stem}…")
        };
        let Some(live) = open.live.as_ref() else {
            // The boot is deferred out of the click that asked for it, so
            // there is at least one render in Live with no session behind
            // it. The boot screen covers those: falling through to the
            // canvas would paint a frame of the DESIGN renderer in the
            // middle of switching away from it.
            return self.live_boot_pending.then(booting);
        };
        if matches!(live.status, LiveStatus::Failed(_)) {
            return None;
        }
        match live.endpoint.state() {
            ggo_common::ViewerState::Building => Some("Building viewer cart…".to_string()),
            ggo_common::ViewerState::Running => {
                if live.status == LiveStatus::Connected && live.frame.is_some() {
                    return None;
                }
                Some(booting())
            }
            // A stopped cart is a failure the status row reports; the
            // fallback to Design is what the user sees.
            ggo_common::ViewerState::Stopped(_) => None,
        }
    }

    /// One turn of the live session. Returns whether the poll loop should
    /// keep going: a session that has no future -- gone, or `Failed` --
    /// never says yes, because `live_step` has nothing left to do for it
    /// and a loop that kept waking would notify the panel every
    /// [`live::POLL_INTERVAL`] for the rest of the document's life.
    fn live_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let wall = cx.background_executor().now();
        // The document's instance list decides the flattened index map,
        // and walking it reads world files -- so the walk happens off this
        // thread, and only when the session says the document moved.
        self.refresh_instance_counts(cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return false;
        };
        let live_and_running = open
            .live
            .as_ref()
            .is_some_and(|live| !matches!(live.status, LiveStatus::Failed(_)));
        if !live_and_running {
            return false;
        }
        let step = open.live_step(wall);
        if step.changed {
            cx.notify();
        }
        match step.failure {
            Some(failure) => {
                self.fall_back_to_design_from(failure, cx);
                false
            }
            None => true,
        }
    }

    /// Show the design renderer again and say why. The [`LiveView`] (if
    /// there is one) is kept in [`LiveStatus::Failed`] so the toolbar can
    /// name the failure; it stops being polled.
    fn fall_back_to_design(&mut self, reason: String, cx: &mut Context<Self>) {
        self.fall_back_to_design_from(
            LiveFailure {
                reason,
                cart_stopped: false,
            },
            cx,
        );
    }

    fn fall_back_to_design_from(&mut self, failure: LiveFailure, cx: &mut Context<Self>) {
        let LiveFailure {
            reason,
            cart_stopped,
        } = failure;
        log::warn!("GGO: live world view fell back to the design view: {reason}");
        self.canvas_mode = CanvasMode::Design;
        self.live_boot_pending = false;
        // Deliberately NOT `note_sticky_live`: the dock's sticky mode is
        // the user's choice, and a transient boot failure must not turn
        // Live off for every tab they open afterwards.
        // `Failed` is terminal, so nothing polls this cart again -- and
        // `enter_live`'s Retry would hand the session straight back to the
        // one that just failed. Ending the run is what makes Retry boot a
        // fresh cart. A cart that reported `Stopped` is already over.
        if !cart_stopped {
            self.stop_live_endpoint();
        }
        if let ViewerState::Ready(open) = &mut self.state {
            // A save waiting on this session is over: the cart that was
            // going to hand its world back is gone, and nothing else will
            // ever answer the blobs it asked for.
            if open.save_pending() {
                if let Some(live) = &mut open.live {
                    live.saving = None;
                }
                open.fail_save(format!("the live session ended: {reason}"));
            }
            open.live_error = Some(reason.clone());
            if let Some(live) = &mut open.live {
                live.status = LiveStatus::Failed(reason);
                // The emu panel calls `drop_image` on every frame it
                // retires, and a run that is ending retires its last one.
                live.frame = None;
            }
        }
        cx.notify();
    }

    /// Show `mode` on the canvas, and make it the panel's mode: it is
    /// sticky, so the next world opened through the explorer comes up the
    /// same way.
    ///
    /// Live is entered by asking the booter again rather than by reviving
    /// whatever ran before -- the emulator pane owns "is a rebuild
    /// needed?", and the panel is not in a position to second-guess it.
    pub(crate) fn set_canvas_mode(
        &mut self,
        mode: CanvasMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match mode {
            CanvasMode::Design => self.leave_live(cx),
            CanvasMode::Live => {
                self.canvas_mode = CanvasMode::Live;
                self.enter_live(window, cx);
                cx.notify();
            }
        }
        self.note_sticky_live(cx);
    }

    /// Switch the cart between its edit systems and its game systems.
    ///
    /// The rail reads [`live::LiveView::mode`] back, so the state moves
    /// here rather than waiting for the cart to confirm: the wire has no
    /// mode report, and a switch that only showed once the cart answered
    /// would never show at all. Whatever the mode change means for the
    /// SELECTION is the cart's (it clears its own on entering Play); the
    /// mirror follows it back.
    pub(crate) fn set_live_mode(&mut self, mode: EditorMode, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(live) = open.live.as_mut() else {
            return;
        };
        // Not just `live.mode == mode`: a pick whose push failed left the
        // rail showing a mode the cart is not in, and pressing it again is
        // the only way back -- taking that for a no-op strands the session.
        if live.mode == mode && !live.mode_push_failed {
            return;
        }
        live.mode = mode;
        match mode {
            // The mirror discards every report while the game runs (the
            // entities are the game's), and the cart republishes nothing
            // on its way back out -- so what Edit inherits is a document
            // that may disagree with the cart on every entity. The blob
            // is the only thing that can say the whole table at once, and
            // it also puts the play-through's motion back.
            EditorMode::Edit => live.resend_world(),
            // Commands planned against the pre-Play document would land on
            // the game's world; the resend on the way back out re-states
            // whatever they were owed.
            EditorMode::Play => live.forget_edits(),
        }
        match live.mailbox.set_mode(mode) {
            Ok(()) => live.mode_push_failed = false,
            Err(error) => {
                // The rail already reads the new mode, so a push that never
                // left would otherwise show a cart switched that is not.
                live.mode_push_failed = true;
                log::warn!("GGO: live mode: {error}");
                open.live_error = Some(format!("live update: {error}"));
            }
        }
        cx.notify();
    }

    /// Pick the tool the cart runs the pointer through. Sticky on the
    /// panel (and through it the dock) for the same reason the canvas mode
    /// is: the next session is the same user's.
    pub(crate) fn set_live_tool(&mut self, tool: u8, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(live) = open.live.as_mut() else {
            return;
        };
        live.tool = tool;
        if let Err(error) = live.mailbox.set_tool(tool) {
            log::warn!("GGO: live tool: {error}");
            open.live_error = Some(format!("live update: {error}"));
        }
        self.live_tool = tool;
        self.note_sticky_live(cx);
        cx.notify();
    }

    /// Whether the CART is playing rather than editing: the game systems
    /// are moving the entities, so the document under them is not the
    /// user's to edit.
    fn live_playing(&self) -> bool {
        self.live_active()
            && match &self.state {
                ViewerState::Ready(open) => open
                    .live
                    .as_ref()
                    .is_some_and(|live| live.mode == EditorMode::Play),
                _ => false,
            }
    }

    /// Leave Live mode deliberately: the session goes, and so does the
    /// viewer run behind it.
    fn leave_live(&mut self, cx: &mut Context<Self>) {
        self.canvas_mode = CanvasMode::Design;
        self.live_boot_pending = false;
        if let ViewerState::Ready(open) = &mut self.state {
            let saving = open.save_pending();
            open.live = None;
            open.live_error = None;
            // Same as a failed session: the cart the save was waiting on
            // is gone with the session the user just left.
            if saving {
                open.fail_save("the live session was closed".to_string());
            }
        }
        self.stop_live_endpoint();
        cx.notify();
    }

    /// Recount what each `[[instance]]` contributes to the flattened
    /// entity order, when (and only when) the document's instance LIST has
    /// changed since the last count -- an add, a remove, an undo or redo
    /// of either, a paste. Every other edit leaves the counts alone.
    ///
    /// The walk reads every instanced world file (recursively), so it runs
    /// on a background thread; the live index map is rebuilt when the
    /// result lands. Called from the mutation funnels that can change the
    /// list ([`Self::apply_op`], the batch delete, undo and redo) so the
    /// recount starts with the edit, AND from [`Self::live_tick`] as the
    /// backstop for any path that reaches the store another way.
    fn refresh_instance_counts(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // Only worth asking while a session is mirroring the document:
        // the counts exist for the live index map and nothing else.
        if open.live.is_none() {
            return;
        }
        let state = open.store.state();
        let stems: Vec<String> = state
            .instances
            .iter()
            .map(|instance| instance.world.clone())
            .collect();
        if stems == open.counted_instances {
            return;
        }
        // Claimed up front so a walk in flight is not re-spawned every
        // tick; a result is only applied if the list still matches.
        open.counted_instances = stems.clone();
        let root = open.root.clone();
        let instances: Vec<world_file::WorldInstance> = state
            .instances
            .iter()
            .map(|instance| world_file::WorldInstance {
                world: instance.world.clone(),
                pos: instance.pos,
                background_priority: instance.background_priority,
            })
            .collect();
        let counts =
            cx.background_spawn(async move { loader::instance_entity_counts(&root, &instances) });
        open._instance_counts_task = Some(cx.spawn(async move |this, cx| {
            let counts = counts.await;
            this.update(cx, |this, cx| {
                let ViewerState::Ready(open) = &mut this.state else {
                    return;
                };
                if open.counted_instances != stems {
                    return;
                }
                open.instance_counts = counts;
                open.rebuild_index_map();
                cx.notify();
            })
            .ok();
        }));
    }

    /// End the viewer run this panel booted, if any. Advisory: the
    /// emulator acts on it at its next frame.
    fn stop_live_endpoint(&mut self) {
        if let Some(endpoint) = self.live_endpoint.take() {
            endpoint.request_stop();
        }
    }

    /// The toolbar's popout-Emulate button: build the cartridge exactly as
    /// the in-pane Emulate does (save first, `emd pack-ggo --world <stem>`),
    /// then launch the standalone `ggo-emu` on the artifact instead of
    /// booting the in-IDE pane. No registry hop, because no emulator pane
    /// is involved -- the whole flow is this panel's own, and its failures
    /// land on this toolbar ([`OpenWorld::popout_error`]).
    /// The toolbar's flash button: hand the open project to the emulator
    /// pane's board flasher. Deferred out of this click for the same
    /// reason [`Self::emulate_impl`] defers -- the handler updates the
    /// workspace, which is leased while a panel listener runs.
    fn flash_impl(&mut self, rebuild_gateware: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        // The world THIS panel has open is what "flash the current world"
        // means; without it the cart boots the manifest's `default_world`
        // and the board shows a world nobody asked for.
        let world = self.open_world_stem();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                if !ggo_common::flash_to_board(
                    workspace,
                    world.as_deref(),
                    rebuild_gateware,
                    window,
                    cx,
                ) {
                    log::warn!("no emulator pane is available to flash this project");
                }
            });
        });
    }

    /// The open document's world stem (`worlds/arena`), by the same
    /// [`world_stem`] rule the Emulate and popout builds use -- there is
    /// exactly one `worlds/`-splitting rule in this fork and this is not a
    /// second one. `None` while no world is open.
    ///
    /// Public because the emulator panel's own flash surfaces fall back to
    /// it: a world open HERE is the world the user is working on, whether
    /// or not they pressed anything in this panel.
    pub fn open_world_stem(&self) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        world_stem(&open.source_rel)
    }

    fn emulate_popout_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let rel = open.source_rel.clone();
        self.set_popout_error(None, cx);
        // Same stale-file rule as the in-pane Emulate: a failed save
        // cancels the build (`save_error` is already on the toolbar).
        if !self.save_if_open_and_dirty(&rel, cx) {
            return;
        }
        let Some(root) = self.project_root.clone() else {
            self.set_popout_error(Some("no project folder is open".to_string()), cx);
            return;
        };
        let Some(stem) = world_stem(&rel) else {
            self.set_popout_error(Some(format!("{rel} is not a world file")), cx);
            return;
        };
        let Some(project_dir) = ggo_common::emerald_project_root(&root.join(&rel)) else {
            self.set_popout_error(
                Some(format!(
                    "no {} above {rel} — emd needs an emerald project",
                    ggo_common::EMERALD_MANIFEST
                )),
                cx,
            );
            return;
        };
        let out_dir = project_dir.join(ggo_common::PACK_OUT_DIR);
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            self.set_popout_error(Some(format!("{}: {e}", out_dir.display())), cx);
            return;
        }
        let out = out_dir.join(ggo_common::pack_out_name(&stem));
        let pack =
            ggo_common::ProcRequest::emd(&project_dir, ggo_common::world_pack_args(&out, &stem));
        let launch = ggo_common::ProcRequest::new(
            ggo_common::ggo_emu_bin(),
            project_dir,
            vec![out.to_string_lossy().into_owned()],
        );
        let runner = self.proc_runner.clone();
        let launcher = self.emu_launcher.clone();
        self._popout_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let capture = runner(pack);
                    if !capture.ok {
                        return Err(format!(
                            "build failed: {}",
                            ggo_common::failure_reason(&capture)
                        ));
                    }
                    launcher(launch)
                })
                .await;
            this.update(cx, |this, cx| this.set_popout_error(result.err(), cx))
                .ok();
        }));
    }

    /// Set (or clear) the popout-Emulate failure on the toolbar. A no-op
    /// when no world is open any more -- there is nowhere to show it.
    fn set_popout_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.popout_error = error;
            cx.notify();
        }
    }

    // ------------------------------------------------- clipboard

    /// The selection as a world-file fragment (entities first, then
    /// instances, each in selection order).
    fn selection_fragment(open: &OpenWorld) -> (Vec<WorldEntity>, Vec<world_file::WorldInstance>) {
        let state = open.store.state();
        let mut entities = Vec::new();
        let mut instances = Vec::new();
        for target in &open.selected {
            match *target {
                Selection::Entity(i) => {
                    if let Some(entity) = state.entities.get(i) {
                        entities.push(entity.clone());
                    }
                }
                Selection::Instance(i) => {
                    if let Some(instance) = state.instances.get(i) {
                        instances.push(world_file::WorldInstance {
                            world: instance.world.clone(),
                            pos: instance.pos,
                            background_priority: instance.background_priority,
                        });
                    }
                }
            }
        }
        (entities, instances)
    }

    fn copy_impl(&mut self, cx: &mut Context<Self>) {
        // In paint mode the clipboard is the CELL clipboard: copying
        // entities from under a brush would put the wrong thing on it.
        if self.in_paint_mode() {
            if let Some(stamp) = self.paint_session().and_then(PaintSession::selection_stamp) {
                self.cell_clipboard = Some(stamp);
                cx.notify();
            }
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let (entities, instances) = Self::selection_fragment(open);
        if entities.is_empty() && instances.is_empty() {
            return;
        }
        match fragment_to_toml(&entities, &instances) {
            Ok(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            Err(e) => {
                open.clipboard_error = Some(format!("copy failed: {e}"));
                cx.notify();
            }
        }
    }

    fn paste_impl(&mut self, cx: &mut Context<Self>) {
        // Ahead of the system-clipboard read, not folded into
        // `paste_fragment`'s gate: in paint mode the system clipboard is
        // not even consulted, so whatever text happens to be on it cannot
        // raise a "not world TOML" error over a paste that never wanted it.
        if self.in_paint_mode() {
            self.paste_cells(cx);
            return;
        }
        if self.live_playing() {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let fragment = match parse_fragment(&text) {
            Ok(fragment) => fragment,
            Err(e) => {
                if let ViewerState::Ready(open) = &mut self.state {
                    open.clipboard_error = Some(format!("clipboard is not world TOML: {e}"));
                    cx.notify();
                }
                return;
            }
        };
        self.paste_fragment(fragment.entities, fragment.instances, cx);
    }

    /// Copy + paste without touching the clipboard.
    fn duplicate_impl(&mut self, cx: &mut Context<Self>) {
        // The cart copies its own selection and reports the spawns; the
        // document hears about them through the mirror.
        if self.live_active() {
            self.live_command(EditCommand::Duplicate);
            return;
        }
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let (entities, instances) = Self::selection_fragment(open);
        if entities.is_empty() && instances.is_empty() {
            return;
        }
        self.paste_fragment(entities, instances, cx);
    }

    /// Where a paste lands: the group's top-left position goes to the
    /// cursor when it is over the canvas (snapped when Snap is on),
    /// otherwise every item shifts one tile right and down.
    fn paste_delta(
        &self,
        entities: &[WorldEntity],
        instances: &[world_file::WorldInstance],
    ) -> [f64; 2] {
        let ViewerState::Ready(open) = &self.state else {
            return [PASTE_OFFSET_PX, PASTE_OFFSET_PX];
        };
        let hover = open.view.borrow().hover;
        let Some((hover, view)) = hover.zip(self.canvas_view()) else {
            return [PASTE_OFFSET_PX, PASTE_OFFSET_PX];
        };
        let positions: Vec<[f64; 2]> = entities
            .iter()
            .filter_map(inspector::transform_pos)
            .chain(instances.iter().map(|i| i.pos))
            .collect();
        let Some(base) = positions
            .iter()
            .copied()
            .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1])])
        else {
            return [PASTE_OFFSET_PX, PASTE_OFFSET_PX];
        };
        let mut target = drag_ops::screen_to_world(hover[0], hover[1], &view);
        if open.snap {
            target = drag_ops::snap_to_tile(target);
        }
        [target[0] - base[0], target[1] - base[1]]
    }

    /// Add `entities` and `instances` (shifted by [`Self::paste_delta`]) as
    /// ONE undo entry, resolve the new instances so they render, and make
    /// the pasted set the selection. An instance the cycle guard refuses
    /// is skipped and reported.
    fn paste_fragment(
        &mut self,
        entities: Vec<WorldEntity>,
        instances: Vec<world_file::WorldInstance>,
        cx: &mut Context<Self>,
    ) {
        // The mutation funnel both Paste and Duplicate reach: one gate
        // keeps both out of the world while a map is under the brush.
        if self.in_paint_mode() {
            return;
        }
        let delta = self.paste_delta(&entities, &instances);
        let candidates = self.instance_candidates();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        let first_entity = state.entities.len();
        let first_instance = state.instances.len();
        let mut ops = Vec::new();
        for entity in &entities {
            let mut components = entity.components.clone();
            if let Some(pos) = inspector::transform_pos(entity) {
                inspector::set_transform_pos(
                    &mut components,
                    [pos[0] + delta[0], pos[1] + delta[1]],
                );
            }
            ops.push(WorldOp::AddEntity { components });
        }
        let mut refused = Vec::new();
        let mut accepted_stems = Vec::new();
        for instance in &instances {
            if !candidates.contains(&instance.world) {
                refused.push(instance.world.clone());
                continue;
            }
            let index = first_instance + accepted_stems.len();
            accepted_stems.push(instance.world.clone());
            ops.push(WorldOp::AddInstance {
                world: instance.world.clone(),
            });
            ops.push(WorldOp::MoveInstance {
                index,
                pos: [instance.pos[0] + delta[0], instance.pos[1] + delta[1]],
                gesture: None,
            });
        }
        open.clipboard_error = (!refused.is_empty()).then(|| {
            format!(
                "skipped instance{} that would cycle: {}",
                if refused.len() == 1 { "" } else { "s" },
                refused.join(", ")
            )
        });
        if ops.is_empty() {
            cx.notify();
            return;
        }
        open.store.apply(WorldOp::Batch(ops));
        open.note_doc_changed();
        for stem in &accepted_stems {
            let result = loader::resolve_instance(&open.root, stem);
            open.store.set_instances_resolved(stem, &result, false);
        }
        Self::refresh_asset_images(open, &mut self.retired_images);
        let state = open.store.state();
        open.selected = (first_entity..state.entities.len())
            .map(Selection::Entity)
            .chain((first_instance..state.instances.len()).map(Selection::Instance))
            .collect();
        open.edit_drag = None;
        open.nudge_gesture = None;
        cx.notify();
        // Instances can arrive with a paste, and this batch never went
        // through `apply_op` -- see `add_instance_impl`.
        self.refresh_instance_counts(cx);
    }

    /// Compose load targets introduced since the last rebuild and refresh
    /// the image cache -- the tail every path that adds assets shares.
    fn refresh_asset_images(open: &mut OpenWorld, retired: &mut Vec<Arc<RenderImage>>) {
        let state = open.store.state();
        loader::fill_missing_asset_loads(
            &open.root,
            &state,
            &mut open.sprite_loads,
            &mut open.map_loads,
            &mut open.meta_sprite_loads,
        );
        Self::replace_images(open, retired);
    }

    /// Rebuild the image cache from the current loads and queue every
    /// image the new cache no longer holds for atlas release.
    fn replace_images(open: &mut OpenWorld, retired: &mut Vec<Arc<RenderImage>>) {
        // Reusing the previous cache's images is what makes retiring by
        // key correct: a key still present keeps its RenderImage (and its
        // atlas identity); only a key that vanished is released.
        let images = Arc::new(canvas::build_image_cache_reusing(
            &[&open.sprite_loads, &open.map_loads, &open.meta_sprite_loads],
            &open.images,
        ));
        retired.extend(retired_by_rebuild(&open.images, &images));
        open.images = images;
    }

    /// Queue the whole image cache of the open world (it is about to be
    /// replaced or dropped).
    fn retire_open_images(&mut self) {
        if let ViewerState::Ready(open) = &self.state {
            self.retired_images.extend(open.images.values().cloned());
        }
    }

    /// The per-render half of the release contract, called by the canvas
    /// item: drop what was retired before the previous render, hold this
    /// render's batch one more frame.
    pub(crate) fn retire_images(&mut self, window: &mut Window) {
        for image in std::mem::take(&mut self.retired_previous) {
            if let Err(error) = window.drop_image(image) {
                log::warn!("releasing a retired world image: {error}");
            }
        }
        self.retired_previous = std::mem::take(&mut self.retired_images);
    }

    // ------------------------------------------------- entity list

    /// Select from the list: the same rules as a canvas click.
    fn select_from_list(&mut self, target: Selection, shift: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if shift {
            open.toggle_selected(target);
        } else {
            open.selected = vec![target];
        }
        open.edit_drag = None;
        open.nudge_gesture = None;
        cx.notify();
    }

    fn render_entity_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            return div().into_any_element();
        };
        let state = open.store.state();
        let rows = entity_list_rows(&state);
        // Collected once, not re-derived per row: `store.state()` clones
        // the whole document, so asking `paint_target_rel` per row would
        // make rendering the list quadratic in entity count.
        let paintable: Vec<bool> = state
            .entities
            .iter()
            .map(|entity| tilemap_fields(entity).is_some())
            .collect();
        let selected_bg = cx.theme().colors().element_selected;
        div()
            .id("ggo-world-entity-list")
            .w(LIST_WIDTH)
            .h_full()
            .flex_none()
            .overflow_y_scroll()
            .child(v_flex().children(rows.into_iter().map(|(target, label)| {
                let selected = open.selected.contains(&target);
                let row = div()
                    .id(SharedString::from(format!("ggo-world-list-{target:?}")))
                    .px_1()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(selected_bg))
                    .child(Label::new(label).size(LabelSize::Small).color(if selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.select_from_list(target, event.modifiers.shift, cx)
                        }),
                    );
                let Selection::Entity(index) = target else {
                    return row.into_any_element();
                };
                if !paintable.get(index).copied().unwrap_or(false) {
                    return row.into_any_element();
                }
                let weak = cx.weak_entity();
                ui::right_click_menu(SharedString::from(format!("ggo-world-list-menu-{index}")))
                    .trigger(move |_menu_open, _window, _cx| row)
                    .menu(move |window, cx| {
                        let weak = weak.clone();
                        ContextMenu::build(window, cx, move |menu, _window, _cx| {
                            menu.entry("Paint tiles", None, move |_window, cx| {
                                weak.update(cx, |this, cx| {
                                    this.enter_paint_mode(PaintTarget::TilemapEntity(index), cx);
                                })
                                .ok();
                            })
                        })
                    })
                    .into_any_element()
            })))
            .into_any_element()
    }

    // ------------------------------------------------- audio budget

    /// Size any audio stem the world names that has no cached size yet --
    /// one background task at a time, generation-guarded against a world
    /// switch. Called from `render`, so a stem committed in the inspector
    /// is sized on the next frame.
    fn schedule_audio_sizes(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open._audio_size_task.is_some() {
            return;
        }
        let state = open.store.state();
        let missing: Vec<String> = audio_budget::audio_stems(&state, &open.schemas)
            .into_iter()
            .filter(|stem| !open.audio_sizes.contains_key(stem))
            .collect();
        if missing.is_empty() {
            return;
        }
        open.audio_size_generation += 1;
        let generation = open.audio_size_generation;
        let root = open.root.clone();
        let sized = cx.background_spawn(async move {
            missing
                .into_iter()
                .map(|stem| {
                    let size = audio_budget::size_stem(&root, &stem);
                    (stem, size)
                })
                .collect::<Vec<_>>()
        });
        open._audio_size_task = Some(cx.spawn(async move |this, cx| {
            let sized = sized.await;
            this.update(cx, |this, cx| {
                let ViewerState::Ready(open) = &mut this.state else {
                    return;
                };
                if open.audio_size_generation != generation {
                    return;
                }
                open._audio_size_task = None;
                open.audio_sizes.extend(sized);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The toolbar's audio readout, `None` when the world names no audio.
    fn audio_budget(&self) -> Option<audio_budget::AudioBudget> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let stems = audio_budget::audio_stems(&open.store.state(), &open.schemas);
        if stems.is_empty() {
            return None;
        }
        Some(audio_budget::summarize(&stems, &open.audio_sizes))
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
                    // `on_mouse_down`, NOT `on_click`: the down half of a
                    // click blurs the field's editor, whose blur-commit
                    // re-renders the inspector -- the rebuilt list never
                    // sees the matching mouse-up, so a click handler is
                    // simply lost. Acting on the down beats the blur.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.pick_stem(target.clone(), pick.clone(), window, cx)
                        }),
                    ),
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
                ggo_common::CopyableText::new("ggo-world-picker-error-copy", error.clone())
                    .size(LabelSize::Small),
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
        // The DOCUMENT's predicate, not the world store's: a brush stroke
        // on a saved world leaves the store clean, and a disabled Save
        // button in front of unwritten cells is an affordance that lies.
        // One local drives all three of the dot, the title color and the
        // button, so they cannot drift apart from each other.
        let dirty = self.dirty_world_name().is_some();
        let saving = open.save_pending();
        let has_selection = !open.selected.is_empty();
        // While the cart plays, the entities are the game's: every button
        // that would mutate the document is greyed out (Delete is not --
        // it is forwarded to the cart, which owns the despawn).
        let playing = self.live_playing();
        // The picker has no `disabled`, so Play empties it instead: an
        // add-instance entry that `add_instance_impl` would refuse is an
        // affordance that lies.
        let candidates = if playing {
            Vec::new()
        } else {
            self.instance_candidates()
        };
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
            .children(self.audio_budget().map(|budget| {
                let tooltip = if budget.missing.is_empty() {
                    "Audio this world uploads into the APU sample region".to_string()
                } else {
                    format!("missing: {}", budget.missing.join(", "))
                };
                div()
                    .id("ggo-world-audio-budget")
                    .tooltip(ui::Tooltip::text(tooltip))
                    .child(Label::new(budget.label()).size(LabelSize::Small).color(
                        if budget.over() {
                            Color::Error
                        } else {
                            Color::Muted
                        },
                    ))
            }))
            .child(div().flex_1())
            .child(
                IconButton::new("ggo-world-emulate", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Emulate this world (cart)"))
                    .on_click(cx.listener(|this, _, window, cx| this.emulate_impl(window, cx))),
            )
            .child(
                // A menu, not a one-click flash: the plain flash reuses the
                // cached bitstream, and pressing the wrong one costs either
                // a stale board or a ~20-minute place-and-route.
                PopoverMenu::new("ggo-world-flash-menu")
                    .trigger(
                        IconButton::new("ggo-world-flash", IconName::GgoFlashRun)
                            .icon_size(IconSize::Small)
                            .tooltip(ui::Tooltip::text(ggo_common::flash_tooltip(
                                "Flash this project to the board",
                                self.open_world_stem().as_deref(),
                            ))),
                    )
                    .menu({
                        let weak = cx.weak_entity();
                        move |window, cx| {
                            let weak = weak.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                                let flash = weak.clone();
                                let rebuild = weak;
                                menu.entry(
                                    "Flash now (cached gateware)",
                                    None,
                                    move |window, cx| {
                                        flash
                                            .update(cx, |this, cx| {
                                                this.flash_impl(false, window, cx)
                                            })
                                            .ok();
                                    },
                                )
                                .entry(
                                    "Flash + rebuild gateware (~20 min)",
                                    None,
                                    move |window, cx| {
                                        rebuild
                                            .update(cx, |this, cx| {
                                                this.flash_impl(true, window, cx)
                                            })
                                            .ok();
                                    },
                                )
                            }))
                        }
                    }),
            )
            .child(
                IconButton::new("ggo-world-emulate-popout", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text(
                        "Emulate this world in the external emulator",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.emulate_popout_impl(cx))),
            )
            // Each wrapper carries the `debug_selector` for the reason the
            // Save button's does: a DISABLED button records no bounds of
            // its own. The wrapper resolves either way, so the suffix is
            // what says WHICH -- greyed out is a state a test has to be
            // able to read, not infer from a click that does nothing.
            .child(
                div()
                    .debug_selector(move || {
                        format!("ggo-world-add-entity-{}", toggle_suffix(!playing))
                    })
                    .child(
                        IconButton::new("ggo-world-add-entity", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(ui::Tooltip::text("Add entity"))
                            .disabled(playing)
                            .on_click(cx.listener(|this, _, _, cx| this.add_entity_impl(cx))),
                    ),
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
                    .on_click(
                        cx.listener(|this, _, window, cx| this.delete_selected_impl(window, cx)),
                    ),
            )
            .child(
                div()
                    .debug_selector(move || format!("ggo-world-undo-{}", toggle_suffix(!playing)))
                    .child(
                        IconButton::new("ggo-world-undo", IconName::Undo)
                            .icon_size(IconSize::Small)
                            .tooltip(ui::Tooltip::text("Undo"))
                            .disabled(playing)
                            .on_click(cx.listener(|this, _, _, cx| this.undo_impl(cx))),
                    ),
            )
            .child(
                div()
                    .debug_selector(move || format!("ggo-world-redo-{}", toggle_suffix(!playing)))
                    .child(
                        IconButton::new("ggo-world-redo", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(ui::Tooltip::text("Redo"))
                            .disabled(playing)
                            .on_click(cx.listener(|this, _, _, cx| this.redo_impl(cx))),
                    ),
            )
            // The wrapper carries the `debug_selector`: `Button` is a
            // `RenderOnce`, and a DISABLED button records no bounds of its
            // own -- so a test that located it by id could never tell
            // "greyed out" from "not rendered".
            .child(
                div().debug_selector(|| "ggo-world-save".into()).child(
                    Button::new("ggo-world-save", "Save")
                        .disabled(!dirty || saving)
                        .on_click(cx.listener(|this, _, _, cx| this.save_impl(cx))),
                ),
            )
            // In Live the write waits on the cart handing its world back,
            // which takes cart frames -- so the row says the save is in
            // flight rather than looking like a button that did nothing.
            .children(saving.then(|| {
                div().debug_selector(|| "ggo-world-saving".into()).child(
                    Label::new("Saving…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }))
            .children(open.save_error.as_ref().map(|e| {
                ggo_common::CopyableText::new(
                    "ggo-world-save-error-copy",
                    format!("save failed: {e}"),
                )
                .size(LabelSize::Small)
            }))
            .children(open.popout_error.as_ref().map(|e| {
                ggo_common::CopyableText::new(
                    "ggo-world-popout-error-copy",
                    format!("popout failed: {e}"),
                )
                .size(LabelSize::Small)
            }))
            // Wrapped for the `debug_selector`, the same reason the Save
            // button is: `CopyableText` is a `RenderOnce` and carries none
            // of its own, so a test could not otherwise tell "the row is
            // there" from "the row was never rendered".
            .children(open.live_error.as_ref().map(|e| {
                div()
                    .debug_selector(|| "ggo-world-live-error".into())
                    .child(
                        ggo_common::CopyableText::new(
                            "ggo-world-live-error-copy",
                            format!("live view: {e}"),
                        )
                        .size(LabelSize::Small),
                    )
            }))
            .children(open.clipboard_error.as_ref().map(|e| {
                ggo_common::CopyableText::new("ggo-world-paste-error-copy", e.clone())
                    .size(LabelSize::Small)
            }))
            .children(open.paint_error.as_ref().map(|e| {
                ggo_common::CopyableText::new(
                    "ggo-world-paint-error-copy",
                    format!("cannot paint: {e}"),
                )
                .size(LabelSize::Small)
            }))
            .into_any_element()
    }

    /// The `Design | Live` switch: which renderer draws the canvas. Its
    /// selected half is [`Self::canvas_mode`] and not [`Self::live_active`]
    /// -- a Live session that failed still reads as Live here, and says so
    /// on the status line rather than snapping the switch back silently.
    fn render_mode_switch(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        h_flex()
            .gap_0p5()
            .child(self.render_mode_half(
                "ggo-world-mode-design",
                "Design",
                CanvasMode::Design,
                "Draw the world with the editor's own renderer",
                cx,
            ))
            .child(self.render_mode_half(
                "ggo-world-mode-live",
                "Live",
                CanvasMode::Live,
                "Draw the world with the viewer cart running it",
                cx,
            ))
            .into_any_element()
    }

    fn render_mode_half(
        &self,
        id: &'static str,
        label: &'static str,
        mode: CanvasMode,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.canvas_mode == mode;
        // The wrapper carries the `debug_selector`: `Button` is a
        // `RenderOnce` and records no bounds of its own. The selected
        // state is IN the selector because a toggled `Button` is otherwise
        // indistinguishable from an untoggled one to a test -- `toggle_state`
        // changes only how it paints.
        div()
            .flex_none()
            .debug_selector(move || format!("{id}-{}", toggle_suffix(selected)))
            .child(
                Button::new(id, label)
                    .label_size(LabelSize::XSmall)
                    .toggle_state(selected)
                    .tooltip(ui::Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_canvas_mode(mode, window, cx)
                    })),
            )
    }

    /// Where the live session is, under the toolbar. Shown for the whole
    /// of Live mode, including the state a windowless load leaves behind
    /// -- [`Self::canvas_mode`] is sticky, so the close-prompt reload and
    /// the MCP `world_open` land in Live with no session at all, and that
    /// is a state the user has to be able to see and leave.
    fn render_live_status(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.canvas_mode != CanvasMode::Live {
            return None;
        }
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let (text, button, failed) = match open.live.as_ref() {
            None => (
                LIVE_IDLE.to_string(),
                Some(("ggo-world-live-start", "Start")),
                false,
            ),
            Some(live) => {
                let (text, failed) = live::status_line(&live.status, live.mailbox.frame_seq());
                let retry = failed.then_some(("ggo-world-live-retry", "Retry"));
                (text, retry, failed)
            }
        };
        let color = if failed { Color::Error } else { Color::Muted };
        Some(
            h_flex()
                .gap_1()
                .px_1()
                .pb_1()
                .debug_selector(|| "ggo-world-live-status".into())
                .child(Label::new(text).size(LabelSize::XSmall).color(color))
                .children(button.map(|(id, label)| {
                    // Both buttons do the same thing -- `enter_live` starts
                    // a session, and clears a failed one first.
                    div().flex_none().debug_selector(move || id.into()).child(
                        Button::new(id, label)
                            .label_size(LabelSize::XSmall)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.enter_live(window, cx)),
                            ),
                    )
                }))
                .into_any_element(),
        )
    }

    /// The edit rail: the cart's `Edit | Play` switch and the tool radio
    /// its greeting named. Live's alone -- the mode is the CART's system
    /// table and the tool names come off the `HelloAck`, and in Design
    /// mode there is no cart to run either.
    fn render_edit_rail(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.live_active() {
            return None;
        }
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let live = open.live.as_ref()?;
        if live.status != LiveStatus::Connected {
            return None;
        }
        let playing = live.mode == EditorMode::Play;
        let mut rail = h_flex()
            .gap_1()
            .px_1()
            .pb_1()
            .child(self.render_edit_mode_half(
                "ggo-world-mode-edit",
                "Edit",
                EditorMode::Edit,
                live.mode,
                "Run the cart's edit systems",
                cx,
            ))
            .child(self.render_edit_mode_half(
                "ggo-world-mode-play",
                "Play",
                EditorMode::Play,
                live.mode,
                "Run the cart's game systems",
                cx,
            ));
        let rows = live::tool_rows(live.mailbox.tool_names(), live.tool);
        if rows.is_empty() {
            return Some(rail.into_any_element());
        }
        rail = rail.child(Divider::vertical());
        for (number, (name, on)) in rows.into_iter().enumerate() {
            // `tool_rows` drops anything a `u8` cannot number, so this
            // cast is the row's own tool number.
            let tool = number as u8;
            let weak = cx.weak_entity();
            rail = rail.child(
                // Wrapped for the `debug_selector` for the same reason the
                // Save button is: `Button` is a `RenderOnce` and records no
                // bounds of its own, and a `toggle_state` is invisible to a
                // test.
                div()
                    .flex_none()
                    .debug_selector(move || {
                        format!("ggo-world-tool-{number}-{}", toggle_suffix(on))
                    })
                    .child(
                        Button::new(("ggo-world-tool", number), SharedString::new(name))
                            .label_size(LabelSize::XSmall)
                            .toggle_state(on)
                            // The tools ARE the edit table: nothing behind
                            // them is running while the game is.
                            .disabled(playing)
                            .on_click(move |_, _window, cx| {
                                weak.update(cx, |this, cx| this.set_live_tool(tool, cx))
                                    .ok();
                            }),
                    ),
            );
        }
        Some(rail.into_any_element())
    }

    /// One half of the rail's `Edit | Play` switch.
    fn render_edit_mode_half(
        &self,
        id: &'static str,
        label: &'static str,
        mode: EditorMode,
        current: EditorMode,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = current == mode;
        div()
            .flex_none()
            .debug_selector(move || format!("{id}-{}", toggle_suffix(selected)))
            .child(
                Button::new(id, label)
                    .label_size(LabelSize::XSmall)
                    .toggle_state(selected)
                    .tooltip(ui::Tooltip::text(tooltip))
                    .on_click(
                        cx.listener(move |this, _, _window, cx| this.set_live_mode(mode, cx)),
                    ),
            )
    }

    /// The view-control row under the toolbar: the `Design | Live` switch,
    /// grid + snap toggles, the zoom bar with its `-`/`+` buttons and live
    /// readout, and "Reset".
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
            .child(self.render_mode_switch(cx))
            .child(Divider::vertical())
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
            .child({
                // The slider walks the zoom ladder: one notch per level.
                let levels = canvas::ZOOM_LEVELS;
                let last = levels.len().saturating_sub(1).max(1);
                let index = levels
                    .iter()
                    .position(|&level| (level - zoom).abs() < 1e-9)
                    .unwrap_or_else(|| levels.iter().filter(|&&level| level < zoom).count());
                let weak = cx.weak_entity();
                ui::Slider::new("ggo-world-zoom", index as f32 / last as f32)
                    .width(px(72.))
                    .on_change(move |value, _window, cx| {
                        let index = (value * last as f32).round() as usize;
                        let level = levels[index.min(levels.len() - 1)];
                        weak.update(cx, |this, cx| this.set_zoom(level, cx)).ok();
                    })
            })
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

    /// The layers rail: one row per hardware background slot of the OPEN
    /// world. Linked slots show the map's stem and a clear button; empty
    /// ones show a tileset picker whose pick generates and links a map.
    ///
    /// Base-world slots ONLY, deliberately: `open.merged` can be filled
    /// by an `[[instance]]`'d world, and offering a "clear" on a slot
    /// this document does not declare would silently do nothing (the
    /// undoable `SetBackground` only ever edits the base list).
    fn render_layers_rail(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_layers_rail is only called in the Ready state");
        };
        let backgrounds = open.store.state().backgrounds;
        // Every slot on this rail is a document edit, so the whole rail is
        // dead while the game runs -- and, as everywhere else, the wrapper
        // selectors say so rather than leaving a test to infer it from a
        // click that does nothing.
        let playing = self.live_playing();
        let mut rail = h_flex().gap_2().px_1().pb_1().flex_wrap().child(
            Label::new("Layers")
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        );
        for layer in 0..world_file::BACKGROUND_LAYER_COUNT as u8 {
            let slot = h_flex().gap_1().child(
                Label::new(format!("bg{layer}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
            rail = rail.child(match backgrounds.iter().find(|bg| bg.layer == layer) {
                Some(background) => slot
                    .child(
                        // The wrapper carries the `debug_selector` for the
                        // same reason the Save button's does: `Button` is a
                        // `RenderOnce` and records none of its own, and the
                        // label here is the map's stem -- which a test that
                        // resolved the button by label would have to
                        // hard-code twice.
                        div()
                            // `flex_none` because `ButtonLike` sets it on
                            // itself: without it this new wrapper becomes a
                            // shrinkable flex item in the wrapping rail and
                            // the label it holds could squeeze.
                            .flex_none()
                            .debug_selector(move || {
                                format!("ggo-world-bg-paint-{layer}-{}", toggle_suffix(!playing))
                            })
                            .child(
                                // The map's name IS the paint-mode entry
                                // (spec: "click a linked slot in the layers
                                // rail"), and its toggled state is what says
                                // which layer the brush is on.
                                Button::new(
                                    SharedString::from(format!("ggo-world-bg-paint-{layer}")),
                                    background.stem().to_string(),
                                )
                                .label_size(LabelSize::XSmall)
                                .toggle_state(
                                    open.mode == EditMode::Paint(PaintTarget::BgSlot(layer)),
                                )
                                .tooltip(ui::Tooltip::text("Paint this background layer"))
                                .disabled(playing)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.enter_paint_mode(PaintTarget::BgSlot(layer), cx);
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .debug_selector(move || {
                                format!("ggo-world-bg-clear-{layer}-{}", toggle_suffix(!playing))
                            })
                            .child(
                                IconButton::new(
                                    ("ggo-world-bg-clear", layer as usize),
                                    IconName::Trash,
                                )
                                .icon_size(IconSize::Small)
                                .tooltip(ui::Tooltip::text("Unlink this background layer"))
                                .disabled(playing)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.clear_background_impl(layer, cx),
                                )),
                            ),
                    )
                    .into_any_element(),
                None => {
                    let root = open.root.clone();
                    let weak = cx.weak_entity();
                    slot.child(
                        // The wrapper carries the `debug_selector`: a
                        // `PopoverMenu` trigger has to be `Toggleable`, so
                        // the marker cannot go on a div INSIDE the trigger
                        // slot, and the `Button` itself records none. Its
                        // bounds are the trigger's -- the menu is deferred.
                        div()
                            .flex_none()
                            .debug_selector(move || {
                                format!("ggo-world-bg-slot-{layer}-{}", toggle_suffix(!playing))
                            })
                            .child(
                                PopoverMenu::new(SharedString::from(format!(
                                    "ggo-world-bg-menu-{layer}"
                                )))
                                .trigger(
                                    Button::new(
                                        SharedString::from(format!("ggo-world-bg-slot-{layer}")),
                                        "Add…",
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .disabled(playing),
                                )
                                // Lazy on purpose: the tileset list is a
                                // recursive walk of the asset root, and this
                                // rail renders every frame. It also means a
                                // tileset created while the world is open
                                // shows up on the next open of the picker.
                                .menu(move |window, cx| {
                                    let tilesets = io::list_tilesets(&root);
                                    let weak = weak.clone();
                                    Some(ContextMenu::build(
                                        window,
                                        cx,
                                        move |mut menu, _window, _cx| {
                                            for til in tilesets {
                                                let weak = weak.clone();
                                                menu = menu.entry(
                                                    SharedString::from(til.clone()),
                                                    None,
                                                    move |_window, cx| {
                                                        let til = til.clone();
                                                        weak.update(cx, |this, cx| {
                                                            this.add_background_impl(layer, til, cx)
                                                        })
                                                        .ok();
                                                    },
                                                );
                                            }
                                            menu
                                        },
                                    ))
                                }),
                            ),
                    )
                    .into_any_element()
                }
            });
        }
        rail.into_any_element()
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
        let state = open.store.state();
        let screen_origin = active_camera_origin(&state);
        let world_center = canvas::camera_center(screen_origin);
        let view = open.view.clone();
        let grid = open.grid;
        let background = cx.theme().colors().editor_background;
        let marquee = open.marquee.as_ref().map(|m| {
            let x0 = m.start[0].min(m.current[0]);
            let y0 = m.start[1].min(m.current[1]);
            [
                x0,
                y0,
                (m.start[0] - m.current[0]).abs(),
                (m.start[1] - m.current[1]).abs(),
            ]
        });

        let element = if self.live_active() {
            // Cloned per render, never held across ticks: the emu panel
            // retires the frames it replaces (`LinkEndpoint::frame`).
            let frame = open
                .live
                .as_ref()
                .and_then(|live| live.frame.as_ref().map(|(_, image)| image.clone()));
            let rows = open.live.as_ref().map_or_else(Vec::new, |live| {
                live::overlay_rows(live, OpenWorld::doc_counts(&state), &open.selected)
            });
            // The band is the CART's: it decides what a drag on empty space
            // means, and the host only draws what it reports.
            let marquee = open.live.as_ref().and_then(live::LiveView::marquee_rect);
            let accent = cx.theme().colors().text_accent;
            // Read here, not in the closures: prepaint runs while this
            // entity is borrowed, so the geometry inputs travel as values.
            let scale_override = open.live.as_ref().and_then(|live| live.scale);
            let camera = open
                .live
                .as_ref()
                .map_or([0.0, 0.0], live::LiveView::overlay_camera);
            gpui::canvas(
                move |canvas_bounds, _window, _cx| {
                    // Stamped directly rather than through `layout_camera`:
                    // that one also centres the DESIGN pan, which Live must
                    // not touch. Every Live gesture reads this stamp
                    // (`live_canvas_size`), so it has to happen here.
                    view.borrow_mut().last_bounds = Some(canvas_bounds);
                    let size = [
                        f64::from(canvas_bounds.size.width),
                        f64::from(canvas_bounds.size.height),
                    ];
                    let (transform, frame_rect, _) = live::geometry(size, scale_override, camera);
                    canvas::LiveScene {
                        frame,
                        frame_rect,
                        view: transform,
                        rows,
                        marquee,
                        background,
                        accent,
                    }
                },
                move |canvas_bounds, scene, window, _cx| {
                    canvas::paint_live(&scene, canvas_bounds, window)
                },
            )
            .size_full()
            .into_any_element()
        } else {
            let items = draw_items(open);
            let paint_focus = self.paint_focus_key();
            let images = open.images.clone();
            let text_color = cx.theme().colors().text;
            gpui::canvas(
                move |canvas_bounds, _window, _cx| {
                    let (zoom, pan) = layout_camera(&view, canvas_bounds, world_center);
                    canvas::Scene {
                        items,
                        images,
                        zoom,
                        pan,
                        screen_origin,
                        marquee,
                        grid,
                        background,
                        text_color,
                        paint_focus,
                    }
                },
                move |canvas_bounds, scene, window, cx| {
                    canvas::paint_scene(&scene, canvas_bounds, window, cx)
                },
            )
            .size_full()
            .into_any_element()
        };

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
                    let Some(local) = this.canvas_local(event.position) else {
                        return;
                    };
                    // In Live the press is the cart's: it hit-tests, it
                    // selects, it decides what a second click means.
                    if this.canvas_button_down(local, MouseButton::Left, &event.modifiers) {
                        return;
                    }
                    // A double-click that opens a Tilemap entity for
                    // painting must NOT also arm a placement drag on it:
                    // the first click already selected it, and the drag
                    // would move the entity out from under the brush.
                    if event.click_count >= 2 && this.canvas_double_click(local, cx) {
                        return;
                    }
                    this.canvas_primary_down_with(local, event.modifiers.shift, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    if let Some(local) = this.canvas_local(event.position)
                        && this.canvas_button_up(local, MouseButton::Left, &event.modifiers)
                    {
                        return;
                    }
                    this.canvas_primary_up(cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    let cursor = [f64::from(event.position.x), f64::from(event.position.y)];
                    // The Live camera pan is the cart's own edit system,
                    // driven by the very same forwarded pointer.
                    if let Some(local) = this.canvas_local(event.position)
                        && this.canvas_button_down(local, MouseButton::Middle, &event.modifiers)
                    {
                        return;
                    }
                    let ViewerState::Ready(open) = &this.state else {
                        return;
                    };
                    let mut v = open.view.borrow_mut();
                    if let Some(pan) = v.pan {
                        v.drag = Some(Drag {
                            start_cursor: cursor,
                            start_pan: pan,
                        });
                    }
                }),
            )
            // Never add `.on_click` (or a drag listener) to this div: gpui
            // suppresses hover updates while a click is pending on the
            // element (`pending_mouse_down`), which is exactly the held
            // button whose release this handler has to flush. Worse than
            // suppression, the MouseMove path (`div.rs`) computes
            // `is_hovered = has_mouse_down.is_none() && ...`, so with a
            // click listener a move made WITH the button down reads as
            // "not hovered" and fires `on_hover(false)` mid-drag ON the
            // canvas -- a release the user never made.
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if *hovered {
                    return;
                }
                let mut painting = false;
                if let ViewerState::Ready(open) = &mut this.state {
                    open.view.borrow_mut().hover = None;
                    // A band released outside the canvas gets no mouse-up
                    // here; settle it as "nothing more" rather than leave
                    // it drawing.
                    if open.marquee.take().is_some() {
                        cx.notify();
                    }
                    painting = open.paint_gesture;
                }
                // Same reasoning for a stroke: the release may never come
                // back to this element, so end it at the boundary. A drag
                // that wanders off the canvas and returns is two undo
                // entries -- the price of never leaving one open.
                if painting {
                    this.end_canvas_paint(cx);
                }
                // And the same for the cart's own drag or band: gpui
                // stops delivering moves the moment the cursor is
                // outside, so a gesture left open would freeze here and
                // then jump to wherever the button came up.
                this.live_release_held();
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if let ViewerState::Ready(open) = &this.state {
                    let mut v = open.view.borrow_mut();
                    if let Some(bounds) = v.last_bounds {
                        v.hover = Some([
                            f64::from(event.position.x - bounds.origin.x),
                            f64::from(event.position.y - bounds.origin.y),
                        ]);
                    }
                }
                if let Some(local) = this.canvas_local(event.position)
                    && this.canvas_pointer_move(local, event.pressed_button, &event.modifiers)
                {
                    return;
                }
                if this.handle_pan_move(event, cx) {
                    return;
                }
                let Some(local) = this.edit_drag_local(event, cx) else {
                    return;
                };
                this.canvas_drag_to(local, cx);
            }))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                    if let Some(local) = this.canvas_local(event.position)
                        && this.canvas_button_up(local, MouseButton::Middle, &event.modifiers)
                    {
                        return;
                    }
                    if let ViewerState::Ready(open) = &this.state {
                        open.view.borrow_mut().drag = None;
                    }
                }),
            )
            // Right-button samples exist for the cart alone -- a user edit
            // system may bind it -- so these listeners do nothing in
            // Design, where the canvas has no right-button gesture.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    if let Some(local) = this.canvas_local(event.position) {
                        this.canvas_button_down(local, MouseButton::Right, &event.modifiers);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                    if let Some(local) = this.canvas_local(event.position) {
                        this.canvas_button_up(local, MouseButton::Right, &event.modifiers);
                    }
                }),
            )
            // A release the canvas is not under never reaches the handlers
            // above (gpui delivers `on_mouse_up` to the hovered element
            // only, and nothing captures the mouse here), so the cart
            // would never hear the end of a drag that left the canvas.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                    if let Some(local) = this.canvas_local(event.position) {
                        this.canvas_button_up_out(local, MouseButton::Left, &event.modifiers);
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                    if let Some(local) = this.canvas_local(event.position) {
                        this.canvas_button_up_out(local, MouseButton::Middle, &event.modifiers);
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                    if let Some(local) = this.canvas_local(event.position) {
                        this.canvas_button_up_out(local, MouseButton::Right, &event.modifiers);
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                this.wheel_zoom(event, cx);
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
        // Every control that CHANGES the document is off while the cart is
        // playing; the field editors are held read-only by
        // `ensure_inspector`.
        let playing = self.live_playing();
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
                        h_flex().gap_1().child(
                            IconButton::new(
                                SharedString::from(format!("ggo-remove-{component}")),
                                IconName::Trash,
                            )
                            .icon_size(IconSize::XSmall)
                            .disabled(playing)
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
                                    .disabled(playing)
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
                            Some(FieldKind::Asset(ext)) => {
                                let target = inspector::FieldTarget::EntityField {
                                    entity: entity_ix,
                                    component: component.clone(),
                                    field: field.clone(),
                                };
                                if let Some(editor) = editors.get(&target) {
                                    // The COMMITTED stem, not the editor's live
                                    // text: the badge answers "does the world
                                    // as saved resolve", and flickering red on
                                    // every keystroke would say nothing.
                                    let view = self.asset_field_view(entity_ix, component, field);
                                    let stem =
                                        view.as_ref().map(|v| v.stem.clone()).unwrap_or_default();
                                    let status = view
                                        .as_ref()
                                        .map(|v| v.status)
                                        .unwrap_or(inspector::AssetStatus::Empty);
                                    let jump = view.and_then(|v| v.rel);
                                    let mut row = h_flex()
                                        .gap_1()
                                        .child(Self::field_label(field.as_str()))
                                        .child(Self::editor_input(editor, cx));
                                    if status == inspector::AssetStatus::Missing {
                                        row = row.child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "ggo-asset-missing-{component}-{field}"
                                                )))
                                                .tooltip(ui::Tooltip::text(format!(
                                                    "no {stem}.{ext} under {}",
                                                    open.root.display()
                                                )))
                                                .child(
                                                    Icon::new(IconName::Warning)
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Error),
                                                ),
                                        );
                                    }
                                    if let Some(rel) = jump {
                                        row = row.child(
                                            IconButton::new(
                                                SharedString::from(format!(
                                                    "ggo-goto-asset-{component}-{field}"
                                                )),
                                                IconName::ArrowUpRight,
                                            )
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(ui::Tooltip::text(format!("Open {rel}")))
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.goto_asset(rel.clone(), window, cx);
                                                }),
                                            ),
                                        );
                                    }
                                    panel = panel.child(row);
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
            col = col.child(
                DropdownMenu::new("ggo-add-component", "Add component…", menu).disabled(playing),
            );
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
                    ggo_common::CopyableText::new(
                        "ggo-world-instance-error-copy",
                        error.to_string(),
                    )
                    .size(LabelSize::Small),
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
        let selection = open.primary()?;
        let playing = self.live_playing();
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
        // Wrapped for the `debug_selector` for the same reason the Save
        // button is, and rendered as a row of its own rather than as a
        // state of the fields: "the cart is playing" is why they are
        // inert, and a greyed field that says nothing is a bug report.
        let playing_note = playing.then(|| {
            div()
                .debug_selector(|| "ggo-world-inspector-disabled".into())
                .child(
                    Label::new("Playing — the cart owns these entities")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        });
        Some(
            div()
                .id("ggo-world-inspector")
                .w(INSPECTOR_WIDTH)
                .h_full()
                .flex_none()
                .border_l_1()
                .border_color(cx.theme().colors().border)
                .overflow_y_scroll()
                .child(v_flex().p_1().gap_1().children(playing_note).child(body))
                .into_any_element(),
        )
    }

    /// The paint column: the tool rail, the stamp controls, the terrain
    /// editor, the tileset strip (or the bind prompt) and the resize
    /// fields -- every one of them
    /// [`ggo_map_panel::paint_ui`]'s, mounted here exactly as the
    /// standalone map panel mounts them.
    ///
    /// It TAKES THE PLACE of the entity list and the inspector rather than
    /// joining them, for the same reason every entity action is gated
    /// while painting (spec: "entity manipulation is disabled"): offering
    /// entity rows and entity fields for a mode that will refuse to act on
    /// them is worse than not offering them.
    fn render_paint_column(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (rel, _) = self.active_paint_target()?;
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let session = open.sessions.get(&rel)?;
        let state = session.store.state();
        let tools = paint_ui::render_tool_rail(session, cx);
        let stamp = paint_ui::render_paint_controls(session, cx);
        let terrains = paint_ui::render_terrain_editor(session, open.terrain_name.as_ref(), cx);
        let strip = paint_ui::render_strip(session, &open.strip_bounds, cx);
        // The picker sits here only while a tileset IS bound; an unbound
        // map gets it in the strip's place instead (`render_strip`), so
        // exactly one is ever on screen.
        let bind = session
            .tileset
            .is_some()
            .then(|| paint_ui::render_bind_picker(session, cx));
        let resize = open
            .resize
            .as_ref()
            .map(|fields| paint_ui::render_resize(fields, cx));
        Some(
            div()
                .id("ggo-world-paint")
                // The column's own bounds, so a test can tell the tool
                // rail's Select button (`ICON-Maximize`) from the PANE's
                // zoom button, which shares that icon and therefore that
                // debug selector in a whole-workspace test.
                .debug_selector(|| "ggo-world-paint".into())
                .w(PAINT_WIDTH)
                .h_full()
                .flex_none()
                .border_l_1()
                .border_color(cx.theme().colors().border)
                .overflow_y_scroll()
                .child(
                    v_flex()
                        .p_1()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Label::new(rel).size(LabelSize::XSmall))
                                .child(
                                    Label::new(format!("{}x{}", state.w, state.h))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(tools)
                        .child(stamp)
                        .children(terrains)
                        .child(strip)
                        // The colors on screen then aren't the asset's
                        // own, which is worth saying out loud -- the
                        // retired standalone footer's warning, kept.
                        .children(
                            session
                                .tileset
                                .as_ref()
                                .filter(|tileset| tileset.missing_pal)
                                .map(|_| {
                                    Label::new("no .pal — 16-gray fallback")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning)
                                }),
                        )
                        .child(h_flex().gap_1().flex_wrap().children(bind).children(resize))
                        .children(session.save_error.as_ref().map(|e| {
                            ggo_common::CopyableText::new(
                                "ggo-world-paint-save-error-copy",
                                format!("save failed: {e}"),
                            )
                            .size(LabelSize::XSmall)
                        })),
                )
                .into_any_element(),
        )
    }

    /// The dock's Ready layout: toolbar, view controls, and either the
    /// entity list + inspector or the paint column. The CANVAS is
    /// deliberately absent -- it lives in the center-pane
    /// `WorldCanvasItem` (spec 2026-08-20), which calls
    /// [`Self::render_canvas`] against this same entity's state.
    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let paint = self.render_paint_column(cx);
        let entities = paint.is_none().then(|| {
            let inspector = self.render_inspector(window, cx);
            (self.render_entity_list(cx), inspector)
        });
        let toolbar = self.render_toolbar(window, cx);
        let mut body = h_flex().flex_1().min_h_0().items_stretch();
        if let Some((list, inspector)) = entities {
            body = body.child(list).children(inspector);
        }
        v_flex()
            .size_full()
            .child(toolbar)
            .children(self.render_live_status(cx))
            .child(self.render_view_controls(cx))
            .child(self.render_layers_rail(cx))
            .children(self.render_edit_rail(cx))
            .child(body.children(paint))
            .into_any_element()
    }
}

/// The world editor as a [`paint_ui::PaintHost`]. The session is looked up
/// FRESH through [`Self::active_paint_target`] on every call rather than
/// cached: an undo can unlink the slot under the brush, and a stale handle
/// would keep editing a map the document no longer references.
impl paint_ui::PaintHost for WorldPanel {
    fn paint_session(&self) -> Option<&PaintSession> {
        let (rel, _) = self.active_paint_target()?;
        match &self.state {
            ViewerState::Ready(open) => open.sessions.get(&rel),
            _ => None,
        }
    }

    fn paint_session_mut(&mut self) -> Option<&mut PaintSession> {
        let (rel, _) = self.active_paint_target()?;
        match &mut self.state {
            ViewerState::Ready(open) => open.sessions.get_mut(&rel),
            _ => None,
        }
    }

    fn paint_session_changed(&mut self, cx: &mut Context<Self>) {
        if let Some((rel, _)) = self.active_paint_target() {
            self.refresh_paint_image(&rel, cx);
        }
    }

    fn paint_project_root(&self) -> Option<PathBuf> {
        self.project_root.clone()
    }

    fn paint_resize_fields(&self) -> Option<&paint_ui::ResizeFields> {
        match &self.state {
            ViewerState::Ready(open) => open.resize.as_ref(),
            _ => None,
        }
    }

    fn paint_terrain_name(&self) -> Option<&Entity<Editor>> {
        match &self.state {
            ViewerState::Ready(open) => open.terrain_name.as_ref(),
            _ => None,
        }
    }
}

impl Render for WorldPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_inspector(window, cx);
        self.ensure_paint_fields(window, cx);
        self.refresh_stem_completion(window, cx);
        self.schedule_audio_sizes(cx);
        // The canvas item drains this too; with no canvas tab open the dock
        // is the only render, so drain here as well.
        self.retire_images(window);
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
            .on_action(cx.listener(|this, _: &SelectAll, _window, cx| this.select_all_impl(cx)))
            .on_action(
                cx.listener(|this, _: &ClearSelection, _window, cx| this.clear_selection_impl(cx)),
            )
            .on_action(cx.listener(|this, _: &Copy, _window, cx| this.copy_impl(cx)))
            .on_action(cx.listener(|this, _: &Paste, _window, cx| this.paste_impl(cx)))
            .on_action(cx.listener(|this, _: &Duplicate, _window, cx| this.duplicate_impl(cx)))
            .on_action(cx.listener(|this, _: &Redo, _window, cx| this.redo_impl(cx)))
            .on_action(cx.listener(|this, _: &Save, _window, cx| this.save_impl(cx)))
            .on_action(cx.listener(|this, _: &DeleteSelected, window, cx| {
                this.delete_selected_impl(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ResetView, _window, cx| this.reset_view_impl(cx)))
            .on_action(cx.listener(|this, _: &ToggleDesign, window, cx| {
                this.set_canvas_mode(CanvasMode::Design, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleLive, window, cx| {
                this.set_canvas_mode(CanvasMode::Live, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleCanvasMode, window, cx| {
                let next = match this.canvas_mode {
                    CanvasMode::Design => CanvasMode::Live,
                    CanvasMode::Live => CanvasMode::Design,
                };
                this.set_canvas_mode(next, window, cx)
            }))
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

#[cfg(test)]
mod tests {
    // In `src/tests/`, because a child of this INLINE `mod tests` is only
    // looked for there -- and they have to be children of it to reach the
    // helpers above (`live_of`, `overlay_of`, `routed_project`), which are
    // private to this module.
    mod cart_harness;
    mod cart_journeys;

    use super::*;
    use ggo_worldlib::world_file::{
        WorldEntity, WorldFile, WorldInstance, read_world, write_world,
    };
    use gpui::Action as _;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, WorktreeId};
    use serde_json::json;
    use workspace::dock::{DockPosition, Panel as _};
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
        ggo_common::bind_default_keymap(cx);
    }

    /// Proves the dock is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock.
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
            ggo_common::bind_default_keymap(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.panel::<WorldDock>(cx).is_some(),
                "WorldDock should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update_in(cx, |workspace, window, cx| {
            let dock = workspace
                .panel::<WorldDock>(cx)
                .expect("WorldDock should still be registered");
            assert_eq!(dock.read(cx).position(window, cx), DockPosition::Right);
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
    pub(crate) fn write_fixture(root: &std::path::Path) {
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

    /// A `WorldPanel` bound to `workspace` and reading from `root`: what
    /// the dock builds behind a world tab, for the tests that only need a
    /// panel wired to a real workspace and not the tab itself.
    fn workspace_panel(
        workspace: &Entity<Workspace>,
        root: &std::path::Path,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<WorldPanel> {
        let workspace = workspace.downgrade();
        let root = root.to_path_buf();
        cx.update(|_, cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(Some(workspace), cx);
                panel.root_override = Some(root);
                panel
            })
        })
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
            panel.load_rel_path("worlds/test.toml", None, cx);
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

    /// `mark_loading` puts a fresh panel into `Loading` before its
    /// deferred load runs; a load that then never starts must leave a
    /// TERMINAL state behind, or `remote_read` waits on "still loading"
    /// forever.
    #[gpui::test]
    async fn test_an_abandoned_load_does_not_strand_the_panel_in_loading(cx: &mut TestAppContext) {
        let panel = cx.update(|cx| cx.new(|cx| WorldPanel::new(None, cx)));
        panel.update(cx, |panel, cx| {
            // No workspace and no root override: nothing to resolve a
            // project root from, which is what `load_rel_path` gives up on.
            panel.mark_loading("worlds/test.toml", cx);
            assert!(matches!(panel.state, ViewerState::Loading { .. }));
            panel.load_rel_path("worlds/test.toml", None, cx);
            let ViewerState::Error(reason) = &panel.state else {
                panic!("an abandoned load must not stay Loading");
            };
            assert!(reason.contains("worlds/test.toml"), "{reason}");
            let read = panel.remote_read().unwrap_err();
            assert!(
                !read.contains(WORLD_STILL_LOADING),
                "the reader must get a terminal answer: {read}"
            );
        });
    }

    #[gpui::test]
    async fn test_remote_list_and_read_report_the_authored_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let listed = panel.remote_list(cx);
            let stems: Vec<&str> = listed.iter().map(|(stem, _)| stem.as_str()).collect();
            assert_eq!(stems, ["worlds/sub", "worlds/test"]);
            assert_eq!(listed[1].1, "worlds/test.toml");

            let read = panel.remote_read().expect("a Ready world reads");
            assert_eq!(read["stem"], "worlds/test");
            assert_eq!(read["rel_path"], "worlds/test.toml");
            assert_eq!(read["dirty"], false);
            assert_eq!(read["entities"].as_array().unwrap().len(), 3);
            assert_eq!(read["entities"][1]["pos"], serde_json::json!([40.0, 8.0]));
            assert_eq!(
                read["entities"][1]["components"]["Text"]["content"],
                "hello"
            );
            assert_eq!(read["instances"][0]["world"], "worlds/sub");
            assert_eq!(read["selected"].as_array().unwrap().len(), 0);

            assert!(
                panel
                    .remote_resolve("worlds/nope", cx)
                    .unwrap_err()
                    .contains("worlds/test")
            );
        });
    }

    #[test]
    fn composite_scene_blits_images_and_boxes_relative_to_the_origin() {
        let red: Arc<[u8]> = vec![
            255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ]
        .into();
        let items = vec![
            DrawItem {
                kind: DrawKind::Image {
                    image: RgbaImage {
                        rgba: red,
                        w: 2,
                        h: 2,
                    },
                },
                x: 10.0,
                y: 10.0,
                w: 2.0,
                h: 2.0,
                z: 0.0,
                order: 0,
                sel: None,
            },
            DrawItem {
                kind: DrawKind::SelectionOutline,
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
                z: 1.0,
                order: 1,
                sel: None,
            },
        ];
        let canvas = composite_scene(&items, [8.0, 8.0], 8, 8);
        // Image landed at canvas (2,2): BGRA red, opaque.
        let i = (2 * 8 + 2) * 4;
        assert_eq!(&canvas[i..i + 4], &[0, 0, 255, 255]);
        // Outline drew nothing: (0,0) stays transparent black.
        assert_eq!(&canvas[0..4], &[0, 0, 0, 0]);
        // Outside the image: untouched.
        let j = (5 * 8 + 5) * 4;
        assert_eq!(&canvas[j..j + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn composite_scene_survives_an_image_shorter_than_its_declared_size() {
        // One pixel of data for a 2x2 image: the rest must be skipped, not
        // indexed past the end.
        let short: Arc<[u8]> = vec![255u8, 0, 0, 255].into();
        let items = vec![DrawItem {
            kind: DrawKind::Image {
                image: RgbaImage {
                    rgba: short,
                    w: 2,
                    h: 2,
                },
            },
            x: 0.0,
            y: 0.0,
            w: 2.0,
            h: 2.0,
            z: 0.0,
            order: 0,
            sel: None,
        }];
        let canvas = composite_scene(&items, [0.0, 0.0], 4, 4);
        assert_eq!(&canvas[0..4], &[0, 0, 255, 255]);
        assert_eq!(&canvas[4..8], &[0, 0, 0, 0]);
    }

    #[gpui::test]
    async fn test_remote_screenshot_frames_the_device_screen_or_the_whole_scene(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            // `active_camera_origin` CENTERS the screen on the active
            // camera, so the fixture's Camera at (0,0) puts world point
            // (x, y) at canvas (x - origin.x, y - origin.y).
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready state after load");
            };
            let origin = active_camera_origin(&open.store.state());
            assert_eq!(origin, [-DEVICE_SCREEN_W / 2.0, -DEVICE_SCREEN_H / 2.0]);
            let canvas_index =
                |x: f64, y: f64| (((y - origin[1]) as usize) * 320 + (x - origin[0]) as usize) * 4;

            let (w, h, bgra) = panel.remote_screenshot(false).expect("Ready draws");
            assert_eq!((w, h), (320, 240));
            assert_eq!(bgra.len(), 320 * 240 * 4);
            // The fixture's Text at (40,8) 40x12 paints a box: a pixel
            // inside it is opaque, one far outside the scene is not.
            let inside = canvas_index(45.0, 10.0);
            assert_eq!(bgra[inside + 3], 255, "text box painted");
            let outside = canvas_index(140.0, 110.0);
            assert_eq!(bgra[outside + 3], 0, "empty world pixel");

            let (w, h, _) = panel.remote_screenshot(true).expect("full frames the bbox");
            assert!(
                w > 0 && h > 0 && w <= 320,
                "the fixture scene is smaller than a screen: {w}x{h}"
            );
        });
    }

    /// The agent surface must see the worlds of an `assets/`-rooted
    /// project (the `~/projects/wilds` layout) with nothing open yet --
    /// `world_list` is what an agent calls FIRST, before any world has
    /// been opened by hand, and `world_open` resolves the name it typed
    /// against that same listing before the dock opens a tab for it.
    #[gpui::test]
    async fn test_remote_list_and_resolve_see_an_assets_rooted_project(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(&dir.path().join("assets"));
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            let listed = panel.remote_list(cx);
            assert_eq!(
                listed,
                vec![
                    (
                        "worlds/sub".to_string(),
                        "assets/worlds/sub.toml".to_string()
                    ),
                    (
                        "worlds/test".to_string(),
                        "assets/worlds/test.toml".to_string()
                    ),
                ]
            );
            let rel = panel.remote_resolve("worlds/test", cx).unwrap();
            assert_eq!(rel, "assets/worlds/test.toml");
            panel.load_rel_path(&rel, None, cx);
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            let read = panel.remote_read().expect("the asset-rooted world reads");
            assert_eq!(read["stem"], "worlds/test");
            assert_eq!(read["rel_path"], "assets/worlds/test.toml");
        });
    }

    #[gpui::test]
    async fn test_remote_read_names_the_reason_when_nothing_is_open(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });
        panel.update(cx, |panel, _cx| {
            assert!(
                panel
                    .remote_read()
                    .unwrap_err()
                    .contains("world_open first")
            );
        });
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
            panel.load_rel_path("worlds/test.toml", None, cx);
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
            panel.load_rel_path("assets/worlds/test.toml", None, cx);
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
            assert_eq!(open.selected, vec![Selection::Entity(0)]);
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
            assert!(open.selected.is_empty(), "empty-space click deselects");
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

    /// A drag that wanders and comes back RESTORES the original position:
    /// each move writes an absolute position derived from the gesture's
    /// own anchors, so the move that returns to the start is the one that
    /// puts the entity back. Skipping "no net displacement" moves as a
    /// cheap no-op would strand the entity wherever the last off-origin
    /// move left it, and the release would commit it there.
    #[gpui::test]
    async fn test_a_drag_that_returns_to_its_start_restores_the_position(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        // Snap stays OFF: it quantizes the RESULT, so an off-grid start
        // like the fixture's [4, 4] cannot return to itself under it
        // (`dragged_pos_snaps_the_result_not_the_delta`). The contract
        // under test is the absolute restore, not the snap.
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down([10.0, 10.0], cx);
            assert_eq!(open_of(panel).selected, vec![Selection::Entity(0)]);

            // Out one tile...
            panel.canvas_drag_to([26.0, 10.0], cx);
            assert_eq!(entity_pos_of(panel, 0), [20.0, 4.0]);

            // ...and back to where the press landed.
            panel.canvas_drag_to([10.0, 10.0], cx);
            assert_eq!(
                entity_pos_of(panel, 0),
                [4.0, 4.0],
                "the return move restores the authored position"
            );
            panel.canvas_primary_up(cx);
            assert_eq!(entity_pos_of(panel, 0), [4.0, 4.0]);
        });

        // And the whole round trip is ONE undo entry, like any other drag.
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            assert!(open.store.undo(), "the gesture left one entry");
            assert_eq!(
                inspector::entity_pos(&open.store.state(), 0),
                Some([4.0, 4.0])
            );
            assert!(
                !open.store.undo(),
                "and only one -- the moves coalesced under the gesture id"
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
                assert_eq!(open.selected, vec![Selection::Entity(3)]);
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

            panel.delete_selected_now(cx);
            {
                let ViewerState::Ready(open) = &panel.state else {
                    panic!("expected Ready");
                };
                assert_eq!(open.store.state().entities.len(), 3);
                assert!(open.selected.is_empty(), "delete clears the selection");
            }

            // Stale-selection guard: nothing selected => no-op.
            panel.delete_selected_now(cx);
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
            panel.load_rel_path("worlds/a.toml", None, cx);
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
                assert_eq!(open.selected, vec![Selection::Instance(1)]);
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

    /// A failed write must be VISIBLE (`save_error` set, which the dock
    /// banner renders) and must not `mark_saved` -- the document keeps its
    /// edits and stays dirty for the next attempt.
    #[gpui::test]
    async fn test_save_failure_sets_the_error_and_keeps_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let before = std::fs::read_to_string(dir.path().join("worlds/test.toml")).unwrap();

        // The save resolves against the OPEN document's captured root;
        // repointing that root at a regular file makes the write's
        // parent-dir creation fail deterministically.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [50.0, 60.0],
                    gesture: None,
                },
                cx,
            );
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.root = blocker;

            panel.save_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                open.save_error.is_some(),
                "the failed write must surface as save_error"
            );
            assert!(
                open.store.state().dirty,
                "a failed save must not mark the document saved"
            );
        });
        assert_eq!(
            std::fs::read_to_string(dir.path().join("worlds/test.toml")).unwrap(),
            before,
            "the real file must be untouched by the failed write"
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
            open.selected = vec![Selection::Entity(0)];
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

    /// The inspector's bool checkbox commits through the same funnel every
    /// editor mutation uses: `apply_op` with a `SetField` carrying
    /// `Value::Bool` (the exact op the checkbox's `on_click` builds), and
    /// undo restores the stored value. The `field_kind` assertion pins that
    /// `Camera.is_active` really is the checkbox arm's case.
    #[gpui::test]
    async fn test_inspector_bool_checkbox_commits_set_field(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected = vec![Selection::Entity(2)];
            assert_eq!(
                inspector::field_kind(&open.schemas, "Camera", "is_active"),
                Some(&FieldKind::Bool),
                "the field must render as the checkbox arm"
            );
            assert_eq!(
                open.store.state().entities[2].components["Camera"]["is_active"],
                json!(true)
            );
            panel.apply_op(
                WorldOp::SetField {
                    entity: 2,
                    component: "Camera".to_string(),
                    field: "is_active".to_string(),
                    value: Value::Bool(false),
                },
                cx,
            );
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[2].components["Camera"]["is_active"],
                json!(false),
                "the toggle must land in the store"
            );
            assert!(open.store.state().dirty, "a toggle is an unsaved edit");

            panel.undo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[2].components["Camera"]["is_active"],
                json!(true),
                "undo must restore the stored value"
            );
        });
    }

    /// The Add-component menu's op (`AddComponent` seeded by
    /// `defaults_for`) and the remove-trash button's (`RemoveComponent`)
    /// -- both funnel through `apply_op` on the selected entity, and both
    /// are undoable.
    #[gpui::test]
    async fn test_add_and_remove_component_ops(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected = vec![Selection::Entity(0)];
            assert!(
                !open.store.state().entities[0]
                    .components
                    .contains_key("Camera"),
                "the fixture entity must not already carry the component"
            );
            let schema = open
                .schemas
                .iter()
                .find(|s| s.name == "Camera")
                .expect("builtin schemas offer Camera")
                .clone();
            panel.apply_op(
                WorldOp::AddComponent {
                    entity: 0,
                    name: "Camera".to_string(),
                    defaults: defaults_for(&schema),
                },
                cx,
            );
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Camera"],
                json!({ "is_active": true, "is_centered": true }),
                "the component must appear with its schema defaults"
            );

            panel.apply_op(
                WorldOp::RemoveComponent {
                    entity: 0,
                    name: "Camera".to_string(),
                },
                cx,
            );
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                !open.store.state().entities[0]
                    .components
                    .contains_key("Camera"),
                "remove must take the component off the entity"
            );

            panel.undo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Camera"],
                json!({ "is_active": true, "is_centered": true }),
                "undoing the remove must restore the component"
            );

            panel.undo_impl(cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                !open.store.state().entities[0]
                    .components
                    .contains_key("Camera"),
                "undoing the add must return to the original entity"
            );
        });
    }

    /// Load `worlds/test` into a panel that is the root view of a real
    /// test window, with Entity(0) selected so the inspector editors
    /// exist. Rendering in a window is what drives gpui's draw/focus
    /// cycle, which the blur-commit ordering tests below depend on.
    pub(crate) async fn ready_panel_in_window<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (gpui::Entity<WorldPanel>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
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
            panel.load_rel_path("worlds/test.toml", None, cx);
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready state after load");
            };
            open.view.borrow_mut().pan = Some([0.0, 0.0]);
            open.selected = vec![Selection::Entity(0)];
            cx.notify();
        });
        cx.run_until_parked();
        (panel, cx)
    }

    /// Spec 2026-08-20: the canvas moved to the center-pane item, so the
    /// DOCK render must not paint it (no recorded bounds), while
    /// `render_canvas` stays composable for the item to call.
    #[gpui::test]
    async fn test_dock_render_has_no_canvas_but_render_canvas_still_composes(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                open.view.borrow().last_bounds.is_none(),
                "the dock layout must not have painted the canvas"
            );
            let _element = panel.render_canvas(cx);
        });
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

    /// Committing a stem into an Asset field must feed the CANVAS, not
    /// just the store: the newly named sprite is composed into the load
    /// set and the image cache without a world reload. Picking
    /// `sprites/gg_icon` right after creating it rendered nothing
    /// otherwise -- loads were only resolved at world-open and
    /// instance-add.
    #[gpui::test]
    async fn test_committing_a_stem_composes_the_named_asset(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        // A real decodable sprite trio at the fixture's asset root (the
        // worktree root for `worlds/test.toml`), so the compose succeeds.
        {
            use ggo_worldlib::sprites::cow::{Frame, SpriteState};
            use ggo_worldlib::sprites::hw::TILE_BYTES;
            let mut pool = vec![0u8; 2 * TILE_BYTES];
            for byte in &mut pool[TILE_BYTES..] {
                *byte = 0x11;
            }
            let mut palette = [0u16; 16];
            palette[1] = 0xF800;
            let state = SpriteState {
                pool,
                tile_count: 2,
                session_tiles: std::collections::HashSet::new(),
                palette,
                frames: vec![Frame {
                    map: vec![1],
                    duration_ms: 100,
                    transform: ggo_worldlib::sprites::cow::FrameTransform::IDENTITY,
                }],
                clips: vec![],
                w_tiles: 1,
                h_tiles: 1,
                pool_shared: false,
            };
            std::fs::create_dir_all(dir.path().join("sprites")).unwrap();
            ggo_worldlib::sprites::io::save_sprite(
                dir.path(),
                "sprites/gg_icon.spr",
                &state,
                "sprites/gg_icon.til",
                "sprites/gg_icon.pal",
            )
            .unwrap();
        }
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
            editor.update(cx, |editor, cx| {
                editor.set_text("sprites/gg_icon", window, cx)
            });
            panel.commit_editor(editor.entity_id(), cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                matches!(
                    open.sprite_loads.get("sprites/gg_icon"),
                    Some(ggo_worldlib::render::Loadable::Ready(_))
                ),
                "the committed stem must be composed for the canvas"
            );
        });
    }

    /// A real CLICK on a rendered stem suggestion (not a direct
    /// `pick_stem` call) must fill and commit the field -- covers the
    /// element wiring end to end, including surviving whatever the click
    /// does to the editor's focus.
    #[gpui::test]
    async fn test_clicking_a_stem_suggestion_fills_the_field(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sprites")).unwrap();
        std::fs::write(dir.path().join("sprites/gg_icon.spr"), "").unwrap();
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
        panel.update_in(cx, |_, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("ggo-stem-suggestion-sprites/gg_icon")
            .expect("the focused empty stem field lists the project's sprites");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Sprite"]["stem"],
                json!("sprites/gg_icon"),
                "the click must commit the stem"
            );
            assert_eq!(
                editor.read(cx).text(cx),
                "sprites/gg_icon",
                "and the field shows it"
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
                ViewerState::Ready(open) => open.selected.clear(),
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
                ViewerState::Ready(open) => open.selected = vec![Selection::Entity(0)],
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
                ViewerState::Ready(open) => open.selected = vec![Selection::Entity(0)],
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
                ViewerState::Ready(open) => open.selected = vec![Selection::Entity(0)],
                _ => panic!("expected Ready"),
            }
            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowRight", false, cx);
            let after_first_run = entity_pos_of(panel, 0);

            // A canvas click on empty space: deselects, and ends the run.
            panel.canvas_primary_down([-9999.0, -9999.0], cx);
            assert!(open_of(panel).nudge_gesture.is_none());
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = vec![Selection::Entity(0)],
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
            assert!(open_of(panel).selected.is_empty());
            panel.nudge_impl("ArrowRight", false, cx);
            assert!(!open_of(panel).store.state().dirty, "no selection, no op");

            // A stale selection index (undo/redo restructure) is guarded the
            // same way `delete_selected_impl` guards its own.
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = vec![Selection::Entity(999)],
                _ => panic!("expected Ready"),
            }
            panel.nudge_impl("ArrowRight", false, cx);
            assert!(!open_of(panel).store.state().dirty);

            // A key that isn't an arrow resolves to no delta.
            match &mut panel.state {
                ViewerState::Ready(open) => open.selected = vec![Selection::Entity(0)],
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
                ViewerState::Ready(open) => open.selected = vec![Selection::Instance(0)],
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
                    open.selected = vec![Selection::Entity(0)];
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
    async fn test_asset_field_rel_resolves_the_stem_and_declines_when_missing(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // No MetaSprite on the fixture's entity 0 yet.
            assert_eq!(panel.asset_field_rel(0, META_SPRITE, "stem"), None);

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
                panel.asset_field_rel(0, META_SPRITE, "stem"),
                None,
                "an unauthored sprite offers no jump"
            );
        });

        std::fs::create_dir_all(dir.path().join("sprites")).unwrap();
        std::fs::write(dir.path().join("sprites/hero.spr"), b"not really a sprite").unwrap();

        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.asset_field_rel(0, META_SPRITE, "stem"),
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
                panel.asset_field_rel(0, SPRITE_COMPONENT, "stem"),
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
            assert_eq!(panel.asset_field_rel(0, META_SPRITE, "stem"), None);
            panel.apply_op(
                WorldOp::SetField {
                    entity: 0,
                    component: META_SPRITE.to_string(),
                    field: "stem".to_string(),
                    value: json!("sprites/nobody"),
                },
                cx,
            );
            assert_eq!(panel.asset_field_rel(0, META_SPRITE, "stem"), None);
        });
    }

    /// `asset_rel_for_stem` is the asset-root/worktree bridge; the panel
    /// test above exercises it through a worktree-rooted world, this pins
    /// the ASSET-ROOTED layout (`<worktree>/assets/...`) the F4 split
    /// introduced, where the two frames genuinely differ.
    #[gpui::test]
    fn test_asset_rel_for_stem_bridges_the_asset_root_and_the_worktree(_cx: &mut gpui::App) {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path();
        let assets = worktree.join("assets");
        std::fs::create_dir_all(assets.join("sprites")).unwrap();
        std::fs::write(assets.join("sprites/hero.spr"), b"x").unwrap();

        assert_eq!(
            asset_rel_for_stem(worktree, &assets, "sprites/hero", "spr"),
            Some("assets/sprites/hero.spr".to_string()),
            "the stem resolves under the ASSET root but is handed over worktree-relative"
        );
        assert_eq!(
            asset_rel_for_stem(worktree, &assets, "sprites/ghost", "spr"),
            None
        );
        assert_eq!(asset_rel_for_stem(worktree, &assets, "", "spr"), None);
        // An asset root outside the worktree has nothing to relativize.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(elsewhere.path().join("sprites")).unwrap();
        std::fs::write(elsewhere.path().join("sprites/hero.spr"), b"x").unwrap();
        assert_eq!(
            asset_rel_for_stem(worktree, elsewhere.path(), "sprites/hero", "spr"),
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
            panel
                .asset_field_rel(0, META_SPRITE, "stem")
                .expect("the stem resolves")
        });
        assert_eq!(rel, "sprites/hero.spr");

        // The jump routes through the path-open interceptor registry, so
        // the sprite panel's `init` is what makes a `.spr` land in a
        // center-pane sprite tab.
        cx.update(|_, cx| ggo_sprite_panel::init(cx));
        let before = workspace.read_with(cx, |workspace, cx| {
            workspace.active_pane().read(cx).items_len()
        });
        panel.update_in(cx, |panel, window, cx| panel.goto_asset(rel, window, cx));
        cx.run_until_parked();
        let after = workspace.read_with(cx, |workspace, cx| {
            workspace.active_pane().read(cx).items_len()
        });
        assert_eq!(after, before + 1, "the jump opened a sprite tab");
    }

    /// Dirty the open world so the close guard has something to protect,
    /// without going through the canvas.
    pub(crate) fn dirty_the_world(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) {
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

    /// The wiring test: a dirty world TAB in a REAL workspace makes
    /// `Workspace::prepare_to_close` (the single funnel for window close,
    /// quit and restart) prompt and, on Cancel, report `false` -- which is
    /// what `MultiWorkspace::close_window` and `zed::quit` honour. The
    /// dock itself no longer guards anything: the document lives in the
    /// tab, and it is the tab's own dirt the close flow saves.
    #[gpui::test]
    async fn test_a_dirty_world_tab_vetoes_workspace_prepare_to_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = world_dock::tests::dock_workspace(cx, dir.path()).await;
        workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::open_in_panel(workspace, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
        dirty_the_world(&panel, cx);

        let close = workspace.update_in(cx, |workspace, window, cx| {
            workspace.prepare_to_close(workspace::CloseIntent::CloseWindow, window, cx)
        });
        cx.run_until_parked();
        assert!(
            cx.has_pending_prompt(),
            "the dirty world tab must be polled by Workspace::prepare_to_close"
        );
        cx.simulate_prompt_answer("Cancel");
        assert!(
            !close.await.unwrap(),
            "Cancel in the tab's save prompt must cancel the whole close"
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
            // The routing test asserts the world's `.toml` opens as a
            // text tab; without editor::init there is no project-item
            // builder for buffers and open_path fails silently.
            editor::init(cx);
            if run_init {
                init(cx);
                ggo_common::bind_default_keymap(cx);
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
        let panel = workspace_panel(&workspace, dir.path(), cx);
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

    /// A recording `ProcRunner` + `DetachedLauncher` pair for the popout
    /// Emulate: the pack reply is canned (`pack_ok`), and both logs are
    /// returned so a test can assert what was (or was not) spawned.
    #[allow(clippy::type_complexity)]
    fn fake_popout_seams(
        pack_ok: bool,
    ) -> (
        ggo_common::ProcRunner,
        ggo_common::DetachedLauncher,
        Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>>,
        Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>>,
    ) {
        let packs: Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let launches: Arc<std::sync::Mutex<Vec<ggo_common::ProcRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = packs.clone();
        let runner: ggo_common::ProcRunner = Arc::new(move |request| {
            recorded.lock().unwrap().push(request);
            ggo_common::ProcCapture {
                ok: pack_ok,
                lines: vec!["pack output".to_string()],
            }
        });
        let recorded = launches.clone();
        let launcher: ggo_common::DetachedLauncher = Arc::new(move |request| {
            recorded.lock().unwrap().push(request);
            Ok(())
        });
        (runner, launcher, packs, launches)
    }

    /// The popout button packs the OPEN world's cart (`emd pack-ggo
    /// --world <stem>`, so the cart BOOTS to that world) and hands the
    /// artifact to the standalone `ggo-emu` as its one positional argument
    /// -- no in-IDE pane involved.
    #[gpui::test]
    async fn test_popout_builds_the_cart_then_launches_ggo_emu_on_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ggo_common::EMERALD_MANIFEST), b"").unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let (runner, launcher, packs, launches) = fake_popout_seams(true);
        panel.update(cx, |panel, cx| {
            panel.proc_runner = runner;
            panel.emu_launcher = launcher;
            panel.emulate_popout_impl(cx);
        });
        cx.executor().run_until_parked();

        let out = dir.path().join("target/ggo-emulate/worlds-test.ggo");
        {
            let packs = packs.lock().unwrap();
            assert_eq!(packs.len(), 1, "exactly one pack run");
            assert_eq!(
                packs[0].args,
                vec![
                    "pack-ggo".to_string(),
                    "--out".to_string(),
                    out.to_string_lossy().into_owned(),
                    "--world".to_string(),
                    "worlds/test".to_string(),
                    "--json".to_string(),
                ],
                "the pack argv names the out path and the boot world"
            );
            assert_eq!(
                packs[0].cwd,
                dir.path().to_path_buf(),
                "emd runs in the emerald project root"
            );
        }
        {
            let launches = launches.lock().unwrap();
            assert_eq!(launches.len(), 1, "exactly one launch");
            assert_eq!(launches[0].bin, ggo_common::DEFAULT_GGO_EMU_BIN);
            assert_eq!(
                launches[0].args,
                vec![out.to_string_lossy().into_owned()],
                "ggo-emu gets the built cart as its one positional argument"
            );
        }
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(open.popout_error.is_none());
        });
    }

    /// A failed pack surfaces on the toolbar and the launch never happens.
    #[gpui::test]
    async fn test_popout_build_failure_is_shown_and_skips_the_launch(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ggo_common::EMERALD_MANIFEST), b"").unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let (runner, launcher, _packs, launches) = fake_popout_seams(false);
        panel.update(cx, |panel, cx| {
            panel.proc_runner = runner;
            panel.emu_launcher = launcher;
            panel.emulate_popout_impl(cx);
        });
        cx.executor().run_until_parked();

        assert!(
            launches.lock().unwrap().is_empty(),
            "no launch after a failed build"
        );
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            let error = open
                .popout_error
                .as_deref()
                .expect("the failure must be surfaced");
            assert!(
                error.contains("pack output"),
                "{error:?} should carry emd's last line"
            );
        });
    }

    /// A dirty world whose save fails must NOT be built -- the popout
    /// would boot the stale file on disk (the same rule the in-pane
    /// Emulate enforces via `save_if_open_and_dirty`).
    #[gpui::test]
    async fn test_popout_is_cancelled_when_the_save_fails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ggo_common::EMERALD_MANIFEST), b"").unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let (runner, launcher, packs, launches) = fake_popout_seams(true);
        // Same deterministic write failure as the save tests: repoint the
        // doc's root at a regular file.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        panel.update(cx, |panel, cx| {
            panel.proc_runner = runner;
            panel.emu_launcher = launcher;
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [50.0, 60.0],
                    gesture: None,
                },
                cx,
            );
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.root = blocker;
            panel.emulate_popout_impl(cx);
        });
        cx.executor().run_until_parked();

        assert!(
            packs.lock().unwrap().is_empty(),
            "a failed save cancels the build"
        );
        assert!(launches.lock().unwrap().is_empty());
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
        let dock = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldDock>(cx)
                .expect("init() adds the dock")
        });
        dock.update(cx, |dock, _| {
            dock.test_root_override(dir.path().to_path_buf())
        });

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

        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
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

        // The FIRST claim opens ONLY the center-pane canvas item, focused
        // -- the toml stays closed until the world is clicked again.
        workspace.read_with(cx, |workspace, cx| {
            let canvas_items = workspace
                .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                .count();
            assert_eq!(canvas_items, 1, "one canvas item");
            assert_eq!(workspace.panes().len(), 1, "no split on first click");
            let pane = workspace.active_pane().read(cx);
            assert_eq!(pane.items_len(), 1, "the canvas tab alone");
            assert!(
                pane.active_item()
                    .and_then(|item| item.downcast::<world_canvas_item::WorldCanvasItem>())
                    .is_some(),
                "the canvas tab ends active"
            );
        });

        // A SECOND claim on the already-open, active world splits its
        // toml out to a right pane (world view left, toml text right).
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "worlds/test.toml"),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                    .count(),
                1,
                "re-claim must not duplicate the canvas item"
            );
            assert_eq!(workspace.panes().len(), 2, "the toml split opened");
            let toml_in_a_pane = workspace.panes().iter().any(|pane| {
                pane.read(cx).items().any(|item| {
                    item.project_path(cx)
                        .is_some_and(|p| p == project_path(worktree_id, "worlds/test.toml"))
                })
            });
            assert!(toml_in_a_pane, "the toml editor lives in the new pane");
        });

        // A THIRD claim reuses the existing toml pane instead of
        // stacking more splits.
        workspace.update_in(cx, |workspace, window, cx| {
            // The canvas must be the active item again for the re-click
            // semantics to trigger (the toml split took focus).
            let canvas = workspace
                .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                .next()
                .expect("canvas item");
            workspace.activate_item(&canvas, true, true, window, cx);
            workspace.intercept_path_open(
                &project_path(worktree_id, "worlds/test.toml"),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _cx| {
            assert_eq!(workspace.panes().len(), 2, "no third pane");
        });

        // The regression: the toml open as a tab in the CANVAS's own pane
        // must not be "focused in place" (that swaps the world view away)
        // -- the re-click MOVES it out to the right split.
        // Rebuild the trap: collapse back to one pane holding canvas+toml.
        workspace.update_in(cx, |workspace, window, cx| {
            let panes: Vec<_> = workspace.panes().to_vec();
            let canvas = workspace
                .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                .next()
                .expect("canvas item");
            let canvas_pane = panes
                .iter()
                .find(|pane| {
                    pane.read(cx)
                        .items()
                        .any(|item| item.item_id() == canvas.entity_id())
                })
                .cloned()
                .expect("canvas pane");
            let toml_path = project_path(worktree_id, "worlds/test.toml");
            let (toml_pane, toml_id) = panes
                .iter()
                .find_map(|pane| {
                    pane.read(cx)
                        .items()
                        .find(|item| item.project_path(cx).as_ref() == Some(&toml_path))
                        .map(|item| (pane.clone(), item.item_id()))
                })
                .expect("toml item");
            workspace::move_item(&toml_pane, &canvas_pane, toml_id, 0, false, window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(
                workspace.panes().len(),
                1,
                "moving the toml back collapsed the empty split"
            );
            let canvas = workspace
                .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                .next()
                .expect("canvas item");
            workspace.activate_item(&canvas, true, true, window, cx);
            workspace.intercept_path_open(
                &project_path(worktree_id, "worlds/test.toml"),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2, "the toml split back out");
            let toml_path = project_path(worktree_id, "worlds/test.toml");
            let canvas = workspace
                .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                .next()
                .expect("canvas item");
            let canvas_pane = workspace
                .panes()
                .iter()
                .find(|pane| {
                    pane.read(cx)
                        .items()
                        .any(|item| item.item_id() == canvas.entity_id())
                })
                .expect("canvas pane");
            assert!(
                !canvas_pane
                    .read(cx)
                    .items()
                    .any(|item| item.project_path(cx).as_ref() == Some(&toml_path)),
                "the toml LEFT the canvas pane instead of swapping over it"
            );
            assert!(
                canvas_pane
                    .read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<world_canvas_item::WorldCanvasItem>())
                    .is_some(),
                "the world view stays visible on the left"
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

    /// Each world the interceptor claims gets its OWN tab and panel: the
    /// click on the second world must not retarget the first world's
    /// panel at it.
    #[gpui::test]
    async fn the_interceptor_opens_each_world_in_its_own_tab(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = crate::world_dock::tests::dock_workspace(cx, dir.path()).await;
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let worktree_id = worktree_id(&project, cx);
        for rel in ["worlds/test.toml", "worlds/other.toml"] {
            let claimed = workspace.update_in(cx, |workspace, window, cx| {
                intercept_world_open(workspace, &project_path(worktree_id, rel), window, cx)
            });
            assert!(claimed, "{rel} is a world");
            cx.run_until_parked();
        }
        assert_eq!(dock.read_with(cx, |dock, _| dock.open_panels().len()), 2);
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                    .count(),
                2,
                "two tabs"
            );
        });
    }

    /// The second-click split follows the world that was CLICKED, not
    /// whichever canvas tab comes first in the pane: with two worlds
    /// open, re-clicking the active (second) one must still split its
    /// toml out.
    #[gpui::test]
    async fn a_second_click_splits_the_toml_of_the_clicked_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _dock, cx) = crate::world_dock::tests::dock_workspace(cx, dir.path()).await;
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let worktree_id = worktree_id(&project, cx);
        for rel in ["worlds/test.toml", "worlds/other.toml"] {
            workspace.update_in(cx, |workspace, window, cx| {
                intercept_world_open(workspace, &project_path(worktree_id, rel), window, cx)
            });
            cx.run_until_parked();
        }

        // `worlds/other.toml`'s tab is the active one: clicking it again
        // is the second click on THAT world.
        workspace.update_in(cx, |workspace, window, cx| {
            intercept_world_open(
                workspace,
                &project_path(worktree_id, "worlds/other.toml"),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2, "the toml split opened");
            let toml_path = project_path(worktree_id, "worlds/other.toml");
            assert!(
                workspace.panes().iter().any(|pane| {
                    pane.read(cx)
                        .items()
                        .any(|item| item.project_path(cx).as_ref() == Some(&toml_path))
                }),
                "the clicked world's toml is what opened"
            );
            assert_eq!(
                workspace
                    .items_of_type::<world_canvas_item::WorldCanvasItem>(cx)
                    .count(),
                2,
                "and no third canvas tab"
            );
        });
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

    /// The other two prompt branches of a dirty document switch: "Save"
    /// writes the edit and then switches, "Don't Save" switches without
    /// writing anything.
    #[gpui::test]
    async fn test_open_rel_path_save_and_discard_branches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_world(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("worlds/sub.toml", window, cx)
            })
        });
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready after the switch");
            };
            assert_eq!(open.listing.rel_path, "worlds/sub.toml", "Save then switch");
            assert!(!open.store.state().dirty, "the new document starts clean");
        });
        let on_disk = read_world(dir.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([50, 60]),
            "Save must have written the abandoned document's edit"
        );

        // Don't Save: dirty the (now open) sub world, switch back, discard.
        dirty_the_world(&panel, cx);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("worlds/test.toml", window, cx)
            })
        });
        cx.simulate_prompt_answer("Don't Save");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready after the switch");
            };
            assert_eq!(open.listing.rel_path, "worlds/test.toml");
        });
        let on_disk = read_world(dir.path(), "worlds/sub.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([0, 0]),
            "Don't Save must not write the abandoned document"
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
        let dock = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldDock>(cx)
                .expect("init() adds the dock")
        });
        dock.update(cx, |dock, _| dock.test_root_override(root.to_path_buf()));
        // The menu entries act on the ACTIVE world tab, so open one.
        workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::open_in_panel(workspace, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world("worlds/test.toml", window, cx);
            })
        });
        cx.run_until_parked();
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
        (workspace, panel, worktree_id, cx)
    }

    /// Load `rel` into the workspace's own panel and leave it Ready.
    fn open_in_menu_panel(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext, rel: &str) {
        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            panel.load_rel_path(rel, None, cx);
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

    /// Deleting a world from the explorer while ANOTHER world's tab is
    /// active must clear the tab that had the deleted file open -- the
    /// menu acts on the world it was invoked for, not on whatever the
    /// dock happens to be showing.
    #[gpui::test]
    async fn test_delete_world_clears_the_tab_that_has_it_open(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, dock, cx) = crate::world_dock::tests::dock_workspace(cx, dir.path()).await;
        for rel in ["worlds/test.toml", "worlds/other.toml"] {
            workspace.update_in(cx, |workspace, window, cx| {
                ggo_common::open_in_panel(
                    workspace,
                    window,
                    cx,
                    |dock: &mut WorldDock, window, cx| {
                        dock.open_world(rel, window, cx);
                    },
                )
            });
            cx.run_until_parked();
        }
        let panels = dock.read_with(cx, |dock, _| dock.open_panels());
        let [test_panel, other_panel] = panels.as_slice() else {
            panic!("a panel per world");
        };

        let handler = delete_world_handler(workspace.downgrade(), "worlds/test.toml".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(
            !dir.path().join("worlds/test.toml").exists(),
            "Delete must unlink the file"
        );
        test_panel.read_with(cx, |panel, _| {
            assert!(
                matches!(panel.state, ViewerState::Empty),
                "the deleted world's own tab must clear"
            );
        });
        other_panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.open_rel_path_now(),
                Some("worlds/other.toml"),
                "and the active tab keeps its own document"
            );
        });
    }

    // ----------------------------------------------- keystroke dispatch

    /// The panel-scoped bindings only fire while the panel's own focus
    /// handle has focus -- the same focus the canvas click handler takes
    /// in production -- so each keystroke test takes it explicitly.
    fn focus_the_panel(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();
    }

    fn entity0_pos(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) -> [f64; 2] {
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            inspector::entity_pos(&open.store.state(), 0).expect("entity 0 has a Transform")
        })
    }

    /// `ctrl-z`/`ctrl-shift-z` reach `undo_impl`/`redo_impl` through the
    /// keymap (the method-level history semantics have their own tests;
    /// this pins the KEYSTROKE wiring over a real `MoveEntity`).
    #[gpui::test]
    async fn test_undo_redo_keystrokes_step_a_real_move(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        dirty_the_world(&panel, cx);
        focus_the_panel(&panel, cx);

        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(
            entity0_pos(&panel, cx),
            [4.0, 4.0],
            "ctrl-z must undo the move back to the fixture position"
        );

        cx.simulate_keystrokes("ctrl-shift-z");
        assert_eq!(
            entity0_pos(&panel, cx),
            [50.0, 60.0],
            "ctrl-shift-z must redo the move"
        );
    }

    #[gpui::test]
    async fn test_save_keystroke_writes_the_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        dirty_the_world(&panel, cx);
        focus_the_panel(&panel, cx);

        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();

        let on_disk = read_world(dir.path(), "worlds/test.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([50, 60]),
            "ctrl-s must write the edited position to disk"
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.dirty_world_name().is_none(), "save clears dirty");
        });
    }

    #[gpui::test]
    async fn test_delete_keystroke_removes_the_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        focus_the_panel(&panel, cx);

        cx.simulate_keystrokes("delete");
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities.len(),
                2,
                "delete must remove the selected entity"
            );
            assert!(open.selected.is_empty(), "nothing is left selected");
        });
    }

    #[gpui::test]
    async fn test_arrow_keystrokes_nudge_the_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        focus_the_panel(&panel, cx);

        cx.simulate_keystrokes("right down");
        assert_eq!(
            entity0_pos(&panel, cx),
            [5.0, 5.0],
            "plain arrows nudge the selected entity one pixel"
        );

        cx.simulate_keystrokes("shift-left");
        assert_eq!(
            entity0_pos(&panel, cx),
            [-11.0, 5.0],
            "shift-arrow nudges one tile (16 px)"
        );
    }

    /// The same arrows with NOTHING selected pan the camera instead --
    /// opposite the key's direction (arrow right slides content left) --
    /// and leave the document alone.
    #[gpui::test]
    async fn test_arrow_keystrokes_pan_the_camera_when_nothing_is_selected(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected.clear();
            cx.notify();
        });
        cx.run_until_parked();
        focus_the_panel(&panel, cx);

        cx.simulate_keystrokes("right");
        cx.simulate_keystrokes("up");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).view.borrow().pan,
                Some([-CAMERA_PAN_STEP_PX, CAMERA_PAN_STEP_PX]),
                "arrows pan opposite the content with nothing selected"
            );
            assert!(
                panel.dirty_world_name().is_none(),
                "a camera pan must not touch the document"
            );
        });
    }

    /// Enter inside a focused inspector editor resolves through the
    /// `GgoWorldPanel > Editor` binding to `CommitField`: the typed buffer
    /// lands in the doc, and the single-line editor gets no newline.
    #[gpui::test]
    async fn test_enter_commits_the_focused_field_via_commit_field(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        let w_editor = field_editor(&panel, cx, "Text", "max_width");

        w_editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        w_editor.update_in(cx, |editor, window, cx| editor.set_text("21", window, cx));

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(
                open.store.state().entities[0].components["Text"]["max_width"],
                json!(21),
                "enter must commit the typed value"
            );
        });
        assert_eq!(
            w_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "21",
            "the field keeps the committed value -- no newline"
        );
        assert!(
            w_editor.update_in(cx, |editor, window, cx| editor
                .focus_handle(cx)
                .is_focused(window)),
            "commit leaves focus in the field"
        );
    }

    // -------------------------------------------- canvas gesture methods

    fn move_event(x: f32, y: f32, button: Option<MouseButton>) -> MouseMoveEvent {
        MouseMoveEvent {
            position: gpui::point(px(x), px(y)),
            pressed_button: button,
            modifiers: gpui::Modifiers::default(),
        }
    }

    /// Middle-drag pan: the move handler applies the cursor delta to the
    /// drag's starting pan, a release elsewhere cancels the drag (while
    /// still claiming the event), and with no drag in flight the move is
    /// not this handler's to consume.
    #[gpui::test]
    async fn test_handle_pan_move_pans_by_the_cursor_delta_and_cancels_on_release(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            open_of(panel).view.borrow_mut().drag = Some(Drag {
                start_cursor: [10.0, 10.0],
                start_pan: [5.0, 5.0],
            });

            let held = move_event(30.0, 25.0, Some(MouseButton::Middle));
            assert!(
                panel.handle_pan_move(&held, cx),
                "an in-flight pan owns the move"
            );
            assert_eq!(open_of(panel).view.borrow().pan, Some([25.0, 20.0]));

            let released = move_event(40.0, 40.0, None);
            assert!(
                panel.handle_pan_move(&released, cx),
                "the cancelling move still belongs to the pan"
            );
            assert!(
                open_of(panel).view.borrow().drag.is_none(),
                "a move without the middle button held cancels the drag"
            );
            assert_eq!(
                open_of(panel).view.borrow().pan,
                Some([25.0, 20.0]),
                "cancelling must not move the pan again"
            );

            assert!(
                !panel.handle_pan_move(&held, cx),
                "with no drag in flight the move is not a pan event"
            );
        });
    }

    /// The placement drag's window->canvas-local mapping goes through the
    /// stamped `last_bounds`; without bounds there is no local position
    /// (but the drag survives), and a move without the left button held
    /// cancels the drag.
    #[gpui::test]
    async fn test_edit_drag_local_maps_through_bounds_and_cancels_on_release(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.edit_drag = Some(EditDrag {
                gesture_id: "g1".to_string(),
                start_pos: [0.0, 0.0],
                start_world: [0.0, 0.0],
                starts: Vec::new(),
                moved: false,
            });

            let held = move_event(52.0, 30.0, Some(MouseButton::Left));
            assert_eq!(
                panel.edit_drag_local(&held, cx),
                None,
                "before the first layout there are no bounds to map through"
            );
            assert!(
                open_of(panel).edit_drag.is_some(),
                "a missing layout must not kill the drag"
            );

            open_of(panel).view.borrow_mut().last_bounds = Some(gpui::bounds(
                gpui::point(px(20.), px(10.)),
                gpui::size(px(400.), px(300.)),
            ));
            assert_eq!(
                panel.edit_drag_local(&held, cx),
                Some([32.0, 20.0]),
                "window coords map through the canvas origin"
            );

            let released = move_event(52.0, 30.0, None);
            assert_eq!(panel.edit_drag_local(&released, cx), None);
            assert!(
                open_of(panel).edit_drag.is_none(),
                "a move without the left button held cancels the drag"
            );
        });
    }

    // --------------------------------------------------------- wheel zoom

    fn wheel_event(x: f32, y: f32, dy: f32) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: gpui::point(px(x), px(y)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(dy))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::default(),
        }
    }

    /// One wheel notch steps the zoom ladder, anchored on the cursor: the
    /// world point under the cursor before the zoom is under it after
    /// ([`canvas::zoom_at`]'s invariant, here through the event plumbing).
    #[gpui::test]
    async fn test_wheel_zoom_steps_the_ladder_anchored_on_the_cursor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            open_of(panel).view.borrow_mut().last_bounds = Some(gpui::bounds(
                gpui::point(px(100.), px(50.)),
                gpui::size(px(400.), px(300.)),
            ));

            // Window (132, 66) is canvas-local [32, 16].
            panel.wheel_zoom(&wheel_event(132.0, 66.0, 4.0), cx);
            {
                let v = open_of(panel).view.borrow();
                assert_eq!(v.zoom, 2.0, "one notch up is the next ladder step");
                assert_eq!(
                    v.pan,
                    Some([-32.0, -16.0]),
                    "the world point under the cursor stays under it"
                );
            }

            panel.wheel_zoom(&wheel_event(132.0, 66.0, -4.0), cx);
            let v = open_of(panel).view.borrow();
            assert_eq!(v.zoom, 1.0);
            assert_eq!(
                v.pan,
                Some([0.0, 0.0]),
                "zooming back out at the same cursor returns home"
            );
        });
    }

    #[gpui::test]
    async fn test_wheel_zoom_no_ops_at_ladder_ends_zero_delta_and_before_layout(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Before the first layout there are no bounds to anchor on.
            panel.wheel_zoom(&wheel_event(10.0, 10.0, 4.0), cx);
            assert_eq!(open_of(panel).view.borrow().zoom, canvas::ZOOM_DEFAULT);

            open_of(panel).view.borrow_mut().last_bounds = Some(gpui::bounds(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(400.), px(300.)),
            ));

            panel.wheel_zoom(&wheel_event(10.0, 10.0, 0.0), cx);
            {
                let v = open_of(panel).view.borrow();
                assert_eq!(v.zoom, canvas::ZOOM_DEFAULT, "zero delta is a no-op");
                assert_eq!(v.pan, Some([0.0, 0.0]));
            }

            open_of(panel).view.borrow_mut().zoom = canvas::ZOOM_MAX;
            panel.wheel_zoom(&wheel_event(10.0, 10.0, 4.0), cx);
            {
                let v = open_of(panel).view.borrow();
                assert_eq!(v.zoom, canvas::ZOOM_MAX, "the top of the ladder holds");
                assert_eq!(
                    v.pan,
                    Some([0.0, 0.0]),
                    "a ladder end must not move the pan"
                );
            }

            open_of(panel).view.borrow_mut().zoom = canvas::ZOOM_MIN;
            panel.wheel_zoom(&wheel_event(10.0, 10.0, -4.0), cx);
            let v = open_of(panel).view.borrow();
            assert_eq!(v.zoom, canvas::ZOOM_MIN, "the bottom of the ladder holds");
            assert_eq!(v.pan, Some([0.0, 0.0]));
        });
    }

    /// The toolbar's audio readout: every `Music`/`Sfx` stem the world
    /// names is sized against the sample region -- a baked `.adp` by its
    /// blocks, a missing file counted as 0 and listed.
    #[gpui::test]
    async fn test_audio_budget_sizes_the_worlds_stems(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let world = WorldFile {
            entities: vec![
                entity(json!({ "Sfx": { "stem": "sfx/jump", "looping": false } })),
                entity(json!({ "Music": { "stem": "music/theme" } })),
            ],
            instances: vec![],
            backgrounds: vec![],
        };
        write_world(dir.path(), "worlds/audio.toml", &world).unwrap();
        let decoded = ggo_audio::Decoded {
            samples: vec![500; 16_000],
            rate_hz: 16_000,
            source_channels: 1,
        };
        ggo_audio::write_adp(
            dir.path(),
            "sfx/jump.adp",
            &ggo_audio::bake(&decoded, 16_000),
        )
        .unwrap();

        panel.update(cx, |panel, cx| {
            panel.load_rel_path("worlds/audio.toml", None, cx)
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.audio_budget().map(|b| b.pending),
                Some(2),
                "both stems are unsized until a render schedules them"
            );
            panel.schedule_audio_sizes(cx);
        });
        cx.executor().run_until_parked();

        panel.read_with(cx, |panel, _| {
            let budget = panel.audio_budget().expect("the world names audio");
            assert_eq!(budget.used, (16_000 / 120 + 1) * 64);
            assert_eq!(budget.missing, vec!["music/theme".to_string()]);
            assert_eq!(budget.pending, 0);
            assert!(!budget.over());
            assert_eq!(budget.label(), "audio 9 / 384 KiB · 1 missing");
        });
    }

    /// An Asset field's status follows the file on disk: a dangling stem
    /// is flagged (but stays committed), and resolves -- with a jump --
    /// once the file exists. Schema-driven, so `Sfx.stem` (`.adp`) works
    /// exactly like `MetaSprite.stem` (`.spr`).
    #[gpui::test]
    async fn test_asset_field_status_flags_dangling_stems_until_the_file_exists(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let mut defaults = serde_json::Map::new();
            defaults.insert("stem".to_string(), json!("sfx/jump"));
            panel.apply_op(
                WorldOp::AddComponent {
                    entity: 0,
                    name: "Sfx".to_string(),
                    defaults,
                },
                cx,
            );
            assert_eq!(
                panel.asset_field_status(0, "Sfx", "stem"),
                Some(inspector::AssetStatus::Missing)
            );
            assert_eq!(panel.asset_field_rel(0, "Sfx", "stem"), None);
            assert_eq!(
                panel.asset_field_stem(0, "Sfx", "stem").as_deref(),
                Some("sfx/jump"),
                "a dangling stem is still committed"
            );
            assert_eq!(
                panel.asset_field_status(0, "Sfx", "looping"),
                None,
                "only Asset fields have a status"
            );
        });

        std::fs::create_dir_all(dir.path().join("sfx")).unwrap();
        std::fs::write(dir.path().join("sfx/jump.adp"), b"x").unwrap();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.asset_field_status(0, "Sfx", "stem"),
                Some(inspector::AssetStatus::Resolves)
            );
            assert_eq!(
                panel.asset_field_rel(0, "Sfx", "stem").as_deref(),
                Some("sfx/jump.adp")
            );
        });
    }

    /// The jump is generic -- whatever interceptor claims the extension
    /// gets the path -- and DEFERRED: an interceptor that updates this very
    /// panel (what revealing a dock panel does through `set_active`) must
    /// not find it mid-update. The `.map` recorder here does exactly that.
    #[gpui::test]
    async fn test_goto_asset_routes_through_the_path_open_interceptors(cx: &mut TestAppContext) {
        thread_local! {
            static CLAIMED: std::cell::RefCell<Vec<String>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        fn recording_til(
            workspace: &mut Workspace,
            path: &ProjectPath,
            _window: &mut Window,
            cx: &mut Context<Workspace>,
        ) -> bool {
            if !path.path.extension().is_some_and(|e| e == "til") {
                return false;
            }
            let rel = ggo_common::rel_in_primary_worktree(workspace, path, cx).unwrap_or_default();
            CLAIMED.with(|c| c.borrow_mut().push(rel));
            true
        }
        fn nesting_map(
            workspace: &mut Workspace,
            path: &ProjectPath,
            _window: &mut Window,
            cx: &mut Context<Workspace>,
        ) -> bool {
            if !path.path.extension().is_some_and(|e| e == "map") {
                return false;
            }
            // What `open_in_panel::<MapPanel>` ends up doing to the dock's
            // current panel: an update of the WorldPanel the jump came
            // from, from inside the interceptor. Panics unless the jump
            // was deferred.
            let world_panel = workspace
                .panel::<WorldDock>(cx)
                .and_then(|dock| dock.read(cx).active())
                .expect("the active world tab's panel");
            world_panel.update(cx, |_, _| {});
            let rel = ggo_common::rel_in_primary_worktree(workspace, path, cx).unwrap_or_default();
            CLAIMED.with(|c| c.borrow_mut().push(rel));
            true
        }
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "worlds/test.toml");
        cx.update(|_, cx| {
            workspace::register_path_open_interceptor(cx, recording_til);
            workspace::register_path_open_interceptor(cx, nesting_map);
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.goto_asset("fonts/mono.til".to_string(), window, cx);
            panel.goto_asset("levels/arena.map".to_string(), window, cx);
        });
        cx.run_until_parked();
        CLAIMED.with(|c| {
            assert_eq!(
                c.borrow().as_slice(),
                ["fonts/mono.til", "levels/arena.map"],
                "each jump reached the interceptor that claims its extension"
            );
        });
    }

    /// The stem walk uses worldlib's hygiene: dotdirs and build dirs are
    /// skipped, so a `.git` full of stale sprites never pollutes the feed.
    #[gpui::test]
    fn test_list_asset_stems_skips_dot_and_build_dirs(_cx: &mut gpui::App) {
        let dir = tempfile::tempdir().unwrap();
        for rel in [
            "sprites/hero.spr",
            ".git/junk.spr",
            "target/old.spr",
            "sprites/map.map",
        ] {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        assert_eq!(
            list_asset_stems(dir.path(), "spr"),
            vec!["sprites/hero".to_string()]
        );
        assert_eq!(
            list_asset_stems(dir.path(), "map"),
            vec!["sprites/map".to_string()]
        );
    }

    /// Shift-click toggles membership; a plain click on a member keeps the
    /// set (and makes it primary); a plain click elsewhere replaces it.
    #[gpui::test]
    async fn test_shift_click_toggles_and_plain_click_replaces(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            assert_eq!(open_of(panel).selected, vec![Selection::Entity(0)]);
            panel.canvas_primary_down_with([50., 12.], true, cx);
            assert_eq!(
                open_of(panel).selected,
                vec![Selection::Entity(0), Selection::Entity(1)],
                "shift adds"
            );
            assert_eq!(open_of(panel).primary(), Some(Selection::Entity(1)));
            panel.canvas_primary_down_with([10., 10.], false, cx);
            assert_eq!(
                open_of(panel).selected,
                vec![Selection::Entity(1), Selection::Entity(0)],
                "a plain click on a member keeps the set and makes it primary"
            );
            panel.canvas_primary_down_with([50., 12.], true, cx);
            assert_eq!(
                open_of(panel).selected,
                vec![Selection::Entity(0)],
                "shift removes"
            );
            panel.canvas_primary_down_with([50., 12.], false, cx);
            assert_eq!(
                open_of(panel).selected,
                vec![Selection::Entity(1)],
                "plain replaces"
            );
        });
    }

    /// Empty-space drag rubber-bands: everything whose bbox the band
    /// overlaps is selected; with shift the band adds to the set.
    #[gpui::test]
    async fn test_rubber_band_selects_by_overlap(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([150., 150.], false, cx);
            assert!(open_of(panel).marquee.is_some(), "empty space arms a band");
            assert!(open_of(panel).selected.is_empty());
            panel.canvas_drag_to([0., 0.], cx);
            panel.canvas_primary_up(cx);
            let open = open_of(panel);
            assert!(open.marquee.is_none());
            assert!(
                open.selected.contains(&Selection::Entity(0)),
                "{:?}",
                open.selected
            );
            assert!(
                open.selected.contains(&Selection::Entity(1)),
                "{:?}",
                open.selected
            );
            assert!(
                open.selected.contains(&Selection::Entity(2)),
                "{:?}",
                open.selected
            );

            // A tiny band on empty space clears (not additive).
            panel.canvas_primary_down_with([150., 150.], false, cx);
            panel.canvas_drag_to([151., 151.], cx);
            panel.canvas_primary_up(cx);
            assert!(open_of(panel).selected.is_empty());

            // Additive band keeps a prior member.
            panel.canvas_primary_down_with([50., 12.], false, cx);
            panel.canvas_primary_down_with([150., 150.], true, cx);
            panel.canvas_drag_to([0., 0.], cx);
            panel.canvas_primary_up(cx);
            let selected = &open_of(panel).selected;
            assert_eq!(
                selected[0],
                Selection::Entity(1),
                "the prior member stays first"
            );
            assert!(selected.len() >= 3);
        });
    }

    /// Dragging one member of a set moves the whole set by the same delta
    /// and lands in ONE undo entry.
    #[gpui::test]
    async fn test_group_drag_moves_the_set_as_one_undo_entry(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.select_all_impl(cx);
            let before = open_of(panel).selected_positions();
            assert!(before.len() >= 3);
            panel.canvas_primary_down_with([10., 10.], false, cx);
            assert_eq!(
                open_of(panel).selected.len(),
                before.len(),
                "clicking a member keeps the set"
            );
            panel.canvas_drag_to([26., 42.], cx);
            panel.canvas_drag_to([42., 74.], cx);
            let after = open_of(panel).selected_positions();
            for (target, pos) in &before {
                let moved = after
                    .iter()
                    .find(|(t, _)| t == target)
                    .map(|(_, p)| *p)
                    .unwrap();
                assert_eq!(
                    moved,
                    [pos[0] + 32.0, pos[1] + 64.0],
                    "{target:?} moved by the delta"
                );
            }
            panel.undo_impl(cx);
            let restored = open_of(panel).selected_positions();
            for (target, pos) in &before {
                let now = restored
                    .iter()
                    .find(|(t, _)| t == target)
                    .map(|(_, p)| *p)
                    .unwrap();
                assert_eq!(now, *pos, "one undo restores {target:?}");
            }
            assert!(
                !open_of(panel).store.state().dirty,
                "one undo -> back at the save point"
            );
        });
    }

    /// Delete removes the whole set in one undo entry.
    #[gpui::test]
    async fn test_delete_removes_the_set_in_one_undo_entry(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let entities = open_of(panel).store.state().entities.len();
            let instances = open_of(panel).store.state().instances.len();
            panel.select_all_impl(cx);
            panel.delete_selected_now(cx);
            let state = open_of(panel).store.state();
            assert!(state.entities.is_empty() && state.instances.is_empty());
            assert!(open_of(panel).selected.is_empty());
            panel.undo_impl(cx);
            let state = open_of(panel).store.state();
            assert_eq!(
                (state.entities.len(), state.instances.len()),
                (entities, instances)
            );
            assert!(!state.dirty);
        });
    }

    /// `ctrl-a` selects everything, `escape` clears, and the arrow keys
    /// nudge the whole set.
    #[gpui::test]
    async fn test_select_all_escape_and_group_nudge_keys(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        focus_the_panel(&panel, cx);
        cx.simulate_keystrokes("ctrl-a");
        let (count, before) = panel.read_with(cx, |panel, _| {
            let open = open_of(panel);
            (open.selected.len(), open.selected_positions())
        });
        assert!(
            count >= 3,
            "ctrl-a selects everything, got {:?}",
            panel.read_with(cx, |panel, _| open_of(panel).selected.clone())
        );
        cx.simulate_keystrokes("right right");
        panel.read_with(cx, |panel, _| {
            let after = open_of(panel).selected_positions();
            for (target, pos) in &before {
                let now = after
                    .iter()
                    .find(|(t, _)| t == target)
                    .map(|(_, p)| *p)
                    .unwrap();
                assert_eq!(now, [pos[0] + 2.0, pos[1]], "{target:?} nudged twice");
            }
        });
        cx.simulate_keystrokes("escape");
        panel.read_with(cx, |panel, _| assert!(open_of(panel).selected.is_empty()));
    }

    /// Copy writes the selection as world-file TOML; paste with the cursor
    /// away from the canvas lands one tile right/down of the originals,
    /// as one undo entry, and selects the pasted set.
    #[gpui::test]
    async fn test_copy_and_paste_round_trip_through_the_clipboard(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_down_with([50., 12.], true, cx);
            panel.copy_impl(cx);
        });
        let text = cx
            .update(|cx| cx.read_from_clipboard().and_then(|i| i.text()))
            .unwrap();
        assert_eq!(text.matches("[[entity]]").count(), 2, "{text}");
        assert!(parse_fragment(&text).is_ok());

        panel.update(cx, |panel, cx| {
            let before = open_of(panel).store.state().entities.len();
            panel.paste_impl(cx);
            let state = open_of(panel).store.state();
            assert_eq!(state.entities.len(), before + 2);
            assert_eq!(
                inspector::transform_pos(&state.entities[before]),
                Some([4.0 + 16.0, 4.0 + 16.0]),
                "one tile right/down of the original"
            );
            assert_eq!(
                open_of(panel).selected,
                vec![Selection::Entity(before), Selection::Entity(before + 1)],
                "the pasted set is the selection"
            );
            assert!(open_of(panel).clipboard_error.is_none());
            panel.undo_impl(cx);
            assert_eq!(
                open_of(panel).store.state().entities.len(),
                before,
                "one undo"
            );
        });
    }

    /// With the cursor over the canvas the pasted group's top-left lands
    /// at the cursor (snapped when Snap is on); duplicate uses the same
    /// placement and leaves the clipboard alone.
    #[gpui::test]
    async fn test_paste_lands_at_the_cursor_and_duplicate_skips_the_clipboard(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_down_with([50., 12.], true, cx);
            {
                let open = open_of(panel);
                open.view.borrow_mut().hover = Some([100.0, 200.0]);
                let _ = open;
            }
            let before = open_of(panel).store.state().entities.len();
            panel.duplicate_impl(cx);
            let state = open_of(panel).store.state();
            assert_eq!(state.entities.len(), before + 2);
            // Group base was (4, 4); it now sits at the cursor (100, 200),
            // the second member keeps its offset (36, 4).
            assert_eq!(
                inspector::transform_pos(&state.entities[before]),
                Some([100.0, 200.0])
            );
            assert_eq!(
                inspector::transform_pos(&state.entities[before + 1]),
                Some([136.0, 204.0])
            );
        });
        assert!(
            cx.update(|cx| cx.read_from_clipboard()).is_none(),
            "duplicate never touched the clipboard"
        );
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.snap = true;
                open.view.borrow_mut().hover = Some([101.0, 203.0]);
            }
            let before = open_of(panel).store.state().entities.len();
            panel.duplicate_impl(cx);
            let state = open_of(panel).store.state();
            assert_eq!(
                inspector::transform_pos(&state.entities[before]),
                Some([96.0, 208.0]),
                "snapped to the tile grid"
            );
        });
    }

    /// A pasted instance goes through the cycle guard: the world's own
    /// stem is refused and reported, a legal one is added, moved, and
    /// resolved so it renders.
    #[gpui::test]
    async fn test_paste_refuses_cycling_instances_and_resolves_legal_ones(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let fragment = "[[instance]]\nworld = \"worlds/test\"\npos = [1, 2]\n[[instance]]\nworld = \"worlds/sub\"\npos = [3, 4]\n";
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(fragment.to_string())));
        panel.update(cx, |panel, cx| {
            let before = open_of(panel).store.state().instances.len();
            panel.paste_impl(cx);
            let open = open_of(panel);
            let state = open.store.state();
            assert_eq!(
                state.instances.len(),
                before + 1,
                "the self-instance is refused"
            );
            let pasted = &state.instances[before];
            assert_eq!(pasted.world, "worlds/sub");
            assert_eq!(pasted.pos, [3.0 + 16.0, 4.0 + 16.0]);
            assert!(pasted.resolved.is_some(), "resolved so it renders");
            assert!(
                open.clipboard_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("worlds/test"),
                "{:?}",
                open.clipboard_error
            );
            assert_eq!(open.selected, vec![Selection::Instance(before)]);
        });
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("not = [toml".into())));
        panel.update(cx, |panel, cx| {
            panel.paste_impl(cx);
            assert!(
                open_of(panel)
                    .clipboard_error
                    .as_deref()
                    .unwrap()
                    .contains("not world TOML")
            );
        });
    }

    #[gpui::test]
    fn entity_list_rows_name_entities_and_instances(_cx: &mut gpui::App) {
        let world = WorldFile {
            entities: vec![
                entity(json!({ "Transform": { "pos": [0, 0] }, "Sprite": { "stem": "hero" } })),
                entity(json!({ "Transform": { "pos": [0, 0] } })),
            ],
            instances: vec![ggo_worldlib::world_file::WorldInstance {
                world: "worlds/arena".into(),
                pos: [0.0, 0.0],
                background_priority: false,
            }],
            backgrounds: vec![],
        };
        let store = ggo_worldlib::world_doc::WorldDocStore::new(
            ggo_worldlib::world_doc::WorldDocWire::from(world),
        );
        let rows = entity_list_rows(&store.state());
        assert_eq!(
            rows[0],
            (Selection::Entity(0), "#0 Sprite · hero".to_string())
        );
        assert_eq!(rows[1], (Selection::Entity(1), "#1 Entity".to_string()));
        assert_eq!(rows[2].0, Selection::Instance(0));
        assert!(rows[2].1.starts_with("⧉ "));
    }

    /// A rebuild keeps the RenderImage (and atlas identity) of every key
    /// still loaded and retires only a key that vanished -- the property
    /// that makes retiring by key release the right tiles.
    #[gpui::test]
    async fn test_replace_images_reuses_kept_images_and_retires_dropped_keys(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let rgba = |n: u8| ggo_worldlib::render::RgbaImage {
            rgba: Arc::from(vec![n; 4]),
            w: 1,
            h: 1,
        };
        panel.update(cx, |panel, _| {
            let mut retired = Vec::new();
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.sprite_loads
                .insert("a".into(), ggo_worldlib::render::Loadable::Ready(rgba(1)));
            open.sprite_loads
                .insert("b".into(), ggo_worldlib::render::Loadable::Ready(rgba(2)));
            WorldPanel::replace_images(open, &mut retired);
            assert_eq!(open.images.len(), 2);
            assert!(retired.is_empty(), "a first build retires nothing");
            let ids: HashMap<usize, _> = open.images.iter().map(|(k, v)| (*k, v.id)).collect();

            // Rebuild with the same loads: same RenderImages, nothing retired.
            WorldPanel::replace_images(open, &mut retired);
            assert!(retired.is_empty(), "unchanged keys keep their images");
            for (key, image) in open.images.iter() {
                assert_eq!(
                    ids[key], image.id,
                    "the image for {key} was reused, not re-minted"
                );
            }

            // Drop one load: exactly its image is retired.
            let removed_key = match open.sprite_loads.get("a") {
                Some(ggo_worldlib::render::Loadable::Ready(img)) => canvas::image_key(img),
                _ => panic!("load a is ready"),
            };
            open.sprite_loads.remove("a");
            WorldPanel::replace_images(open, &mut retired);
            assert_eq!(retired.len(), 1);
            assert_eq!(retired[0].id, ids[&removed_key]);
            assert_eq!(open.images.len(), 1);
        });
    }

    /// A shift-click that removes a member must not arm a drag of the
    /// survivors; a stale instance index never prompts a delete.
    #[gpui::test]
    async fn test_shift_remove_does_not_drag_and_stale_instances_do_not_prompt(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_down_with([50., 12.], true, cx);
            assert!(open_of(panel).edit_drag.is_some());
            panel.canvas_primary_down_with([50., 12.], true, cx);
            assert_eq!(open_of(panel).selected, vec![Selection::Entity(0)]);
            assert!(
                open_of(panel).edit_drag.is_none(),
                "removing a member arms no drag"
            );
            let state = open_of(panel).store.state();
            assert_eq!(removable_instances(&[Selection::Instance(99)], &state), 0);
            assert_eq!(
                removable_instances(&[Selection::Instance(0), Selection::Entity(0)], &state),
                1
            );
        });
    }

    /// Undo/redo prune selection entries that no longer index anything.
    #[gpui::test]
    async fn test_undo_prunes_a_selection_that_outlived_its_entity(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.add_entity_impl(cx);
            let added = open_of(panel).store.state().entities.len() - 1;
            assert_eq!(open_of(panel).selected, vec![Selection::Entity(added)]);
            panel.undo_impl(cx);
            assert!(
                open_of(panel).selected.is_empty(),
                "the undone entity is no longer selectable"
            );
        });
    }

    #[gpui::test]
    fn a_rebuild_retires_exactly_the_images_the_new_cache_dropped(_cx: &mut gpui::App) {
        let image = |n: u8| ggo_common::to_render_image(&[n; 4], 1, 1).unwrap();
        let (a, b, c) = (image(1), image(2), image(3));
        let old: HashMap<usize, Arc<RenderImage>> =
            [(1, a.clone()), (2, b.clone())].into_iter().collect();
        let new: HashMap<usize, Arc<RenderImage>> = [(2, b), (3, c)].into_iter().collect();
        let retired = retired_by_rebuild(&old, &new);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].id, a.id, "only the key the new cache lost");
    }

    /// Undo then redo of an add-instance used to bring the instance back
    /// unresolved (a placeholder until reload); redo now re-resolves it.
    #[gpui::test]
    async fn test_redo_of_an_add_instance_re_resolves_its_subtree(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.add_instance_impl("worlds/sub".to_string(), cx);
            let state = open_of(panel).store.state();
            let added = state.instances.len() - 1;
            assert!(state.instances[added].resolved.is_some(), "resolved on add");
            panel.undo_impl(cx); // the move
            panel.undo_impl(cx); // the add
            assert_eq!(open_of(panel).store.state().instances.len(), added);
            panel.redo_impl(cx); // the add
            let state = open_of(panel).store.state();
            assert_eq!(state.instances.len(), added + 1);
            assert!(
                state.instances[added].resolved.is_some(),
                "redo re-resolves the subtree instead of leaving a placeholder"
            );
        });
    }

    thread_local! {
        static FLASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static FLASHED_WORLD: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    fn recording_flasher(
        _workspace: &mut Workspace,
        world: Option<&str>,
        _rebuild_gateware: bool,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        FLASHED.with(|f| f.set(f.get() + 1));
        FLASHED_WORLD.with(|w| *w.borrow_mut() = world.map(str::to_string));
        true
    }

    /// The toolbar's flash button reaches the emulator pane through the
    /// registry, never by naming it: `ggo_emu_panel` depends on this
    /// crate, so the edge only goes one way -- and it names the world this
    /// panel has open, so the board boots what is being edited instead of
    /// the manifest's `default_world`.
    #[gpui::test]
    async fn test_the_flash_button_routes_through_the_board_flasher(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        FLASHED.with(|f| f.set(0));
        FLASHED_WORLD.with(|w| *w.borrow_mut() = None);
        cx.update(|cx| ggo_common::register_board_flasher(cx, recording_flasher));
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace_panel(&workspace, dir.path(), cx);

        // Nothing open yet: no world to name, and the project's own
        // default stands.
        panel.update_in(cx, |panel, window, cx| panel.flash_impl(false, window, cx));
        cx.run_until_parked();
        assert_eq!(
            FLASHED.with(|f| f.get()),
            1,
            "the button reached the registered flasher"
        );
        assert_eq!(FLASHED_WORLD.with(|w| w.borrow().clone()), None);

        // `routed_project` wrote the fixture into `dir`; the workspace's
        // worktree is a fake fs, so the load reads through the override.
        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            panel.load_rel_path("worlds/test.toml", None, cx);
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.flash_impl(false, window, cx));
        cx.run_until_parked();
        assert_eq!(
            FLASHED_WORLD.with(|w| w.borrow().clone()),
            Some("worlds/test".to_string()),
            "the open document's stem is what the board boots"
        );
    }

    // ------------------------------------------------------- layers rail

    /// A real 4-tile `.til`/`.pal` pair, so the maps the add-layer flow
    /// binds to it actually COMPOSE (the world fixture itself is
    /// deliberately image-free -- see `write_fixture`).
    fn write_test_tileset(root: &std::path::Path, til_rel: &str) {
        use ggo_worldlib::sprites::palette565::PAL_SLOTS;
        use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;

        const TILES: usize = 4;
        let mut indices = vec![0u8; TILES * TILE_PIXELS];
        for (tile, chunk) in indices.chunks_exact_mut(TILE_PIXELS).enumerate() {
            chunk.fill((tile % PAL_SLOTS) as u8);
        }
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800; // pure 565 red
        io::save_tileset(root, til_rel, &indices, TILES, &palette).unwrap();
    }

    #[test]
    fn background_map_rel_strips_worlds_prefix_and_keeps_nesting() {
        assert_eq!(background_map_rel("worlds/main", 0), "maps/main.bg0.map");
        assert_eq!(
            background_map_rel("worlds/nested/arena", 3),
            "maps/nested/arena.bg3.map"
        );
        assert_eq!(background_map_rel("main", 1), "maps/main.bg1.map");
    }

    #[gpui::test]
    async fn test_add_background_writes_bound_map_links_slot_and_undo_unlinks_only(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");

        panel.update(cx, |panel, cx| {
            assert!(
                open_of(panel).images.is_empty(),
                "the fixture world has no image assets before a layer is added"
            );
            panel.add_background_impl(1, "tiles/bg.til".into(), cx);
            assert_eq!(
                panel.test_backgrounds(),
                vec![Background {
                    layer: 1,
                    map: "maps/test.bg1.map".into()
                }]
            );
            assert!(panel.test_is_dirty());
            let open = open_of(panel);
            assert_eq!(
                open.merged
                    .iter()
                    .map(|m| m.stem.as_str())
                    .collect::<Vec<_>>(),
                ["maps/test.bg1"],
                "the merged set must include the freshly linked slot"
            );
            assert!(
                matches!(
                    open.map_loads.get("maps/test.bg1"),
                    Some(ggo_worldlib::render::Loadable::Ready(_))
                ),
                "the new background must have composed from disk"
            );
            assert!(
                !open.images.is_empty(),
                "the canvas image cache must have picked it up"
            );
        });
        // The map exists on disk, blank, bound to the picked tileset.
        let data = io::open_map(dir.path(), "maps/test.bg1.map").unwrap();
        assert_eq!(data.til_path, "tiles/bg.til");
        assert_eq!(data.pal_path, "tiles/bg.pal");

        panel.update(cx, |panel, cx| {
            panel.undo_impl(cx);
            assert!(panel.test_backgrounds().is_empty(), "undo unlinks the slot");
            assert!(
                open_of(panel).merged.is_empty(),
                "undo must re-merge, not leave the canvas painting a dropped slot"
            );
        });
        assert!(
            io::open_map(dir.path(), "maps/test.bg1.map").is_ok(),
            "undo never deletes the file (accepted orphan)"
        );
        panel.update(cx, |panel, cx| {
            panel.redo_impl(cx);
            assert_eq!(
                panel.test_backgrounds().len(),
                1,
                "redo relinks the EXISTING file"
            );
            assert_eq!(open_of(panel).merged.len(), 1, "redo must re-merge too");
        });
    }

    #[gpui::test]
    async fn test_add_background_links_an_existing_map_without_overwriting(
        cx: &mut TestAppContext,
    ) {
        use ggo_worldlib::sprites::map_doc::{MapState, pack_cell};

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");

        let mut cells = ggo_worldlib::sprites::map_doc::blank_map_state(8, 8);
        cells[9] = pack_cell(2, 0, false, false);
        io::save_map(
            dir.path(),
            "maps/test.bg0.map",
            &MapState {
                w: 8,
                h: 8,
                cells: cells.clone(),
                til_path: "tiles/bg.til".to_string(),
                pal_path: "tiles/bg.pal".to_string(),
                dirty: false,
            },
        )
        .unwrap();

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            assert_eq!(
                panel.test_backgrounds(),
                vec![Background {
                    layer: 0,
                    map: "maps/test.bg0.map".into()
                }]
            );
        });

        let data = io::open_map(dir.path(), "maps/test.bg0.map").unwrap();
        assert_eq!(
            (data.w, data.h),
            (8, 8),
            "linking must not resize an existing map"
        );
        assert_eq!(data.cells, cells, "linking must not blank painted cells");
    }

    /// A present-but-undecodable `.map` is an ERROR, not an absence: the
    /// add must leave its bytes alone (the re-link promise is exactly
    /// about not destroying painted work), must not link the slot to a
    /// file nothing can compose, and must say why.
    #[gpui::test]
    async fn test_add_background_over_a_corrupt_map_reports_and_writes_nothing(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");

        let map_path = dir.path().join("maps").join("test.bg0.map");
        std::fs::create_dir_all(dir.path().join("maps")).unwrap();
        let garbage = b"this is not a .map file".to_vec();
        std::fs::write(&map_path, &garbage).unwrap();

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            assert!(
                panel.test_backgrounds().is_empty(),
                "a map that will not open must not be linked"
            );
            assert!(
                !panel.test_is_dirty(),
                "and no op reached the document at all"
            );
            let error = open_of(panel)
                .save_error
                .as_deref()
                .expect("the failure has to reach the user");
            assert!(
                error.contains("maps/test.bg0.map"),
                "the error names the file: {error}"
            );
        });
        assert_eq!(
            std::fs::read(&map_path).unwrap(),
            garbage,
            "the corrupt file's bytes must survive untouched"
        );
    }

    #[gpui::test]
    async fn test_clear_background_unlinks_and_save_persists_the_toml(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(2, "tiles/bg.til".into(), cx);
            panel.save_impl(cx);
        });
        // Draw the dock with the rail in its MIXED state -- one linked
        // slot (label + clear button) and three empty ones (picker
        // triggers) -- so both arms of the rail are exercised for real.
        cx.run_until_parked();
        assert_eq!(
            read_world(dir.path(), "worlds/test.toml")
                .unwrap()
                .backgrounds,
            vec![Background {
                layer: 2,
                map: "maps/test.bg2.map".into()
            }]
        );

        panel.update(cx, |panel, cx| {
            panel.clear_background_impl(2, cx);
            assert!(panel.test_backgrounds().is_empty());
            assert!(
                open_of(panel).merged.is_empty(),
                "clearing must re-merge the canvas set"
            );
            panel.save_impl(cx);
        });
        assert!(
            read_world(dir.path(), "worlds/test.toml")
                .unwrap()
                .backgrounds
                .is_empty()
        );
        assert!(
            io::open_map(dir.path(), "maps/test.bg2.map").is_ok(),
            "clearing unlinks the slot; it never deletes the map"
        );
    }

    // -------------------------------------------------------- paint mode

    #[gpui::test]
    async fn test_paint_mode_loads_a_session_and_brush_edits_the_map_doc(cx: &mut TestAppContext) {
        use ggo_worldlib::sprites::map_doc::{CELL_BLANK, pack_cell};

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        focus_the_panel(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            // Nothing selected: a paint-mode click must not put an entity
            // back in the selection.
            panel.clear_selection_impl(cx);
            assert!(
                panel.test_enter_paint_bg(0, cx),
                "a linked slot resolves to a map"
            );
            assert_eq!(
                panel.test_paint_mode_rel(),
                None,
                "the session loads off-thread; the mode waits for it"
            );
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/test.bg0.map"),
                "the loaded session put the panel in paint mode"
            );
            // Identity camera (`ready_panel_in_window` pins pan/zoom), and
            // a background anchors at the world origin, so canvas px are
            // world px are `TILE_PX`-scaled cells. [10, 10] also sits on
            // the fixture's first Text entity: in entity mode this exact
            // click selects it.
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert!(
                open_of(panel).selected.is_empty(),
                "entity hit-testing is off while painting"
            );
            let session = panel
                .test_paint_session("maps/test.bg0.map")
                .expect("the session stays cached under its rel");
            let state = session.store.state();
            assert_eq!(
                state.cells[0],
                pack_cell(0, 0, false, false),
                "the brush stamped cell (0, 0)"
            );
            assert!(session.dirty(), "and the document knows it changed");
            assert!(
                matches!(
                    open_of(panel).map_loads.get("maps/test.bg0"),
                    Some(Loadable::Ready(_))
                ),
                "the live compose replaced the on-disk image"
            );
            assert!(
                panel.paint_focus_key().is_some(),
                "and the canvas knows which image to hold at full strength"
            );
        });

        // Escape leaves paint mode; the session (and its undo history)
        // survives in the map.
        cx.simulate_keystrokes("escape");
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.test_paint_mode_rel(), None, "escape exits the mode");
            assert!(
                panel.test_paint_session("maps/test.bg0.map").is_some(),
                "the session outlives the mode"
            );
            assert!(
                panel.test_enter_paint_bg(0, cx),
                "re-entry reuses the cached session"
            );
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/test.bg0.map"),
                "a cached session enters synchronously -- no second load"
            );
            panel.undo_impl(cx);
            let session = panel.test_paint_session("maps/test.bg0.map").unwrap();
            assert_eq!(
                session.store.state().cells[0],
                CELL_BLANK,
                "undo is mode-scoped: it reversed the brush, not the world edit"
            );
            assert!(!session.dirty());
            assert_eq!(
                panel.test_backgrounds().len(),
                1,
                "the world's own slot list must be untouched by a paint undo"
            );
        });

        // Unlinking the slot under the brush hands the canvas back to the
        // world editor -- and the next undo with it, which is the whole
        // point: a mode whose target is gone must not swallow the step
        // that would bring it back.
        panel.update(cx, |panel, cx| {
            panel.clear_background_impl(0, cx);
            assert_eq!(
                panel.test_paint_mode_rel(),
                None,
                "a cleared slot cannot stay under the brush"
            );
            panel.undo_impl(cx);
            assert_eq!(
                panel.test_backgrounds().len(),
                1,
                "and the undo reached the world store, relinking the slot"
            );
        });
    }

    /// The slot dies while its session load is still in flight (a clear,
    /// or an undo of the add that linked it). The session installs -- it
    /// is a document, and its rel is still its rel -- but the mode must
    /// not flip into a target that no longer resolves, and a mode the
    /// user's own later edit withdrew is not an error to report.
    #[gpui::test]
    async fn test_a_session_landing_after_its_target_died_stays_in_entity_mode(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            assert!(panel.test_enter_paint_bg(0, cx));
            assert_eq!(
                panel.test_paint_mode_rel(),
                None,
                "the session loads off-thread; the mode waits for it"
            );
            panel.clear_background_impl(0, cx);
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.test_paint_mode_rel(),
                None,
                "a target that died mid-load must not become a dead paint mode"
            );
            assert!(
                !panel.in_paint_mode(),
                "and the panel is back on the entity tools, not stuck in Paint"
            );
            assert!(
                panel.test_paint_session("maps/test.bg0.map").is_some(),
                "the loaded session still installs, ready for a re-entry"
            );
            assert!(
                open_of(panel).paint_error.is_none(),
                "a mode the user's own edit withdrew is not a failure"
            );
        });
    }

    /// A drag that began somewhere ELSE -- the tileset strip, a palette
    /// slider, another pane -- crosses the canvas with the left button
    /// still held. Paint mode must stamp nothing on entry: only the
    /// canvas's own primary-down arms a stroke. Then the ordinary
    /// down-drag-up still paints, and the whole drag is one undo entry.
    #[gpui::test]
    async fn test_a_held_button_move_without_a_canvas_press_paints_nothing(
        cx: &mut TestAppContext,
    ) {
        use ggo_worldlib::sprites::map_doc::{CELL_BLANK, pack_cell};

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        focus_the_panel(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            assert!(panel.test_enter_paint_bg(0, cx));
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/test.bg0.map")
            );
            // Canvas pinned at the window origin, so window px are canvas
            // px are (identity camera) world px.
            open_of(panel).view.borrow_mut().last_bounds = Some(gpui::bounds(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(400.), px(300.)),
            ));

            let held = move_event(10., 10., Some(MouseButton::Left));
            assert_eq!(
                panel.edit_drag_local(&held, cx),
                None,
                "no canvas press armed this drag, so no move of it paints"
            );
            let session = panel.test_paint_session("maps/test.bg0.map").unwrap();
            assert!(
                session.store.state().cells.iter().all(|c| *c == CELL_BLANK),
                "a drag that began elsewhere must not stamp on entry"
            );
            assert!(!session.dirty(), "and the document stays clean");
        });

        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            let held = move_event(26., 10., Some(MouseButton::Left));
            let local = panel
                .edit_drag_local(&held, cx)
                .expect("the canvas press armed this stroke");
            panel.canvas_drag_to(local, cx);
            panel.canvas_primary_up(cx);

            let state = panel
                .test_paint_session("maps/test.bg0.map")
                .unwrap()
                .store
                .state();
            let stamp = pack_cell(0, 0, false, false);
            assert_eq!(state.cells[0], stamp, "the press painted cell (0, 0)");
            assert_eq!(state.cells[1], stamp, "the drag painted cell (1, 0)");

            let after = move_event(42., 10., Some(MouseButton::Left));
            assert_eq!(
                panel.edit_drag_local(&after, cx),
                None,
                "the release disarmed the stroke; a held move after it is inert"
            );
        });

        panel.update(cx, |panel, cx| {
            panel.undo_impl(cx);
            let state = panel
                .test_paint_session("maps/test.bg0.map")
                .unwrap()
                .store
                .state();
            assert!(
                state.cells.iter().all(|c| *c == CELL_BLANK),
                "press and drag folded into ONE undo entry"
            );
        });
    }

    /// Spec: "entity manipulation is disabled" while painting -- and the
    /// selection SURVIVES the mode, so the actions themselves have to be
    /// what goes inert. The keyboard is the path that matters: delete and
    /// the arrow keys reach a live selection with no canvas gesture in
    /// sight.
    #[gpui::test]
    async fn test_entity_actions_are_inert_while_painting(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        focus_the_panel(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            // Written out, so the world store is clean going in: any
            // entity edit that slips through shows up as dirty.
            panel.save_impl(cx);
            assert!(panel.test_enter_paint_bg(0, cx));
        });
        cx.run_until_parked();

        let before = panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/test.bg0.map")
            );
            assert!(!panel.test_is_dirty(), "the world is written and clean");
            let open = open_of(panel);
            assert_eq!(
                open.selected,
                vec![Selection::Entity(0)],
                "the fixture's selection survives entry -- exiting has to \
                 put the user back where they were"
            );
            (open.store.state(), open.selected.clone())
        });

        cx.simulate_keystrokes("delete");
        cx.simulate_keystrokes("right shift-down");
        panel.update(cx, |panel, cx| {
            panel.select_all_impl(cx);
            panel.duplicate_impl(cx);
            panel.copy_impl(cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let open = open_of(panel);
            let (state, selected) = &before;
            assert_eq!(
                open.store.state().entities,
                state.entities,
                "no entity moved, and none was deleted or duplicated"
            );
            assert_eq!(open.store.state().instances.len(), state.instances.len());
            assert_eq!(&open.selected, selected, "select-all is inert too");
            assert!(
                !panel.test_is_dirty(),
                "the world store never saw an edit while the brush was out"
            );
        });

        // Leaving the mode hands every one of them back.
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("right");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                inspector::entity_pos(&open_of(panel).store.state(), 0),
                Some([5.0, 4.0]),
                "the same arrow key nudges again once painting is over"
            );
        });
    }

    /// The paint tool surface, driven the way a user drives it: the tool
    /// rail's Select button really is on screen and really switches the
    /// tool, a canvas drag under it settles a CELL selection, Escape drops
    /// that selection without dropping the mode, and delete blanks exactly
    /// the selected cells with ONE undo putting them back.
    ///
    /// (The escape-clears-first branch had nothing to clear until this
    /// task: no tool could make a session selection before the rail
    /// existed.)
    #[gpui::test]
    async fn test_paint_tool_rail_select_marquee_escape_and_delete(cx: &mut TestAppContext) {
        use ggo_worldlib::sprites::map_doc::{CELL_BLANK, pack_cell};

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        focus_the_panel(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            panel.clear_selection_impl(cx);
            assert!(panel.test_enter_paint_bg(0, cx));
        });
        cx.run_until_parked();

        // Identity camera: canvas px are world px are TILE_PX-scaled cells.
        let paint_cell = |panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext, x: f64| {
            panel.update(cx, |panel, cx| {
                panel.canvas_primary_down_with([x, 10.], false, cx);
                panel.canvas_primary_up(cx);
            });
        };
        paint_cell(&panel, cx, 10.);
        paint_cell(&panel, cx, 26.);
        cx.run_until_parked();
        let painted = pack_cell(0, 0, false, false);
        panel.read_with(cx, |panel, _| {
            let cells = paint_cells(panel);
            assert_eq!(
                (cells[0], cells[1]),
                (painted, painted),
                "two cells painted"
            );
        });

        // The tool rail is on screen in paint mode, and its Select button
        // is a real click target (`IconName::Maximize`, which nothing else
        // in the dock draws).
        let button = cx
            .debug_bounds("ICON-Maximize")
            .expect("the Select tool button painted in paint mode");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                paint_session_of(panel).tool,
                ggo_map_panel::MapTool::Select,
                "the rail's button switched the tool"
            );
        });

        let drag_both = |panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext| {
            panel.update(cx, |panel, cx| {
                panel.canvas_primary_down_with([10., 10.], false, cx);
                panel.canvas_drag_to([26., 10.], cx);
                panel.canvas_primary_up(cx);
            });
        };
        drag_both(&panel, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                paint_session_of(panel).selection,
                Some((0, 0, 1, 0)),
                "the drag settled a two-cell selection"
            );
        });

        cx.simulate_keystrokes("escape");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                paint_session_of(panel).selection,
                None,
                "escape clears the selection first"
            );
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/test.bg0.map"),
                "and does NOT leave the mode while there was one to clear"
            );
        });

        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let cells = paint_cells(panel);
            assert_eq!(
                (cells[0], cells[1]),
                (painted, painted),
                "delete with nothing selected is a no-op"
            );
        });

        drag_both(&panel, cx);
        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let cells = paint_cells(panel);
            assert_eq!(
                (cells[0], cells[1]),
                (CELL_BLANK, CELL_BLANK),
                "delete blanked exactly the selected cells"
            );
            assert_eq!(cells[2], CELL_BLANK, "and nothing outside them");
        });

        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let cells = paint_cells(panel);
            assert_eq!(
                (cells[0], cells[1]),
                (painted, painted),
                "one undo restored both -- the delete is ONE rect fill"
            );
        });
    }

    /// Copy/paste of CELLS, through the panel-level clipboard: the
    /// clipboard is a `Stamp` on the panel, so it outlives leaving and
    /// re-entering paint mode (and, on a real world, switching layers).
    #[gpui::test]
    async fn test_paint_copy_paste_round_trips_through_the_cell_clipboard(cx: &mut TestAppContext) {
        use ggo_worldlib::sprites::map_doc::pack_cell;

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        focus_the_panel(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            panel.clear_selection_impl(cx);
            assert!(panel.test_enter_paint_bg(0, cx));
        });
        cx.run_until_parked();

        let painted = pack_cell(0, 0, false, false);
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            paint_session_mut_of(panel).set_tool(ggo_map_panel::MapTool::Select);
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert_eq!(paint_session_of(panel).selection, Some((0, 0, 0, 0)));
            panel.copy_impl(cx);
            assert!(
                panel.cell_clipboard.is_some(),
                "copy in paint mode fills the CELL clipboard, not the world one"
            );
        });

        // Leaving and re-entering the mode must not cost the clipboard.
        // Two escapes: the first drops the cell selection, the second the
        // mode.
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("escape");
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.test_paint_mode_rel(), None);
            assert!(panel.test_enter_paint_bg(0, cx));
            // Where a paste lands: the cell under the cursor.
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            open.view.borrow_mut().hover = Some([34., 18.]);
            panel.paste_impl(cx);
            let session = paint_session_of(panel);
            let state = session.store.state();
            assert_eq!(
                state.cells[state.w as usize + 2],
                painted,
                "the paste landed under the cursor -- cell (2, 1)"
            );
            assert_eq!(
                session.selection,
                Some((2, 1, 2, 1)),
                "and the paste is what is selected afterwards"
            );
        });
    }

    /// A slot linked to a map with no tileset: paint mode opens, painting
    /// is inert, and the bind picker's pick is what makes it paintable --
    /// spec 2026-08-29's "paint mode opens with a bind-tileset prompt".
    #[gpui::test]
    async fn test_an_unbound_paint_target_binds_from_the_picker(cx: &mut TestAppContext) {
        use ggo_map_panel::paint_ui::PaintHost as _;
        use ggo_worldlib::sprites::map_doc::CELL_BLANK;

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        io::save_new_map(dir.path(), "maps/loose.map", 8, 8).unwrap();
        focus_the_panel(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::SetBackground {
                    layer: 0,
                    map: Some("maps/loose.map".to_string()),
                },
                cx,
            );
            assert!(panel.test_enter_paint_bg(0, cx));
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            let session = paint_session_of(panel);
            assert!(session.tileset.is_none(), "the map is unbound");
            assert!(!session.can_paint(), "so painting is inert");
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert_eq!(
                paint_session_of(panel).store.state().cells[0],
                CELL_BLANK,
                "an unbound map must not collect indices nothing can resolve"
            );

            // The picker's pick.
            panel.bind_paint_tileset("tiles/bg.til".to_string(), cx);
            let session = paint_session_of(panel);
            assert_eq!(session.store.state().til_path, "tiles/bg.til");
            assert!(session.tileset.is_some(), "and the strip has tiles now");

            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert_ne!(
                paint_session_of(panel).store.state().cells[0],
                CELL_BLANK,
                "a bound map paints"
            );
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(
                    open_of(panel).map_loads.get("maps/loose"),
                    Some(Loadable::Ready(_))
                ),
                "the bind recomposed what the canvas draws"
            );
        });
    }

    /// The session under the brush.
    fn paint_session_of(panel: &WorldPanel) -> &PaintSession {
        use ggo_map_panel::paint_ui::PaintHost as _;
        panel.paint_session().expect("a session is under the brush")
    }

    fn paint_session_mut_of(panel: &mut WorldPanel) -> &mut PaintSession {
        use ggo_map_panel::paint_ui::PaintHost as _;
        panel
            .paint_session_mut()
            .expect("a session is under the brush")
    }

    fn paint_cells(panel: &WorldPanel) -> Vec<u16> {
        paint_session_of(panel).store.state().cells
    }

    #[gpui::test]
    async fn test_paint_mode_on_a_missing_map_is_an_error_state_not_a_crash(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::SetBackground {
                    layer: 0,
                    map: Some("maps/ghost.map".to_string()),
                },
                cx,
            );
            assert!(
                panel.test_enter_paint_bg(0, cx),
                "the slot names a map, so entry is attempted"
            );
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.test_paint_mode_rel(),
                None,
                "a failed load must leave the panel in entity mode"
            );
            assert!(
                panel.test_paint_session("maps/ghost.map").is_none(),
                "and cache nothing"
            );
            let error = open_of(panel)
                .paint_error
                .clone()
                .expect("the failure is surfaced, not swallowed");
            assert!(error.contains("ghost"), "with the reason: {error}");
            // The canvas still routes to entities.
            panel.canvas_primary_down_with([10., 10.], false, cx);
            assert_eq!(
                open_of(panel).selected.len(),
                1,
                "entity editing is still live after a refused entry"
            );
        });

        // An empty slot has nothing to paint at all.
        panel.update(cx, |panel, cx| {
            panel.clear_background_impl(0, cx);
            assert!(!panel.test_enter_paint_bg(0, cx), "no map, no paint mode");
        });
    }

    /// Spec 2026-08-29: "Paint tiles" on a `Tilemap` entity edits THAT
    /// entity's map, and the cell grid has to start where the composed
    /// image starts -- `render.rs::push_tilemap_item` draws it at
    /// `Transform.pos + (col, row) * TILE_PX`. So the anchor pixel is
    /// cell (0, 0), while the entity's own `Transform.pos` -- one tile to
    /// the left with `col: 1` -- is cell (-1, 0), off the map entirely.
    #[gpui::test]
    async fn test_tilemap_entity_paints_its_map_anchored_at_the_composed_image(
        cx: &mut TestAppContext,
    ) {
        use ggo_worldlib::sprites::map_doc::{CELL_BLANK, pack_cell};

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        io::save_new_bound_map(dir.path(), "maps/deco.map", 16, 16, "tiles/bg.til").unwrap();
        write_world(
            dir.path(),
            "worlds/deco.toml",
            &WorldFile {
                entities: vec![
                    entity(json!({
                        "Transform": { "pos": [32.0, 16.0], "z": 0.0 },
                        "Tilemap": { "stem": "maps/deco", "col": 1.0, "row": 0.0 }
                    })),
                    entity(json!({
                        "Transform": { "pos": [0.0, 0.0], "z": 0.0 },
                        "Camera": { "is_active": true }
                    })),
                ],
                instances: vec![],
                backgrounds: vec![],
            },
        )
        .unwrap();
        panel.update(cx, |panel, cx| {
            panel.load_rel_path("worlds/deco.toml", None, cx)
        });
        cx.run_until_parked();
        panel.update(cx, |panel, _cx| {
            open_of(panel).view.borrow_mut().pan = Some([0.0, 0.0]);
        });
        focus_the_panel(&panel, cx);

        panel.update(cx, |panel, cx| {
            assert!(
                !panel.test_enter_paint_entity(1, cx),
                "an entity with no Tilemap is a refusal, not a paint target"
            );
            assert!(
                !panel.test_enter_paint_entity(9, cx),
                "and neither is an index no entity occupies"
            );
            assert!(
                panel.test_enter_paint_entity(0, cx),
                "the Tilemap entity resolves to its map"
            );
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/deco.map"),
                "the loaded session put the brush on the entity's own map"
            );
            // Identity camera (`ready_panel_in_window` pins pan/zoom, and
            // the load above was re-pinned), so canvas px are world px.
            panel.canvas_primary_down_with([32., 16.], false, cx);
            panel.canvas_primary_up(cx);
            assert_eq!(
                paint_cells(panel)[0],
                CELL_BLANK,
                "the entity's own pos is a tile left of the map's first cell"
            );
            panel.canvas_primary_down_with([48., 16.], false, cx);
            panel.canvas_primary_up(cx);
            assert_eq!(
                paint_cells(panel)[0],
                pack_cell(0, 0, false, false),
                "and the anchor pixel is cell (0, 0)"
            );
        });

        // The canvas entry point, on the same entity.
        cx.simulate_keystrokes("escape");
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.test_paint_mode_rel(), None, "escape exits the mode");
            assert!(
                !panel.canvas_double_click([4., 4.], cx),
                "empty space is not a paint target"
            );
            assert!(
                panel.canvas_double_click([50., 18.], cx),
                "a double-click over the Tilemap entity enters paint mode"
            );
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/deco.map"),
                "the cached session enters synchronously"
            );
        });
    }

    /// Set up a clean world with background layer 0 linked, painted, and
    /// the panel sitting in paint mode over `maps/test.bg0.map`. The world
    /// store is written out first, so every dirty bit the callers assert on
    /// afterwards can only have come from the PAINT session.
    async fn painting_panel<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (gpui::Entity<WorldPanel>, &'a mut gpui::VisualTestContext) {
        let (panel, cx) = ready_panel_in_window(cx, root).await;
        write_test_tileset(root, "tiles/bg.til");
        focus_the_panel(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx);
            panel.save_impl(cx);
            assert!(panel.test_enter_paint_bg(0, cx), "the slot resolves");
        });
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(
                panel.test_paint_mode_rel().as_deref(),
                Some("maps/test.bg0.map")
            );
            assert!(
                panel.dirty_world_name().is_none(),
                "the fixture must start clean"
            );
        });
        (panel, cx)
    }

    /// One brush stroke on a CLEAN world has to reach every consumer of
    /// "this document has unsaved edits": the tab's dirty dot and close
    /// prompt (`dirty_world_name`), the Save action (`save_impl` writes the
    /// `.map` too), and the emulate gate (`save_if_open_and_dirty`, which
    /// `emd pack-ggo` depends on because it reads the cart's assets off
    /// DISK).
    #[gpui::test]
    async fn test_paint_dirt_reaches_the_tab_the_save_and_the_emulate_gate(
        cx: &mut TestAppContext,
    ) {
        use ggo_worldlib::sprites::map_doc::pack_cell;

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = painting_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert!(
                panel
                    .test_paint_session("maps/test.bg0.map")
                    .is_some_and(PaintSession::dirty),
                "the brush dirtied the map document"
            );
            assert!(
                !panel.test_is_dirty(),
                "and only the map document -- the world store is untouched"
            );
            assert_eq!(
                panel.dirty_world_name().as_deref(),
                Some("worlds/test.toml"),
                "paint dirt alone must mark the tab"
            );
        });

        // Through the KEYBINDING, not `save_impl` directly: the paint
        // column has no save button of its own, so ctrl-S reaching the
        // panel's Save action is the only affordance a painting user has.
        cx.simulate_keystrokes("ctrl-s");
        panel.update(cx, |panel, _| {
            assert!(
                panel.dirty_world_name().is_none(),
                "the save cleared the paint dirt too"
            );
            assert!(
                panel
                    .test_paint_session("maps/test.bg0.map")
                    .is_some_and(|session| !session.dirty()),
                "the session itself is marked saved"
            );
        });
        assert_eq!(
            io::open_map(dir.path(), "maps/test.bg0.map").unwrap().cells[0],
            pack_cell(0, 0, false, false),
            "the painted cell reached disk"
        );

        // The emulate gate: a paint edit the user never saved must be
        // flushed before a cart is built from the world.
        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([26., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert_eq!(
                panel.dirty_world_name().as_deref(),
                Some("worlds/test.toml"),
                "the second stroke re-dirties the document"
            );
            assert!(
                panel.save_if_open_and_dirty("worlds/test.toml", cx),
                "the flush must report success"
            );
        });
        assert_eq!(
            io::open_map(dir.path(), "maps/test.bg0.map").unwrap().cells[1],
            pack_cell(0, 0, false, false),
            "and it actually wrote the second cell to disk"
        );
    }

    /// The toolbar renders in paint mode too, and one `dirty` local drives
    /// all three of its title dot, its title color and
    /// `.disabled(!dirty)` on the Save button. That predicate has to be the
    /// DOCUMENT's: after a brush stroke on a saved world the store is
    /// clean, so a store-only predicate greys the only save affordance a
    /// painting user has out from under them. Asserted through a real
    /// CLICK, because `.disabled` filters the click handler out while
    /// ctrl-S reaches the action either way -- a keystroke test cannot see
    /// this regression.
    #[gpui::test]
    async fn test_the_toolbar_save_button_follows_paint_dirt(cx: &mut TestAppContext) {
        use ggo_worldlib::sprites::map_doc::pack_cell;

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = painting_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert!(
                !panel.test_is_dirty(),
                "the world store stays clean -- only the map moved"
            );
        });
        cx.run_until_parked();

        // The selector is on the button's WRAPPER, so it resolves whether
        // the button is live or greyed out: the click below is what tells
        // the two apart.
        let bounds = cx
            .debug_bounds("ggo-world-save")
            .expect("the toolbar renders in paint mode");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            io::open_map(dir.path(), "maps/test.bg0.map").unwrap().cells[0],
            pack_cell(0, 0, false, false),
            "a disabled Save button would have swallowed the click and left \
             the painted cell unwritten"
        );
        panel.update(cx, |panel, _| {
            assert!(
                panel.dirty_world_name().is_none(),
                "and the same predicate that enabled the button now clears \
                 the dot"
            );
        });
    }

    /// "Don't Save" reloads from disk, and discard has to mean discard:
    /// the paint sessions hanging off `OpenWorld` go with it, along with
    /// their undo history and unwritten cells.
    #[gpui::test]
    async fn test_reload_discards_paint_sessions(cx: &mut TestAppContext) {
        use ggo_worldlib::sprites::map_doc::CELL_BLANK;

        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = painting_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            assert!(panel.dirty_world_name().is_some(), "edited");
            panel.reload_from_disk("worlds/test.toml", cx);
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _| {
            assert!(
                open_of(panel).sessions.is_empty(),
                "the reloaded world starts with no sessions"
            );
            assert!(
                panel.dirty_world_name().is_none(),
                "the discarded document is clean again"
            );
        });
        assert_eq!(
            io::open_map(dir.path(), "maps/test.bg0.map").unwrap().cells[0],
            CELL_BLANK,
            "and the discarded stroke never reached disk"
        );
    }

    /// A world write that lands plus a map write that fails is a FAILED
    /// save: `save_for_close` keys off `save_error`, so anything else would
    /// let the close flow drop the painted cells on the floor.
    #[gpui::test]
    async fn test_failed_map_write_keeps_the_document_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = painting_panel(cx, dir.path()).await;

        // A regular file where the session's asset root should be: the
        // map's parent-dir creation then fails deterministically (the same
        // blocker trick `world_canvas_item`'s save test uses on the world).
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();

        panel.update(cx, |panel, cx| {
            panel.canvas_primary_down_with([10., 10.], false, cx);
            panel.canvas_primary_up(cx);
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [70.0, 80.0],
                    gesture: None,
                },
                cx,
            );
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.sessions
                .get_mut("maps/test.bg0.map")
                .expect("the session is cached under its rel")
                .root = blocker;

            assert!(
                !panel.save_for_close(cx),
                "a failed map write must not report the document as saved"
            );
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert!(
                open.save_error.is_some(),
                "the failure must be visible on the toolbar"
            );
            assert!(
                open.sessions["maps/test.bg0.map"].save_error.is_some(),
                "and on the paint column that owns the write"
            );
            assert_eq!(
                panel.dirty_world_name().as_deref(),
                Some("worlds/test.toml"),
                "the document stays dirty until the map actually lands"
            );
        });
        assert_eq!(
            read_world(dir.path(), "worlds/test.toml").unwrap().entities[0].components["Transform"]
                ["pos"],
            serde_json::json!([70, 80]),
            "the world half of the save still landed"
        );
    }

    // ---------------------------------------------------- live mode

    thread_local! {
        static BOOTED: RefCell<Vec<(String, Arc<ggo_common::LinkEndpoint>)>> =
            const { RefCell::new(Vec::new()) };
    }

    fn fake_booter(
        _workspace: &mut Workspace,
        rel: &str,
        endpoint: Arc<ggo_common::LinkEndpoint>,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        BOOTED.with(|booted| booted.borrow_mut().push((rel.to_string(), endpoint)));
        true
    }

    /// One cart -> host datagram, hand-rolled against the wire layout table
    /// in `emerald-editor-runtime`'s `wire.rs`. Built by hand rather than
    /// with `wire::encode_cart` so this crate does not take a dependency on
    /// the runtime just to test the host half -- and so the bytes the
    /// mailbox is fed are the documented ones, not the encoder's opinion of
    /// them.
    fn cart_says(endpoint: &ggo_common::LinkEndpoint, datagram: Vec<u8>) {
        endpoint.push_inbound(datagram);
        endpoint.tick();
    }

    /// `0x81 HelloAck`: `version u8, entity_cap u16, world_cap u32,
    /// layer_cap u32, count u8`, then `count` x (`len u8`, bytes).
    fn hello_ack(version: u8, systems: &[&str]) -> Vec<u8> {
        let mut out = vec![0x81, version];
        out.extend_from_slice(&256u16.to_le_bytes());
        out.extend_from_slice(&32768u32.to_le_bytes());
        out.extend_from_slice(&8192u32.to_le_bytes());
        out.push(systems.len() as u8);
        for name in systems {
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        out
    }

    /// Every APP payload the panel has put on the link since the last call.
    fn host_sent(endpoint: &ggo_common::LinkEndpoint) -> Vec<Vec<u8>> {
        let mut reader = ggo_comm::MessageReader::default();
        endpoint
            .take_outbound()
            .iter()
            .flat_map(|wire| reader.feed(wire))
            .filter_map(|item| match item {
                ggo_comm::LinkItem::Message(message) => Some(message.payload().to_vec()),
                ggo_comm::LinkItem::Text(_) => None,
            })
            .collect()
    }

    /// A panel in a real window with `worlds/test.toml` open in Live mode,
    /// plus the endpoint the (fake) booter was handed.
    async fn live_panel<'a>(
        cx: &'a mut TestAppContext,
        dir: &tempfile::TempDir,
    ) -> (
        Entity<WorldPanel>,
        Arc<ggo_common::LinkEndpoint>,
        &'a mut gpui::VisualTestContext,
    ) {
        live_panel_rel(cx, dir, "worlds/test.toml").await
    }

    /// [`live_panel`] for a world other than the fixture's own.
    async fn live_panel_rel<'a>(
        cx: &'a mut TestAppContext,
        dir: &tempfile::TempDir,
        rel: &str,
    ) -> (
        Entity<WorldPanel>,
        Arc<ggo_common::LinkEndpoint>,
        &'a mut gpui::VisualTestContext,
    ) {
        let rel = rel.to_string();
        let project = routed_project(cx, dir.path(), true).await;
        cx.update(|cx| ggo_common::register_viewer_booter(cx, fake_booter));
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        // Through the dock, so the panel is the one the dock RENDERS: the
        // toolbar, status line and systems rail only paint there.
        let dock = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldDock>(cx)
                .expect("init() adds the dock")
        });
        dock.update(cx, |dock, _| {
            dock.test_root_override(dir.path().to_path_buf())
        });
        workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::open_in_panel(workspace, window, cx, |dock: &mut WorldDock, window, cx| {
                dock.open_world(&rel, window, cx);
            })
        });
        cx.run_until_parked();
        let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
        let endpoint = BOOTED
            .with(|booted| booted.borrow().last().map(|(_, e)| e.clone()))
            .expect("Live mode asked the booter for a viewer cart");
        (panel, endpoint, cx)
    }

    /// The bytes of the LAST layer-`layer` blob in `sent`, reassembled
    /// from its chunks. Placed by `off` rather than appended, so a
    /// go-back-N retransmission overwrites its own range instead of
    /// lengthening the payload.
    fn layer_blob(sent: &[Vec<u8>], layer: u8) -> Option<Vec<u8>> {
        use emerald_editor_runtime::wire::{self, BlobKind, HostMsg};

        let begin = sent.iter().rposition(|message| {
            matches!(
                wire::decode_host(message),
                Some(HostMsg::BlobBegin {
                    kind: BlobKind::Layer,
                    layer: sent_layer,
                    ..
                }) if sent_layer == layer
            )
        })?;
        let HostMsg::BlobBegin { len, .. } = wire::decode_host(&sent[begin])? else {
            return None;
        };
        let mut out = vec![0u8; usize::try_from(len).ok()?];
        for message in &sent[begin + 1..] {
            match wire::decode_host(message) {
                Some(HostMsg::BlobChunk { off, data, .. }) => {
                    let off = usize::try_from(off).ok()?;
                    out.get_mut(off..off.checked_add(data.len())?)?
                        .copy_from_slice(data);
                }
                Some(HostMsg::BlobEnd { .. }) => break,
                _ => {}
            }
        }
        Some(out)
    }

    /// Which background slot each layer `BlobBegin` in `sent` opens, in
    /// order. Decoded rather than read off a byte offset: `BlobBegin` has
    /// grown a field before `layer` once already, and an offset that goes
    /// stale reads a neighbouring field as a slot number instead of
    /// failing.
    fn sent_layers(sent: &[Vec<u8>]) -> Vec<u8> {
        use emerald_editor_runtime::wire::{self, BlobKind, HostMsg};

        sent.iter()
            .filter_map(|message| match wire::decode_host(message) {
                Some(HostMsg::BlobBegin {
                    kind: BlobKind::Layer,
                    layer,
                    ..
                }) => Some(layer),
                _ => None,
            })
            .collect()
    }

    /// `0x82 Ack`: `seq u16`.
    fn blob_ack(seq: u16) -> Vec<u8> {
        let mut out = vec![0x82];
        out.extend_from_slice(&seq.to_le_bytes());
        out
    }

    /// Most rows one `0x84 Entities` datagram carries -- the wire's own cap
    /// (`1 + 1 + 12 * 20 = 242` bytes), which its decoder rejects a datagram
    /// for exceeding.
    const ROWS_PER_DATAGRAM: usize = 12;

    /// The cart's whole entity table: `0x84 Entities` datagrams (`count u8`,
    /// then 20-byte rows of `index u32, x i32, y i32, w u16, h u16, ox i16,
    /// oy i16`) followed by the `0x85 EntityCount` that closes it. `x`/`y`
    /// are the entity's transform and `ox`/`oy` the sprite's draw offset,
    /// zero here: every fixture rect is an unoffset 16x16, which is what the
    /// fixture's entities measure.
    fn cart_rows(endpoint: &ggo_common::LinkEndpoint, rows: &[(u32, f64, f64)]) {
        for batch in rows.chunks(ROWS_PER_DATAGRAM) {
            let mut out = vec![0x84, batch.len() as u8];
            for (index, x, y) in batch {
                out.extend_from_slice(&index.to_le_bytes());
                out.extend_from_slice(&live::to_raw(*x).to_le_bytes());
                out.extend_from_slice(&live::to_raw(*y).to_le_bytes());
                out.extend_from_slice(&16u16.to_le_bytes());
                out.extend_from_slice(&16u16.to_le_bytes());
                out.extend_from_slice(&0i16.to_le_bytes());
                out.extend_from_slice(&0i16.to_le_bytes());
            }
            cart_says(endpoint, out);
        }
        let mut count = vec![0x85];
        count.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        cart_says(endpoint, count);
    }

    fn live_of(panel: &WorldPanel) -> &LiveView {
        open_of(panel).live.as_ref().expect("a live session")
    }

    fn live_mut_of(panel: &mut WorldPanel) -> &mut LiveView {
        let ViewerState::Ready(open) = &mut panel.state else {
            panic!("expected Ready");
        };
        open.live.as_mut().expect("a live session")
    }

    /// What the Live canvas would outline right now.
    fn overlay_of(panel: &WorldPanel) -> Vec<(Selection, [f64; 4], bool)> {
        let open = open_of(panel);
        live::overlay_rows(
            live_of(panel),
            OpenWorld::doc_counts(&open.store.state()),
            &open.selected,
        )
    }

    /// `0x88 FrameSeq`: `seq u32` -- the per-frame heartbeat a running
    /// cart publishes, and the only proof the host has that a datagram
    /// arrived after a blob was acked.
    fn cart_frame(endpoint: &ggo_common::LinkEndpoint, seq: u32) {
        let mut out = vec![0x88];
        out.extend_from_slice(&seq.to_le_bytes());
        cart_says(endpoint, out);
    }

    /// `0x89 Camera`: `x i32, y i32` Q16.16 -- the camera the cart's render
    /// pass drew its last frame with.
    fn cart_camera(endpoint: &ggo_common::LinkEndpoint, x: f64, y: f64) {
        let mut out = vec![0x89];
        out.extend_from_slice(&live::to_raw(x).to_le_bytes());
        out.extend_from_slice(&live::to_raw(y).to_le_bytes());
        cart_says(endpoint, out);
    }

    /// `0x8A Selection`: `part u8, total u8, count u8`, then `count` x
    /// `index u32` -- part 0 of a one-datagram sequence; no fixture here
    /// is wide enough to make the cart split a selection.
    fn cart_selection(endpoint: &ggo_common::LinkEndpoint, indices: &[u32]) {
        let mut out = vec![0x8A, 0, 1, indices.len() as u8];
        for index in indices {
            out.extend_from_slice(&index.to_le_bytes());
        }
        cart_says(endpoint, out);
    }

    /// `0x8B Marquee`: `active u8, x0 i32, y0 i32, x1 i32, y1 i32` -- whole
    /// world pixels, not the fixed point the transforms use.
    fn cart_marquee(endpoint: &ggo_common::LinkEndpoint, active: bool, band: [i32; 4]) {
        let mut out = vec![0x8B, u8::from(active)];
        for value in band {
            out.extend_from_slice(&value.to_le_bytes());
        }
        cart_says(endpoint, out);
    }

    /// `0x8C Gesture`: `kind u8` (0 begin, 1 end), `id u32`.
    fn cart_gesture(endpoint: &ggo_common::LinkEndpoint, kind: u8, id: u32) {
        let mut out = vec![0x8C, kind];
        out.extend_from_slice(&id.to_le_bytes());
        cart_says(endpoint, out);
    }

    /// `0x8D EntityRemoved`: `index u32`.
    fn cart_removed(endpoint: &ggo_common::LinkEndpoint, index: u32) {
        let mut out = vec![0x8D];
        out.extend_from_slice(&index.to_le_bytes());
        cart_says(endpoint, out);
    }

    /// `0x8E ComponentChanged`: `index u32`, then the component's whole
    /// new bag.
    fn cart_component_changed(endpoint: &ggo_common::LinkEndpoint, index: u32, bag: &[u8]) {
        let mut out = vec![0x8E];
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(bag);
        cart_says(endpoint, out);
    }

    /// `0x8F EntityAdded`: `index u32`.
    fn cart_added(endpoint: &ggo_common::LinkEndpoint, index: u32) {
        let mut out = vec![0x8F];
        out.extend_from_slice(&index.to_le_bytes());
        cart_says(endpoint, out);
    }

    /// One whole cart -> host blob: `0x90 CartBlobBegin` (`kind u8, len
    /// u32, index u32`), one `0x91 CartBlobChunk` per [`CHUNK_BYTES`]
    /// (`seq u16, off u32, data`), then `0x92 CartBlobEnd` carrying one
    /// past the last chunk's sequence number.
    fn cart_blob(endpoint: &ggo_common::LinkEndpoint, kind: u8, index: u32, body: &[u8]) {
        use emerald_editor_runtime::wire::CHUNK_BYTES;

        let mut begin = vec![0x90, kind];
        begin.extend_from_slice(&(body.len() as u32).to_le_bytes());
        begin.extend_from_slice(&index.to_le_bytes());
        cart_says(endpoint, begin);
        let mut seq: u16 = 0;
        for (chunk, data) in body.chunks(CHUNK_BYTES).enumerate() {
            let mut out = vec![0x91];
            out.extend_from_slice(&seq.to_le_bytes());
            out.extend_from_slice(&((chunk * CHUNK_BYTES) as u32).to_le_bytes());
            out.extend_from_slice(data);
            cart_says(endpoint, out);
            seq += 1;
        }
        let mut end = vec![0x92];
        end.extend_from_slice(&seq.to_le_bytes());
        cart_says(endpoint, end);
    }

    /// A `Snapshot` blob body: `entity_count u32`, then per tracked slot
    /// `index u32, comp_count u16, bags`.
    fn snapshot_body(entities: &[(u32, Vec<Vec<u8>>)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(entities.len() as u32).to_le_bytes());
        for (index, bags) in entities {
            body.extend_from_slice(&index.to_le_bytes());
            body.extend_from_slice(&(bags.len() as u16).to_le_bytes());
            for bag in bags {
                body.extend_from_slice(bag);
            }
        }
        body
    }

    /// A `Layer` blob body: `w u16, h u16, cells [u16; w * h]`. `0 x 0` is
    /// the cart's answer for a slot with no map.
    fn layer_body(w: u16, h: u16, cells: &[u16]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&h.to_le_bytes());
        for cell in cells {
            body.extend_from_slice(&cell.to_le_bytes());
        }
        body
    }

    /// Answer the readbacks a save in flight asked for, the way the cart
    /// does: one `Snapshot` blob carrying `entities`, and one `Layer` blob
    /// per `ReadLayer` the host sent -- `layers` where the slot has an
    /// entry, `0 x 0` ("no map") where it does not.
    fn answer_cart_save(
        panel: &Entity<WorldPanel>,
        endpoint: &ggo_common::LinkEndpoint,
        cx: &mut gpui::VisualTestContext,
        entities: &[(u32, Vec<Vec<u8>>)],
        layers: &[(u8, u16, u16, Vec<u16>)],
    ) {
        use emerald_editor_runtime::wire::{self, HostMsg};

        let mut asked_snapshot = false;
        let mut asked_layers: Vec<u8> = Vec::new();
        for message in host_sent(endpoint) {
            match wire::decode_host(&message) {
                Some(HostMsg::Snapshot) => asked_snapshot = true,
                Some(HostMsg::ReadLayer { layer }) => asked_layers.push(layer),
                _ => {}
            }
        }
        for layer in asked_layers {
            let body = match layers.iter().find(|(slot, ..)| *slot == layer) {
                Some((_, w, h, cells)) => layer_body(*w, *h, cells),
                None => layer_body(0, 0, &[]),
            };
            cart_blob(endpoint, 1, u32::from(layer), &body);
        }
        if asked_snapshot {
            cart_blob(endpoint, 0, 0, &snapshot_body(entities));
        }
        cart_frames(panel, endpoint, cx, 1);
    }

    /// `0x83 Schema`: `off u16`, then the packed table -- `count u16`,
    /// then per component `name str, field_count u8`, then per field
    /// `name str, kind u8`. The bags only mean anything against this
    /// table, since a bag names its fields by hash alone.
    fn cart_schemas(endpoint: &ggo_common::LinkEndpoint) {
        const INT: u8 = 0;
        const FIXED: u8 = 1;
        const BOOL: u8 = 2;
        const STR: u8 = 3;
        const VEC2: u8 = 4;
        let components: &[(&str, &[(&str, u8)])] = &[
            ("Transform", &[("pos", VEC2), ("z", INT)]),
            (
                "Text",
                &[
                    ("content", STR),
                    ("max_width", FIXED),
                    ("max_height", FIXED),
                ],
            ),
            ("Camera", &[("is_active", BOOL)]),
        ];
        let mut table = Vec::new();
        table.extend_from_slice(&(components.len() as u16).to_le_bytes());
        for (name, fields) in components {
            table.push(name.len() as u8);
            table.extend_from_slice(name.as_bytes());
            table.push(fields.len() as u8);
            for (field, kind) in *fields {
                table.push(field.len() as u8);
                table.extend_from_slice(field.as_bytes());
                table.push(*kind);
            }
        }
        let mut out = vec![0x83];
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&table);
        cart_says(endpoint, out);
    }

    /// A `Transform` bag, the shape the cart publishes and takes back.
    fn transform_bag(pos: [f64; 2], z: i32) -> Vec<u8> {
        let mut writer = emerald_world::FieldWriter::new();
        writer.vec2(
            "pos",
            emerald_core::Vec2::new(
                emerald_core::Fixed::from_raw(live::to_raw(pos[0])),
                emerald_core::Fixed::from_raw(live::to_raw(pos[1])),
            ),
        );
        writer.int("z", z);
        writer.finish("Transform")
    }

    /// Every `SetComponent` in `sent`, as `(entity index, bag)`.
    fn set_components(sent: &[Vec<u8>]) -> Vec<(u32, Vec<u8>)> {
        use emerald_editor_runtime::wire::{self, HostMsg};

        sent.iter()
            .filter_map(|message| match wire::decode_host(message) {
                Some(HostMsg::SetComponent { index, bag }) => Some((index, bag.to_vec())),
                _ => None,
            })
            .collect()
    }

    /// One integer field of a bag, read back the way the cart would.
    fn bag_int(bag: &[u8], field: &str) -> Option<i32> {
        let (_, reader, _) = emerald_world::FieldReader::from_bag(bag)?;
        match reader.get(emerald_world::field_hash(field))? {
            emerald_world::FieldValue::Int(value) => Some(value),
            _ => None,
        }
    }

    /// One string field of a bag, read back the way the cart would.
    fn bag_str(bag: &[u8], field: &str) -> Option<String> {
        let (_, reader, _) = emerald_world::FieldReader::from_bag(bag)?;
        match reader.get(emerald_world::field_hash(field))? {
            emerald_world::FieldValue::Str(value) => Some(value.to_string()),
            _ => None,
        }
    }

    /// Drive `count` cart frames past wherever the session is, so the
    /// deadlines measured on the cart's own frame counter can expire.
    fn cart_frames(
        panel: &Entity<WorldPanel>,
        endpoint: &ggo_common::LinkEndpoint,
        cx: &mut gpui::VisualTestContext,
        count: u32,
    ) {
        let mut frame = panel.read_with(cx, |panel, _| live_of(panel).mailbox.frame_seq());
        for _ in 0..count {
            frame += 1;
            cart_frame(endpoint, frame);
            cx.run_until_parked();
        }
    }

    const GESTURE_BEGIN: u8 = 0;
    const GESTURE_END: u8 = 1;

    /// The fixture as the cart first publishes it: three direct entities
    /// then the one entity `worlds/sub` contributes, each where the
    /// document authored it. The mirror needs this baseline before it can
    /// tell a cart-side move from a first sighting.
    fn cart_fixture_rows(endpoint: &ggo_common::LinkEndpoint) {
        cart_rows(
            endpoint,
            &[
                (0, 4.0, 4.0),
                (1, 40.0, 8.0),
                (2, 0.0, 0.0),
                (3, 32.0, 16.0),
            ],
        );
    }

    /// Answer whatever blob window is on the wire the way the cart does,
    /// and publish NO frame -- so a test can tell "the bytes landed" from
    /// "the cart has drawn something since". Returns what the host had
    /// queued and whether any of it was a blob.
    fn ack_blobs(endpoint: &ggo_common::LinkEndpoint) -> (Vec<Vec<u8>>, bool) {
        let drained = host_sent(endpoint);
        let mut answered = false;
        for message in &drained {
            // `0x09 BlobChunk` and `0x0A BlobEnd` both lead with `seq`.
            if matches!(message.first(), Some(0x09 | 0x0A))
                && let Some(seq) = message.get(1..3)
            {
                endpoint.push_inbound(blob_ack(u16::from_le_bytes([seq[0], seq[1]])));
                answered = true;
            }
        }
        (drained, answered)
    }

    /// Run the session until it has nothing left to push: every round
    /// answers the host's in-flight blob window the way the cart does
    /// (`Ack` per `BlobChunk`, one for the `BlobEnd`; acks are cumulative,
    /// so go-back-N unblocks the next window), publishes the next frame
    /// (a running cart frames whether or not the host is talking to it),
    /// and ticks the endpoint so the poll loop takes another step. Returns
    /// everything the host put on the wire while settling, in order.
    fn settle_live(
        panel: &Entity<WorldPanel>,
        endpoint: &ggo_common::LinkEndpoint,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<Vec<u8>> {
        let mut seen = Vec::new();
        let mut frame = panel.read_with(cx, |panel, _| live_of(panel).mailbox.frame_seq());
        for _ in 0..200 {
            let pending = panel.read_with(cx, |panel, _| {
                let live = live_of(panel);
                live.world_dirty
                    || live.layers_dirty.any()
                    || !live.layer_queue.is_empty()
                    || live.pending_camera.is_some()
                    || !live.pending_edits.is_empty()
                    || live.mailbox.busy()
                    || !live.loaded()
            });
            let (drained, answered) = ack_blobs(endpoint);
            seen.extend(drained);
            if !pending && !answered {
                return seen;
            }
            frame += 1;
            cart_frame(endpoint, frame);
            cx.run_until_parked();
        }
        panic!("the live session never settled");
    }

    /// A connected, fully synced Live panel: the cart has greeted, the
    /// world blob and the four layer slots have been acked, and the canvas
    /// has been given bounds so the Live transform is defined.
    async fn connected_live_panel(
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorldPanel>,
        Arc<ggo_common::LinkEndpoint>,
        tempfile::TempDir,
        &mut gpui::VisualTestContext,
    ) {
        connected_live_panel_with_tools(cx, &[]).await
    }

    /// [`connected_live_panel`], with the cart greeting as a build whose
    /// tool names are `tools` -- what the rail's tool radio lists, tool 0
    /// first.
    async fn connected_live_panel_with_tools<'a>(
        cx: &'a mut TestAppContext,
        tools: &[&str],
    ) -> (
        Entity<WorldPanel>,
        Arc<ggo_common::LinkEndpoint>,
        tempfile::TempDir,
        &'a mut gpui::VisualTestContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, tools),
        );
        // A real cart publishes its schema table right behind the
        // greeting, and without it no bag on either side of the link means
        // anything: the fields are named by hash alone.
        cart_schemas(&endpoint);
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            let mut view = open_of(panel).view.borrow_mut();
            view.zoom = 1.0;
            view.pan = Some([0.0, 0.0]);
            view.last_bounds = Some(gpui::bounds(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(800.), px(600.)),
            ));
        });
        settle_live(&panel, &endpoint, cx);
        (panel, endpoint, dir, cx)
    }

    /// [`connected_live_panel`] on a world with no `[[instance]]` -- the
    /// only shape whose cart flattening a spawn or a despawn keeps in step
    /// with the document's (see `LiveView::plan_replay`).
    async fn connected_live_panel_flat(
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorldPanel>,
        Arc<ggo_common::LinkEndpoint>,
        tempfile::TempDir,
        &mut gpui::VisualTestContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_world(
            dir.path(),
            "worlds/flat.toml",
            &WorldFile {
                entities: vec![entity(
                    json!({ "Transform": { "pos": [4.0, 4.0], "z": 0 } }),
                )],
                instances: vec![],
                backgrounds: vec![],
            },
        )
        .unwrap();
        let (panel, endpoint, cx) = live_panel_rel(cx, &dir, "worlds/flat.toml").await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cart_schemas(&endpoint);
        cx.run_until_parked();
        settle_live(&panel, &endpoint, cx);
        (panel, endpoint, dir, cx)
    }

    /// [`connected_live_panel`] with slot 0 linked to a real `.til`/`.map`
    /// pair, so paint mode has something to open.
    async fn connected_live_panel_with_background(
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorldPanel>,
        Arc<ggo_common::LinkEndpoint>,
        tempfile::TempDir,
        &mut gpui::VisualTestContext,
    ) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx)
        });
        cx.run_until_parked();
        settle_live(&panel, &endpoint, cx);
        (panel, endpoint, dir, cx)
    }

    /// World px -> canvas-local px for the Live view: the centered device
    /// frame at the fitted scale, offset by the cart's camera.
    fn live_screen_of(panel: &WorldPanel, world: [f64; 2]) -> [f64; 2] {
        let (view, _, _) = panel.live_camera().expect("a laid-out live canvas");
        drag_ops::world_to_screen(world[0], world[1], &view)
    }

    #[gpui::test]
    async fn live_mode_boots_the_viewer_and_says_hello_once_it_runs(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        BOOTED.with(|booted| {
            assert_eq!(
                booted.borrow().last().map(|(rel, _)| rel.clone()),
                Some("worlds/test.toml".to_string()),
                "the booter is asked for the world the panel opened"
            );
        });
        assert!(
            host_sent(&endpoint).is_empty(),
            "nothing goes on the wire while the cart is still building"
        );

        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();

        let sent = host_sent(&endpoint);
        assert_eq!(sent.first().map(|m| m[0]), Some(0x01), "Hello goes first");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Live);
            assert_eq!(
                open_of(panel).live.as_ref().map(|live| live.status.clone()),
                Some(LiveStatus::Connecting)
            );
        });
    }

    #[gpui::test]
    async fn live_mode_connects_on_hello_ack_and_sends_the_world(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        host_sent(&endpoint);

        cart_says(
            &endpoint,
            hello_ack(
                emerald_editor_runtime::wire::LINK_PROTO_VERSION,
                &["animate"],
            ),
        );
        cx.run_until_parked();

        let sent = host_sent(&endpoint);
        assert_eq!(
            sent.first().map(|m| (m[0], m[1])),
            // `0x08 BlobBegin`, then `kind u8`: 0 is the world document
            // (1 would be a background layer).
            Some((0x08, 0x00)),
            "the world document goes out as soon as the session opens"
        );
        panel.read_with(cx, |panel, _| {
            let open = open_of(panel);
            let live = open.live.as_ref().expect("a live session");
            assert_eq!(live.status, LiveStatus::Connected);
            assert_eq!(
                live.mailbox.tool_names(),
                ["animate"],
                "the rail lists what the cart greeted with"
            );
            assert_eq!(
                live.index_map.len(),
                4,
                "three direct entities plus the instance's one"
            );
            assert_eq!(open.live_error, None);
        });
    }

    #[gpui::test]
    async fn a_stopped_viewer_falls_back_to_design_with_the_reason(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Stopped(
            "build failed: no emd".into(),
        ));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            let open = open_of(panel);
            assert!(
                open.live_error.as_deref().unwrap_or("").contains("no emd"),
                "the toolbar names the reason: {:?}",
                open.live_error
            );
            assert!(
                matches!(
                    open.live.as_ref().map(|live| &live.status),
                    Some(LiveStatus::Failed(_))
                ),
                "the failed session is kept for its message"
            );
        });
    }

    #[gpui::test]
    async fn no_booter_means_design_mode(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace_panel(&workspace, dir.path(), cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("worlds/test.toml", window, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(open_of(panel).live.is_none());
            assert!(open_of(panel).live_error.is_some());
        });
    }

    #[gpui::test]
    async fn a_version_mismatch_falls_back_and_names_the_rebuild(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(&endpoint, hello_ack(0, &[]));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("rebuild"),
                "the fallback tells the user to rebuild the viewer cart: {:?}",
                open_of(panel).live_error
            );
        });
    }

    /// A fallback for anything but "the cart stopped" would otherwise
    /// leave a cart RUNNING with nothing polling it, and the panel
    /// holding its link until the next boot replaced it. So a
    /// non-`Stopped` failure ends the run there and then.
    #[gpui::test]
    async fn a_non_stopped_fallback_ends_the_viewer_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(&endpoint, hello_ack(0, &[]));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(
                panel.live_endpoint.is_none(),
                "the panel let go of the cart it booted"
            );
            assert!(
                live_of(panel).frame.is_none(),
                "and dropped the frame, whose tile the emu panel may have retired"
            );
        });
        assert!(endpoint.stop_requested(), "and asked the run to end");
    }

    /// The cart reporting `Stopped` IS the run ending: there is nothing
    /// left to ask, and the endpoint stays put until the next
    /// `enter_live` drops it for the cart it boots.
    #[gpui::test]
    async fn a_stopped_cart_is_not_asked_to_stop_again(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Stopped("gone".into()));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
        });
        assert!(
            !endpoint.stop_requested(),
            "a run that is already over is not asked to end"
        );
    }

    /// The poll loop stops with the fallback: a failed session must not
    /// keep waking the panel every 250 ms for the rest of the run.
    #[gpui::test]
    async fn a_fallback_stops_the_poll_loop(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Stopped("gone".into()));
        cx.run_until_parked();
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(
                matches!(
                    open_of(panel).live.as_ref().map(|live| &live.status),
                    Some(LiveStatus::Failed(_))
                ),
                "a later Running does not resurrect a failed session"
            );
        });
        assert!(
            host_sent(&endpoint).is_empty(),
            "and nothing more goes on the wire"
        );
    }

    /// A booter whose cart is dead on arrival -- the build failed before
    /// the panel ever polled it.
    fn stillborn_booter(
        _workspace: &mut Workspace,
        rel: &str,
        endpoint: Arc<ggo_common::LinkEndpoint>,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        endpoint.set_state(ggo_common::ViewerState::Stopped("build failed".into()));
        BOOTED.with(|booted| booted.borrow_mut().push((rel.to_string(), endpoint)));
        true
    }

    /// A session that fails on its very FIRST tick must not leave its poll
    /// task behind: `Failed` is terminal, `live_step` has nothing left to
    /// do for it, and a loop that kept waking would notify the panel every
    /// 250 ms for the rest of the document's life.
    #[gpui::test]
    async fn a_session_that_fails_immediately_leaves_no_poll_task(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        cx.update(|cx| ggo_common::register_viewer_booter(cx, stillborn_booter));
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace_panel(&workspace, dir.path(), cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("worlds/test.toml", window, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            let live = open_of(panel).live.as_ref().expect("the failed session");
            assert!(matches!(live.status, LiveStatus::Failed(_)));
            assert!(
                live.poll.is_none(),
                "the poll task must not outlive the session it polls"
            );
        });
        // And a tick that does arrive anyway is refused: `Failed` never
        // says "keep polling".
        let endpoint = BOOTED
            .with(|booted| booted.borrow().last().map(|(_, e)| e.clone()))
            .expect("the booter ran");
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        let kept_going = panel.update(cx, |panel, cx| panel.live_tick(cx));
        assert!(!kept_going, "a failed session never asks for another tick");
    }

    /// A fallback keeps the failed session only for its message, so asking
    /// for Live again has to start a NEW one -- otherwise the first
    /// failure disables Live for the life of the open world.
    #[gpui::test]
    async fn live_can_be_re_entered_after_a_fallback(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Stopped("gone".into()));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
        });
        assert_eq!(BOOTED.with(|booted| booted.borrow().len()), 1);

        panel.update_in(cx, |panel, window, cx| {
            panel.canvas_mode = CanvasMode::Live;
            panel.enter_live(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            BOOTED.with(|booted| booted.borrow().len()),
            2,
            "the stopped cart is not reused; a fresh one is booted"
        );
        panel.read_with(cx, |panel, _| {
            let live = open_of(panel).live.as_ref().expect("a new session");
            assert!(
                matches!(live.status, LiveStatus::Building),
                "the new session starts over rather than inheriting the failure"
            );
            assert_eq!(open_of(panel).live_error, None);
        });
    }

    /// The fallback reason is only useful if the user can see it.
    #[gpui::test]
    async fn the_live_error_shows_on_the_status_row(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("ggo-world-live-error").is_none(),
            "no row while Live has nothing to complain about"
        );

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.live_error = Some("viewer cart never answered".to_string());
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("ggo-world-live-error").is_some(),
            "the toolbar shows why the canvas fell back to the design view"
        );
    }

    /// A wake that changed nothing must not repaint the panel: the loop
    /// wakes at least every 250 ms for as long as the world is open, and
    /// an unconditional `notify` would re-render the whole panel that
    /// often while the user is doing nothing.
    #[gpui::test]
    async fn a_tick_that_changes_nothing_does_not_notify(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cx.run_until_parked();

        let notifications = Rc::new(std::cell::Cell::new(0usize));
        let subscription = cx.update(|_, cx| {
            cx.observe(&panel, {
                let notifications = notifications.clone();
                move |_, _| notifications.set(notifications.get() + 1)
            })
        });
        for _ in 0..3 {
            endpoint.tick();
            cx.run_until_parked();
        }
        assert_eq!(
            notifications.get(),
            0,
            "an idle cart's ticks must not repaint the panel"
        );

        // ... and a tick that DOES bring something new still does.
        cart_says(
            &endpoint,
            hello_ack(
                emerald_editor_runtime::wire::LINK_PROTO_VERSION,
                &["animate"],
            ),
        );
        cx.run_until_parked();
        assert!(
            notifications.get() > 0,
            "a re-greeting is a change and must repaint"
        );
        drop(subscription);
    }

    /// The flattened instance counts come off the loader thread, and a
    /// document whose instance LIST changes gets recounted there too --
    /// the walk reads every instanced world file.
    #[gpui::test]
    async fn instance_counts_are_loaded_and_recounted_off_thread(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let open = open_of(panel);
            assert_eq!(
                open.instance_counts,
                vec![1],
                "the load counted worlds/sub's one entity"
            );
            assert_eq!(open.counted_instances, vec!["worlds/sub".to_string()]);
            assert_eq!(open.live.as_ref().map(|live| live.index_map.len()), Some(4));
        });

        panel.update(cx, |panel, cx| {
            panel.apply_op(WorldOp::RemoveInstance { index: 0 }, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let open = open_of(panel);
            assert!(open.instance_counts.is_empty(), "the recount landed");
            assert!(open.counted_instances.is_empty());
            assert_eq!(
                open.live.as_ref().map(|live| live.index_map.len()),
                Some(3),
                "and the index map was rebuilt from it"
            );
        });
    }

    /// Every world drives its OWN viewer run: a panel that switches
    /// documents boots a fresh cart for the new world and ends the run it
    /// was greeting, because a cart only ever shows the world it was
    /// greeted with.
    #[gpui::test]
    async fn a_world_switch_boots_its_own_cart_and_ends_the_old_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("worlds/sub.toml", window, cx)
        });
        cx.run_until_parked();

        assert_eq!(
            BOOTED.with(|booted| booted.borrow().len()),
            2,
            "the second world booted a cart of its own"
        );
        assert!(
            endpoint.stop_requested(),
            "and the run the first world was greeting ended"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).listing.rel_path, "worlds/sub.toml");
            assert!(open_of(panel).live.is_some(), "still live after the switch");
        });
    }

    /// A viewer cart that never finishes building produces no frames, so
    /// nothing on the cart's clock can time it out -- the build deadline
    /// is the one that runs on the executor's.
    #[gpui::test]
    async fn a_build_that_never_finishes_falls_back(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, _endpoint, cx) = live_panel(cx, &dir).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Live, "still waiting");
        });
        cx.executor()
            .advance_clock(live::BUILD_DEADLINE + std::time::Duration::from_secs(1));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("build timed out"),
                "{:?}",
                open_of(panel).live_error
            );
        });
    }

    /// Leaving Live ends the viewer run rather than leaving a cart burning
    /// emulator frames for a panel that stopped looking (Phase 2 review).
    #[gpui::test]
    async fn leaving_live_stops_the_viewer_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        assert!(!endpoint.stop_requested());
        panel.update(cx, |panel, cx| panel.leave_live(cx));
        cx.run_until_parked();
        assert!(endpoint.stop_requested(), "the viewer run is asked to end");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(open_of(panel).live.is_none());
        });
    }

    // ------------------------------------------------ the cart's mirror

    /// The cart owns the selection in Live: what it publishes replaces the
    /// document's, mapped back through the flattened index map, and the
    /// overlay outlines it.
    #[gpui::test]
    async fn the_carts_selection_becomes_the_documents(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cart_selection(&endpoint, &[1]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).selected, vec![Selection::Entity(1)]);
            let outlined: Vec<Selection> = overlay_of(panel)
                .into_iter()
                .filter(|(_, _, selected)| *selected)
                .map(|(target, _, _)| target)
                .collect();
            assert_eq!(outlined, [Selection::Entity(1)], "and it is drawn selected");
        });

        // A member of the instance's subtree names the `[[instance]]`, and
        // the whole run collapses to one entry.
        cart_selection(&endpoint, &[3]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).selected, vec![Selection::Instance(0)]);
        });

        cart_selection(&endpoint, &[]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(open_of(panel).selected.is_empty(), "and it can clear it");
        });
    }

    /// The band the cart drags out is drawn by the host and nothing else:
    /// it is state, not an edit.
    #[gpui::test]
    async fn the_carts_marquee_is_mirrored_and_retired(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_marquee(&endpoint, true, [10, 20, 30, 50]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).marquee, Some([10.0, 20.0, 30.0, 50.0]));
            assert_eq!(
                panel.test_live_marquee(),
                Some([10.0, 20.0, 20.0, 30.0]),
                "and the overlay paints it as an origin and a size"
            );
        });

        cart_marquee(&endpoint, false, [0, 0, 0, 0]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).marquee, None);
            assert_eq!(panel.test_live_marquee(), None, "and stops painting it");
        });
    }

    /// A cart-side drag: the `Begin` opens the gesture, every frame of rows
    /// inside it amends ONE undo entry, and the `End` closes it. The frame
    /// that carries the `Begin` is already part of the group, so the undo
    /// has to reach all the way back to where the entity started.
    #[gpui::test]
    async fn a_cart_drag_folds_into_one_undo_entry(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();

        cart_gesture(&endpoint, GESTURE_BEGIN, 7);
        cart_rows(&endpoint, &[(0, 40.0, 50.0)]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [40.0, 50.0]);
            assert_eq!(live_of(panel).gesture, [7]);
        });

        cart_rows(&endpoint, &[(0, 60.0, 50.0)]);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_END, 7);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [60.0, 50.0]);
            assert!(live_of(panel).gesture.is_empty(), "the End closed it");
        });

        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        panel.update(cx, |panel, _| {
            assert_eq!(
                entity_pos_of(panel, 0),
                [4.0, 4.0],
                "one undo returns the whole drag"
            );
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            assert!(!open.store.undo(), "and there was only the one entry");
        });
    }

    /// An `End` for a gesture that was never opened here closes nothing --
    /// the mailbox drops the ones from a previous session, and a stray one
    /// must not seal the entry the open gesture is still amending.
    #[gpui::test]
    async fn an_unknown_gesture_end_leaves_the_open_gesture_alone(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_BEGIN, 3);
        cart_gesture(&endpoint, GESTURE_END, 99);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).gesture, [3], "the stray End closed nothing");
        });
    }

    /// An instance's members have no document position of their own, so a
    /// member row moves the `[[instance]]` by the delta the primary member
    /// took -- one `MoveInstance`, not one op per member.
    #[gpui::test]
    async fn a_moved_instance_member_moves_the_instance(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();

        cart_rows(
            &endpoint,
            &[
                (0, 4.0, 4.0),
                (1, 40.0, 8.0),
                (2, 0.0, 0.0),
                (3, 42.0, 26.0),
            ],
        );
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(
                open_of(panel).store.state().instances[0].pos,
                [42.0, 26.0],
                "the instance took the member's delta"
            );
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            assert!(open.store.undo(), "one entry for the move");
            assert_eq!(open.store.state().instances[0].pos, [32.0, 16.0]);
        });
    }

    /// A component the cart changed lands in the document as `SetField`s
    /// for the fields that actually differ -- and a report that already
    /// reads that way changes nothing at all, which is what the cart's
    /// echo of the host's own write looks like.
    #[gpui::test]
    async fn a_component_change_from_the_cart_updates_the_document(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        let pos_before = panel.read_with(cx, |panel, _| {
            open_of(panel).store.state().entities[0].components["Transform"]["pos"].clone()
        });

        cart_component_changed(&endpoint, 0, &transform_bag([4.0, 4.0], 5));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let state = open_of(panel).store.state();
            assert_eq!(state.entities[0].components["Transform"]["z"], json!(5));
            assert_eq!(
                state.entities[0].components["Transform"]["pos"], pos_before,
                "the position it was already at is not rewritten -- the bag
                 decodes to `4.0` where the document authored `4`, and an op
                 between the two is one the store would fold away anyway"
            );
            assert!(
                !live_of(panel).world_dirty,
                "a mirrored component is not re-sent to the cart that reported it"
            );
        });

        // The same bag again -- what the cart publishes when the host's own
        // `set_component` echoes back off its dirty tracking.
        cart_component_changed(&endpoint, 0, &transform_bag([4.0, 4.0], 5));
        cx.run_until_parked();
        // Destructive, so it goes last: one entry for the change, and none
        // at all for the report that said what the document already held.
        assert_eq!(undo_depth(&panel, cx), 1);
    }

    /// An instance MEMBER's components belong to the instanced world file:
    /// only its transform mirrors, through the rows.
    #[gpui::test]
    async fn an_instance_members_component_change_is_ignored(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        let before = panel.read_with(cx, |panel, _| open_of(panel).store.state());

        // Index 3 is the one entity `worlds/sub` contributes.
        cart_component_changed(&endpoint, 3, &transform_bag([99.0, 99.0], 7));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).store.state().entities, before.entities);
            assert_eq!(open_of(panel).store.state().instances, before.instances);
        });
    }

    /// An inspector commit in Live is a `SetComponent` carrying the
    /// document's own component -- not a whole world blob, which would
    /// reload the cart under the field being typed into.
    #[gpui::test]
    async fn an_inspector_commit_in_live_sets_the_component(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::SetField {
                    entity: 0,
                    component: "Text".to_string(),
                    field: "content".to_string(),
                    value: json!("zz"),
                },
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                !live_of(panel).world_dirty,
                "a field the cart can be told about is not a resend"
            );
        });
        let sent = settle_live(&panel, &endpoint, cx);
        let writes = set_components(&sent);
        assert_eq!(writes.len(), 1, "one SetComponent, once");
        assert_eq!(writes[0].0, 0, "aimed at the cart index of entity 0");
        assert_eq!(bag_str(&writes[0].1, "content").as_deref(), Some("zz"));
        assert!(
            sent.iter().all(|message| message.first() != Some(&0x08)),
            "and no blob of any kind"
        );
    }

    /// Undo of a field edit is the same command with the value the field
    /// used to hold.
    #[gpui::test]
    async fn undo_of_a_field_edit_sends_the_old_bag(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::SetField {
                    entity: 0,
                    component: "Text".to_string(),
                    field: "content".to_string(),
                    value: json!("zz"),
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        let sent = settle_live(&panel, &endpoint, cx);
        let writes = set_components(&sent);
        assert_eq!(writes.len(), 1);
        assert_eq!(
            bag_str(&writes[0].1, "content").as_deref(),
            Some("ab"),
            "the fixture's own text, back on the cart"
        );
        assert!(
            sent.iter().all(|message| message.first() != Some(&0x08)),
            "an undo the cart can be told about does not reload it"
        );
    }

    /// `+ Entity` in Live spawns on the cart: an `Entity` blob at the
    /// reserved spawn index, and the document keeps the entity it added
    /// optimistically once the cart reports the index it gave it.
    #[gpui::test]
    async fn adding_an_entity_in_live_spawns_it_on_the_cart(cx: &mut TestAppContext) {
        use emerald_editor_runtime::wire::{self, BlobKind, HostMsg};

        let (panel, endpoint, _dir, cx) = connected_live_panel_flat(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).store.state().entities.len(), 2);
            assert!(
                !live_of(panel).world_dirty,
                "the cart is told about the entity, not handed the world"
            );
        });
        let sent = settle_live(&panel, &endpoint, cx);
        let spawns: Vec<u32> = sent
            .iter()
            .filter_map(|message| match wire::decode_host(message) {
                Some(HostMsg::BlobBegin {
                    kind: BlobKind::Entity,
                    index,
                    ..
                }) => Some(index),
                _ => None,
            })
            .collect();
        assert_eq!(
            spawns,
            vec![wire::SPAWN_INDEX],
            "one spawn, at the reserved index"
        );

        // The cart numbers it, which is the only way the host learns the
        // index -- and here it is the one the document appended at.
        cart_added(&endpoint, 1);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).store.state().entities.len(), 2);
            assert!(
                !live_of(panel).world_dirty,
                "the two flattenings agree, so nothing has to be re-sent"
            );
            assert!(live_of(panel).pending_spawns.is_empty());
        });
    }

    /// An `EntityAdded` nobody asked for -- one of the cart's own edit
    /// systems spawned it -- makes the host ask for a snapshot, which is
    /// the only place the entity's components exist.
    #[gpui::test]
    async fn a_cart_side_spawn_is_read_out_of_a_snapshot(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        host_sent(&endpoint);

        cart_added(&endpoint, 4);
        cx.run_until_parked();
        assert!(
            host_sent(&endpoint)
                .iter()
                .any(|message| message.as_slice() == [0x12]),
            "the host asks for a snapshot"
        );

        // `entity_count u32`, then per slot `index u32, comp_count u16,
        // bags` -- the fixture's four tracked entities plus the new one.
        let mut body = Vec::new();
        body.extend_from_slice(&5u32.to_le_bytes());
        for index in 0..5u32 {
            body.extend_from_slice(&index.to_le_bytes());
            body.extend_from_slice(&1u16.to_le_bytes());
            body.extend_from_slice(&transform_bag([f64::from(index), 9.0], 3));
        }
        cart_blob(&endpoint, 0, 0, &body);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let state = open_of(panel).store.state();
            assert_eq!(state.entities.len(), 4, "the document gained the entity");
            assert_eq!(
                state.entities[3].components["Transform"]["pos"],
                json!([4.0, 9.0]),
                "with the components the snapshot carried for index 4"
            );
            assert!(
                live_of(panel).world_dirty,
                "the document appended at 3 where the cart numbered 4, so
                 only a reload makes the two flattenings agree"
            );
        });
    }

    /// A spawn the cart never answers is taken back: the protocol has no
    /// refusal edge, so the wait running out is the only report there is.
    #[gpui::test]
    async fn a_spawn_the_cart_never_answers_is_taken_back(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_flat(cx).await;
        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        settle_live(&panel, &endpoint, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).pending_spawns.len(), 1);
        });

        cart_frames(&panel, &endpoint, cx, 61);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).store.state().entities.len(),
                1,
                "the optimistic entity is gone again"
            );
            assert_eq!(
                open_of(panel).live_error.as_deref(),
                Some("add entity refused by the cart")
            );
            assert!(live_of(panel).pending_spawns.is_empty());
        });
    }

    /// A despawn on the cart removes the entity from the document, and the
    /// undo brings it back.
    #[gpui::test]
    async fn a_cart_despawn_removes_the_entity_from_the_document(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).store.state().entities.len(), 3);
        });

        cart_removed(&endpoint, 2);
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            let state = open_of(panel).store.state();
            assert_eq!(state.entities.len(), 2);
            assert!(
                live_of(panel).world_dirty,
                "a removal shifts the document's indices, so the cart is re-sent"
            );
            assert!(
                state
                    .entities
                    .iter()
                    .all(|entity| !entity.components.contains_key("Camera")),
                "the camera entity is the one that went"
            );
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            assert!(open.store.undo(), "one entry for the despawn");
            assert_eq!(open.store.state().entities.len(), 3);
        });
    }

    /// The mirror folds what the cart MOVED, not the difference between
    /// two authorities. A row that has not moved since the last tick
    /// leaves the document alone -- otherwise a host-side edit still on
    /// its way to the cart would be rewritten from the cart's stale rows
    /// on the very next frame.
    #[gpui::test]
    async fn an_unmoved_row_does_not_rewrite_the_document(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_BEGIN, 4);
        cart_rows(&endpoint, &[(0, 40.0, 50.0)]);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_END, 4);
        cx.run_until_parked();

        // The host takes the move back. The cart has not been told yet --
        // that is Task 4's `SetTransform` -- so its rows still show the
        // moved position while the document shows the original.
        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        clear_world_dirty(&panel, cx);
        cart_rows(&endpoint, &[(0, 40.0, 50.0)]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                entity_pos_of(panel, 0),
                [4.0, 4.0],
                "the undone move stays undone"
            );
        });
    }

    /// The cart nests a Nudge inside a drag: `Begin a, Begin b, End b, End
    /// a`. Closing the inner gesture has to hand the rest of the drag back
    /// to the outer one -- with a single gesture slot the frames after
    /// `End b` are untagged, and each of them is its own undo entry.
    #[gpui::test]
    async fn a_nudge_nested_in_a_drag_leaves_three_undo_entries(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();

        cart_gesture(&endpoint, GESTURE_BEGIN, 1);
        cart_rows(&endpoint, &[(0, 10.0, 4.0)]);
        cx.run_until_parked();
        cart_rows(&endpoint, &[(0, 20.0, 4.0)]);
        cx.run_until_parked();

        // The nudge, inside the drag.
        cart_gesture(&endpoint, GESTURE_BEGIN, 2);
        cart_rows(&endpoint, &[(0, 21.0, 4.0)]);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_END, 2);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).gesture,
                [1],
                "the drag is still open underneath it"
            );
        });

        // ...and the drag carries on.
        cart_rows(&endpoint, &[(0, 30.0, 4.0)]);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_END, 1);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [30.0, 4.0]);
            assert!(live_of(panel).gesture.is_empty());
        });
        assert_eq!(
            undo_depth(&panel, cx),
            3,
            "the drag before the nudge, the nudge, and the drag after it"
        );
    }

    /// A user edit system animating an entity reports moved rows with no
    /// gesture around them. That is still one edit to take back, so a
    /// continuous burst folds under one synthetic gesture rather than
    /// leaving an undo entry per cart frame.
    #[gpui::test]
    async fn an_ungestured_burst_of_motion_is_one_undo_entry(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();

        for step in 1..=10 {
            cart_rows(&endpoint, &[(0, 4.0 + f64::from(step), 4.0)]);
            cx.run_until_parked();
        }
        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [14.0, 4.0]);
            assert!(
                live_of(panel).auto_gesture.is_some(),
                "the burst is still running"
            );
        });
        assert_eq!(undo_depth(&panel, cx), 1, "ten ticks, one entry");
    }

    /// ...and a tick where nothing moved ends the burst, so the motion
    /// that follows is a separate entry rather than an amendment of one
    /// the user has stopped thinking about.
    #[gpui::test]
    async fn a_still_tick_ends_the_burst(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();

        cart_rows(&endpoint, &[(0, 10.0, 4.0)]);
        cx.run_until_parked();
        cart_rows(&endpoint, &[(0, 10.0, 4.0)]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).auto_gesture,
                None,
                "the still tick retired it"
            );
        });
        cart_rows(&endpoint, &[(0, 20.0, 4.0)]);
        cx.run_until_parked();
        assert_eq!(undo_depth(&panel, cx), 2, "two bursts, two entries");
    }

    /// Play runs the GAME's systems: its entities move because the game
    /// moved them, and folding that would rewrite the document from a
    /// play-through. The rows still land, because the overlay is drawn
    /// from them.
    #[gpui::test]
    async fn play_mode_folds_no_rows_into_the_document(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            live_mut_of(panel).mode = EditorMode::Play;
        });

        cart_rows(&endpoint, &[(0, 90.0, 90.0)]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                entity_pos_of(panel, 0),
                [4.0, 4.0],
                "the game moved it, not the user"
            );
            assert_eq!(
                live_of(panel).rows.first().map(|row| [row.x, row.y]),
                Some([90.0, 90.0]),
                "and the overlay still follows the frame"
            );
        });
        assert_eq!(undo_depth(&panel, cx), 0, "no ops at all");
    }

    /// A mirrored despawn shifts every document index above it while the
    /// cart's table keeps the emptied slot, so until the reload lands the
    /// two flattenings do not describe each other. Nothing more is folded
    /// through the index map in that window -- a row folded through it
    /// would move whichever entity slid down into the slot.
    #[gpui::test]
    async fn rows_are_not_folded_while_the_reload_is_still_owed(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        // The encoder walks the instanced world to flatten this document;
        // without it every re-send fails, which is how the test holds the
        // window open instead of racing the next tick's blob.
        std::fs::remove_file(dir.path().join("worlds/sub.toml")).unwrap();

        cart_removed(&endpoint, 0);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let live = live_of(panel);
            assert!(live.world_dirty, "the reload is owed");
            assert!(live.loaded(), "and the rows still LOOK current");
            assert_eq!(open_of(panel).store.state().entities.len(), 2);
        });

        // The cart is still holding the pre-despawn world, so its index 1
        // is the entity the document now calls 0 -- and its index 2 is the
        // camera the document now calls 1.
        cart_rows(&endpoint, &[(1, 50.0, 8.0), (2, 0.0, 0.0), (3, 32.0, 16.0)]);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let state = open_of(panel).store.state();
            assert_eq!(
                inspector::entity_pos(&state, 1),
                Some([0.0, 0.0]),
                "the camera did not take the other entity's move"
            );
            assert_eq!(inspector::entity_pos(&state, 0), Some([40.0, 8.0]));
        });
        assert_eq!(undo_depth(&panel, cx), 1, "only the despawn");
    }

    /// Both members of a two-member instance move by the same delta. The
    /// document owns the `[[instance]]`, not its members, so that is ONE
    /// `MoveInstance` and one undo entry -- not one per member.
    #[gpui::test]
    async fn both_members_of_an_instance_move_it_once(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        // The fixture's `worlds/sub` contributes one entity; this is the
        // map the same document would have if it contributed two. Nothing
        // recomputes it here -- the map is rebuilt by a world push or a
        // recount, and the document has not moved.
        panel.update(cx, |panel, _| {
            live_mut_of(panel).index_map = live::IndexMap::new(3, &[2]);
        });
        cart_rows(
            &endpoint,
            &[
                (0, 4.0, 4.0),
                (1, 40.0, 8.0),
                (2, 0.0, 0.0),
                (3, 32.0, 16.0),
                (4, 48.0, 16.0),
            ],
        );
        cx.run_until_parked();

        cart_rows(
            &endpoint,
            &[
                (0, 4.0, 4.0),
                (1, 40.0, 8.0),
                (2, 0.0, 0.0),
                (3, 40.0, 24.0),
                (4, 56.0, 24.0),
            ],
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).store.state().instances[0].pos,
                [40.0, 24.0],
                "the instance took its primary member's delta"
            );
        });
        assert_eq!(undo_depth(&panel, cx), 1, "one entry, not one per member");
    }

    /// The cart groups its own selection and drags, so it is handed the
    /// document's `[[instance]]` spans once the world it describes has
    /// landed.
    #[gpui::test]
    async fn the_cart_is_told_the_documents_instance_groups(cx: &mut TestAppContext) {
        use emerald_editor_runtime::wire::{self, HostMsg};

        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(&endpoint, hello_ack(wire::LINK_PROTO_VERSION, &["Select"]));
        cx.run_until_parked();

        let sent = settle_live(&panel, &endpoint, cx);
        let groups: Vec<Vec<(u32, u32)>> = sent
            .iter()
            .filter_map(|message| match wire::decode_host(message) {
                Some(HostMsg::Groups {
                    part: 0,
                    total: 1,
                    groups,
                }) => Some(groups.collect()),
                _ => None,
            })
            .collect();
        assert_eq!(
            groups,
            [vec![(3, 1)]],
            "the fixture's one instance owns cart index 3 alone"
        );
    }

    /// The mirror never re-sends the world: the cart is already showing the
    /// move, and a blob would reload the world under the drag that made it.
    #[gpui::test]
    async fn a_mirrored_move_never_re_sends_the_world(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        host_sent(&endpoint);

        cart_gesture(&endpoint, GESTURE_BEGIN, 1);
        cart_rows(&endpoint, &[(0, 40.0, 50.0)]);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_END, 1);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [40.0, 50.0]);
            let live = live_of(panel);
            assert!(!live.world_dirty, "the cart already has this edit");
            assert!(live.loaded(), "and the overlay is not blanked for a blob");
        });
        let sent = host_sent(&endpoint);
        assert!(
            sent.iter().all(|message| message.first() != Some(&0x08)),
            "no blob for a mirrored move: {sent:?}"
        );
    }

    /// Every `SetTransform` in `sent`, as `(cart index, world x, world y)`,
    /// plus whether a world blob was started in the same batch.
    fn sent_transforms(sent: &[Vec<u8>]) -> (Vec<(u32, f64, f64)>, bool) {
        use emerald_editor_runtime::wire::{self, HostMsg};

        let blobbed = sent.iter().any(|message| message.first() == Some(&0x08));
        let transforms = sent
            .iter()
            .filter_map(|message| match wire::decode_host(message) {
                Some(HostMsg::SetTransform { id, x, y }) => {
                    Some((id, live::from_raw(x), live::from_raw(y)))
                }
                _ => None,
            })
            .collect();
        (transforms, blobbed)
    }

    /// The cart-side drag of `a_mirrored_move_never_re_sends_the_world`,
    /// left folded into the document with the session settled.
    async fn live_panel_after_a_mirrored_drag(
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorldPanel>,
        Arc<ggo_common::LinkEndpoint>,
        tempfile::TempDir,
        &mut gpui::VisualTestContext,
    ) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_BEGIN, 1);
        cart_rows(&endpoint, &[(0, 40.0, 50.0)]);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_END, 1);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [40.0, 50.0], "the drag folded in");
        });
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);
        (panel, endpoint, dir, cx)
    }

    /// Undoing a MOVE replays it to the cart as a `SetTransform` per
    /// affected index. The cart is holding the right world already: a blob
    /// would blank its rows for a round trip to say what one datagram
    /// says, so there must not be one.
    #[gpui::test]
    async fn undo_of_a_move_replays_a_transform_instead_of_the_world(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = live_panel_after_a_mirrored_drag(cx).await;

        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [4.0, 4.0], "the document undid it");
            assert!(
                !live_of(panel).world_dirty,
                "and owes the cart no world for it"
            );
        });
        let sent = settle_live(&panel, &endpoint, cx);

        let (transforms, blobbed) = sent_transforms(&sent);
        assert_eq!(
            transforms,
            vec![(0, 4.0, 4.0)],
            "the undone position went out as one SetTransform"
        );
        assert!(!blobbed, "and no world blob went with it");

        panel.update(cx, |panel, cx| panel.redo_impl(cx));
        let sent = settle_live(&panel, &endpoint, cx);
        let (transforms, blobbed) = sent_transforms(&sent);
        assert_eq!(
            transforms,
            vec![(0, 40.0, 50.0)],
            "and redo replays the moved position the same way"
        );
        assert!(!blobbed);
    }

    /// A row that comes back where the replayed transform put it is the
    /// host's own undo landing, not cart-side motion: folding it would
    /// write the undone delta back and open an undo entry for the undo.
    #[gpui::test]
    async fn a_replayed_row_is_not_mirrored_back_into_the_document(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = live_panel_after_a_mirrored_drag(cx).await;
        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        settle_live(&panel, &endpoint, cx);

        // The cart applying it: the row comes back at the undone position.
        cart_rows(&endpoint, &[(0, 4.0, 4.0), (1, 40.0, 8.0)]);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(entity_pos_of(panel, 0), [4.0, 4.0], "and nothing re-folded");
            assert!(
                live_of(panel).replayed_rows.is_empty(),
                "the replay was retired by the row that fulfilled it"
            );
        });
        assert_eq!(undo_depth(&panel, cx), 0, "the undo did not become an edit");
    }

    /// A whole-component write and a row for the same entity BOTH go out,
    /// in the order they were asked for. The bag carries the WHOLE
    /// component (`z` and all); the row carries only x and y. Letting the
    /// row cancel the bag lost `z`, and the cart -- which rebuilds a
    /// component whole -- then published it back at its default and the
    /// mirror undid the user's edit.
    #[gpui::test]
    async fn a_queued_component_survives_a_later_move(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_flat(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::SetField {
                    entity: 0,
                    component: "Transform".to_string(),
                    field: "z".to_string(),
                    value: json!(3),
                },
                cx,
            );
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [40.0, 4.0],
                    gesture: None,
                },
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).pending_edits.len(),
                2,
                "both are owed the cart"
            );
            assert!(!live_of(panel).world_dirty, "and neither is a resend");
        });

        let sent = settle_live(&panel, &endpoint, cx);
        let writes = set_components(&sent);
        assert_eq!(writes.len(), 1, "the component write was not cancelled");
        assert_eq!(bag_int(&writes[0].1, "z"), Some(3), "carrying the new z");
        let (transforms, _) = sent_transforms(&sent);
        assert_eq!(
            transforms,
            vec![(0, 40.0, 4.0)],
            "and the row behind it places the entity"
        );
        // Order matters: the bag holds the position it was PLANNED with,
        // so the row has to land after it.
        let bag_at = sent
            .iter()
            .position(|message| message.first() == Some(&0x10))
            .expect("a SetComponent");
        let row_at = sent
            .iter()
            .position(|message| message.first() == Some(&0x02))
            .expect("a SetTransform");
        assert!(bag_at < row_at, "the component goes first");
    }

    /// Two transforms for one index on consecutive ticks: the answers come
    /// back in the order they went out, and neither folds. Erasing the
    /// replay record when the second was queued let the FIRST answer read
    /// as cart-side motion, which is one spurious undo entry per pair of
    /// undos that straddle a tick.
    #[gpui::test]
    async fn back_to_back_replays_both_retire_without_folding(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_flat(cx).await;
        settle_live(&panel, &endpoint, cx);
        cart_rows(&endpoint, &[(0, 4.0, 4.0)]);
        cx.run_until_parked();
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [10.0, 4.0],
                    gesture: None,
                },
                cx,
            );
        });
        cart_frames(&panel, &endpoint, cx, 1);
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [20.0, 4.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).replayed_rows.len(),
                2,
                "both transforms are on the wire, both awaiting an answer"
            );
        });

        // The cart applies them in order, a frame apart, and republishes.
        cart_rows(&endpoint, &[(0, 10.0, 4.0)]);
        cx.run_until_parked();
        cart_rows(&endpoint, &[(0, 20.0, 4.0)]);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                entity_pos_of(panel, 0),
                [20.0, 4.0],
                "the document is where the user put it"
            );
        });
        assert_eq!(
            undo_depth(&panel, cx),
            2,
            "two moves, two entries -- and nothing folded from their answers"
        );
    }

    /// The index map has to follow an entity the document just appended:
    /// nothing else rebuilds it before the next world blob, and a cart row
    /// for an index the map does not name is dropped on the floor.
    #[gpui::test]
    async fn a_row_for_a_just_added_entity_moves_the_document(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_flat(cx).await;
        settle_live(&panel, &endpoint, cx);
        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        settle_live(&panel, &endpoint, cx);
        cart_added(&endpoint, 1);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                !live_of(panel).world_dirty,
                "the cart numbered it where the document did"
            );
        });

        cart_rows(&endpoint, &[(0, 4.0, 4.0), (1, 90.0, 90.0)]);
        cx.run_until_parked();
        cart_rows(&endpoint, &[(0, 4.0, 4.0), (1, 99.0, 90.0)]);
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                entity_pos_of(panel, 1),
                [99.0, 90.0],
                "dragging the entity just added moves it in the document"
            );
        });
    }

    /// Play discards every report the cart makes, and the cart republishes
    /// nothing on its own -- so coming back into Edit re-sends the whole
    /// world, which is the only thing that can say the entity table at
    /// once. Without it the two sides stay silently out of step.
    #[gpui::test]
    async fn leaving_play_re_sends_the_world(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_flat(cx).await;
        settle_live(&panel, &endpoint, cx);
        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Play, cx));
        cx.run_until_parked();
        host_sent(&endpoint);

        // Reports made while the game runs are the game's, and dropped.
        cart_component_changed(&endpoint, 0, &transform_bag([77.0, 77.0], 9));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).store.state().entities[0].components["Transform"]["z"],
                json!(0),
                "a play-through is not an edit"
            );
        });

        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Edit, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).world_dirty, "the cart is owed the world");
        });
        let sent = settle_live(&panel, &endpoint, cx);
        assert!(
            sent.iter().any(|message| message.first() == Some(&0x08)),
            "and the blob goes out"
        );
    }

    /// A `ComponentChanged` for an index the flattening does not name --
    /// past the document's entities and past every instance member -- is
    /// dropped rather than aimed at whatever sits at that document index.
    #[gpui::test]
    async fn a_component_change_for_an_unknown_index_is_dropped(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        let before = panel.read_with(cx, |panel, _| open_of(panel).store.state());

        cart_component_changed(&endpoint, 99, &transform_bag([12.0, 12.0], 8));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).store.state().entities, before.entities);
            assert!(!live_of(panel).world_dirty, "and it is not a resend either");
        });
    }

    /// A `Transform` report whose position differs from the document's IS
    /// folded, gesture or no gesture: the cart is the authority on where
    /// its rows are, and a report that arrives after the drag's `End` is
    /// still the drag's result.
    #[gpui::test]
    async fn a_transform_report_after_a_gesture_end_still_moves_the_document(
        cx: &mut TestAppContext,
    ) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_flat(cx).await;
        settle_live(&panel, &endpoint, cx);
        cart_rows(&endpoint, &[(0, 4.0, 4.0)]);
        cx.run_until_parked();
        cart_gesture(&endpoint, GESTURE_BEGIN, 1);
        cart_gesture(&endpoint, GESTURE_END, 1);
        cx.run_until_parked();

        cart_component_changed(&endpoint, 0, &transform_bag([31.0, 17.0], 0));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                entity_pos_of(panel, 0),
                [31.0, 17.0],
                "the cart's own position, folded into the document"
            );
        });
        assert_eq!(undo_depth(&panel, cx), 1, "as one entry of its own");
    }

    /// Only MOVES replay: undoing a despawn puts an entity back, which
    /// shifts every index above it, and nothing but the blob can say that.
    ///
    /// This fixture is the INSTANCED one, so what refuses here is
    /// `spawnable` -- a world with `[[instance]]`s cannot describe any
    /// structural change as a command. `a_middle_removal_in_a_flat_world_
    /// sends_the_blob` covers the other reason.
    #[gpui::test]
    async fn undo_of_a_removal_still_re_sends_the_world(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();
        cart_removed(&endpoint, 0);
        cx.run_until_parked();
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        panel.read_with(cx, |panel, _| {
            assert!(
                live_of(panel).world_dirty,
                "the cart is owed the whole world"
            );
        });
        let sent = settle_live(&panel, &endpoint, cx);

        let (transforms, blobbed) = sent_transforms(&sent);
        assert!(blobbed, "the restored entity went out as a world blob");
        assert!(
            transforms.is_empty(),
            "and not as transforms: {transforms:?}"
        );
    }

    /// How many undo entries the open document has, by unwinding the whole
    /// stack. Destructive on purpose: a test that asks this is asserting
    /// on the SHAPE of the history, which is what a mirrored gesture is
    /// judged by, and it has nothing to do afterwards.
    fn undo_depth(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) -> usize {
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            let mut depth = 0;
            while open.store.undo() {
                depth += 1;
            }
            depth
        })
    }

    /// Clear the flag, so the next funnel under test has to set it on its
    /// own rather than inherit the previous one's mark.
    fn clear_world_dirty(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            if let Some(live) = &mut open.live {
                live.world_dirty = false;
            }
        });
    }

    /// Structural edits never describe themselves to the cart: every
    /// funnel that reaches the store marks the world dirty, and the next
    /// free tick re-sends the whole blob. Asserted one funnel at a time --
    /// a single "something marked it" would pass with all but one of the
    /// call sites missing.
    #[gpui::test]
    async fn structural_edits_mark_the_world_dirty_and_the_next_tick_sends_a_blob(
        cx: &mut TestAppContext,
    ) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        clear_world_dirty(&panel, cx);

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected = vec![Selection::Entity(0)];
            panel.delete_selected_now(cx);
        });
        panel.read_with(cx, |panel, _| assert!(live_of(panel).world_dirty, "delete"));
        clear_world_dirty(&panel, cx);

        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        panel.read_with(cx, |panel, _| assert!(live_of(panel).world_dirty, "add"));
        clear_world_dirty(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.apply_op(WorldOp::RemoveInstance { index: 0 }, cx)
        });
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).world_dirty, "apply_op")
        });
        clear_world_dirty(&panel, cx);

        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        panel.read_with(cx, |panel, _| assert!(live_of(panel).world_dirty, "undo"));
        clear_world_dirty(&panel, cx);

        panel.update(cx, |panel, cx| panel.redo_impl(cx));
        panel.read_with(cx, |panel, _| assert!(live_of(panel).world_dirty, "redo"));
        clear_world_dirty(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.add_instance_impl("worlds/sub".to_string(), cx)
        });
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).world_dirty, "add instance");
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).instance_counts,
                vec![1],
                "the counts are re-asked for off-thread; this path never
                 goes through `apply_op`"
            );
        });
        clear_world_dirty(&panel, cx);

        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected = vec![Selection::Instance(0)];
            panel.copy_impl(cx);
            panel.paste_impl(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).world_dirty, "paste");
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).instance_counts,
                vec![1, 1],
                "a pasted instance is recounted too"
            );
        });

        endpoint.tick();
        cx.run_until_parked();
        assert!(
            host_sent(&endpoint)
                .iter()
                .any(|message| message.first() == Some(&0x08) && message.get(1) == Some(&0)),
            "BlobBegin for a world document after a structural edit"
        );
    }

    /// Arrow keys with nothing selected look around, which in Live is a
    /// camera move -- the same contract a middle-drag has, and the design
    /// The arrows in Live are the cart's: it owns the selection they move
    /// and the camera they look around with, so the host queues a
    /// `Nudge` and changes nothing of its own.
    #[gpui::test]
    async fn the_arrows_in_live_go_to_the_cart_as_a_nudge(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        let design_pan_before = panel.read_with(cx, |panel, _| panel.test_design_pan());
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected.clear();
            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowUp", true, cx);
        });
        endpoint.tick();
        cx.run_until_parked();
        let sent = host_sent(&endpoint);
        assert_eq!(
            commands_in(&sent),
            vec![
                (emerald_editor_runtime::wire::command::NUDGE, 1, 0),
                (emerald_editor_runtime::wire::command::NUDGE, 0, -16),
            ],
            "one pixel, then one tile"
        );
        assert!(
            sent.iter().all(|message| message.first() != Some(&0x03)),
            "the host has no camera of its own to move any more"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.test_design_pan()),
            design_pan_before
        );
        panel.read_with(cx, |panel, _| {
            assert!(
                !open_of(panel).store.state().dirty,
                "the document is the cart's to move, not the host's"
            );
        });
    }

    /// Reset view in Live is the Live framing's own reset: back to the
    /// fitted scale, with the cart pointed at the document's camera origin
    /// again. The design zoom and pan are not what it resets.
    #[gpui::test]
    async fn reset_view_in_live_refits_the_scale_and_re_aims_the_cart(cx: &mut TestAppContext) {
        let (panel, _endpoint, _dir, cx) = connected_live_panel(cx).await;
        let design_pan_before = panel.read_with(cx, |panel, _| panel.test_design_pan());
        panel.update(cx, |panel, cx| {
            panel.live_wheel(1, [640.0, 480.0], cx);
            panel.reset_view_impl(cx);
        });
        panel.read_with(cx, |panel, _| {
            let origin = active_camera_origin(&open_of(panel).store.state());
            let live = live_of(panel);
            assert_eq!(live.scale, None, "back to fit");
            assert_eq!(live.pending_camera, Some(origin));
            assert_eq!(panel.test_design_pan(), design_pan_before);
        });
    }

    /// The cart's camera report, not the design pan, is what Live draws
    /// against: a report of (100, 50) puts a row at world (110, 60) ten px
    /// right and down of the frame's origin at scale 1.
    #[gpui::test]
    async fn live_overlay_rows_follow_the_carts_camera_report(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_rows(&endpoint, &[(0, 110.0, 60.0)]);
        cart_camera(&endpoint, 100.0, 50.0);
        cart_frame(&endpoint, 3);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).camera, Some([100.0, 50.0]));
        });
        // 640x480 canvas -> scale 2, frame origin (0, 0): the row's screen
        // rect is ((110 - 100) * 2, (60 - 50) * 2) = (20, 20), 32x32.
        let rect = panel.read_with(cx, |panel, _| {
            panel
                .test_live_row_screen_rect(0, [640.0, 480.0])
                .expect("a published row")
        });
        assert_eq!(rect, [20.0, 20.0, 32.0, 32.0]);
    }

    /// The overlay follows the cart's REPORT, never the camera a pan still
    /// owes it: a cart system that moves the camera has to be able to win,
    /// so the picture and the outlines lag together by one report instead
    /// of the outlines running ahead.
    #[gpui::test]
    async fn the_live_overlay_ignores_a_camera_the_cart_has_not_reported(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_rows(&endpoint, &[(0, 110.0, 60.0)]);
        cart_camera(&endpoint, 100.0, 50.0);
        cart_frame(&endpoint, 3);
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            live_mut_of(panel).pending_camera = Some([200.0, 150.0]);
        });
        let rect = panel.read_with(cx, |panel, _| {
            panel
                .test_live_row_screen_rect(0, [640.0, 480.0])
                .expect("a published row")
        });
        assert_eq!(
            rect,
            [20.0, 20.0, 32.0, 32.0],
            "the report, not the owed camera, places the outline"
        );
    }

    /// The Live prepaint stamps `ViewShared.last_bounds` itself -- it must
    /// not go through `layout_camera`, which would also centre the DESIGN
    /// pan -- and every Live gesture converts through that stamp.
    #[gpui::test]
    async fn a_live_render_stamps_the_canvas_bounds_for_gestures(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_rows(&endpoint, &[(0, 4.0, 4.0)]);
        cart_frame(&endpoint, 3);
        // A frame, or the boot screen stands in for the canvas and there
        // is no Live layout to stamp anything.
        let image = ggo_common::to_render_image(&vec![0u8; 320 * 240 * 4], 320, 240)
            .expect("a 320x240 RGBA buffer");
        *endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((1, image));
        endpoint.tick();
        cx.run_until_parked();
        // Forget what the helper stamped: only a real Live layout can put
        // it back.
        panel.update(cx, |panel, cx| {
            open_of(panel).view.borrow_mut().last_bounds = None;
            cx.notify();
        });
        show_panel(cx);
        let size = panel
            .read_with(cx, |panel, _| panel.live_canvas_size())
            .expect("the live prepaint stamped the canvas bounds");
        assert!(
            size[0] > 0.0 && size[1] > 0.0,
            "a laid-out canvas, not a zero one: {size:?}"
        );
        // And the stamp is what places world px on the canvas: a point
        // inside the device frame lands inside the laid-out canvas.
        let at = panel.read_with(cx, |panel, _| live_screen_of(panel, [5.0, 5.0]));
        assert!(
            at[0] >= 0.0 && at[0] <= size[0] && at[1] >= 0.0 && at[1] <= size[1],
            "world (5, 5) through the stamped transform: {at:?} in {size:?}"
        );
    }

    /// Every `Pointer` the host put on the wire, in order.
    fn pointers_in(sent: &[Vec<u8>]) -> Vec<(i16, i16, u8, u8, bool)> {
        use emerald_editor_runtime::wire::{self, HostMsg};
        sent.iter()
            .filter_map(|message| match wire::decode_host(message) {
                Some(HostMsg::Pointer {
                    x,
                    y,
                    buttons,
                    modifiers,
                    snap,
                }) => Some((x, y, buttons, modifiers, snap)),
                _ => None,
            })
            .collect()
    }

    /// Every `Command` the host put on the wire, in order.
    fn commands_in(sent: &[Vec<u8>]) -> Vec<(u8, i32, i32)> {
        use emerald_editor_runtime::wire::{self, HostMsg};
        sent.iter()
            .filter_map(|message| match wire::decode_host(message) {
                Some(HostMsg::Command { kind, a, b }) => Some((kind, a, b)),
                _ => None,
            })
            .collect()
    }

    /// One turn of the live poll, and everything it put on the wire.
    fn live_tick_sent(
        endpoint: &ggo_common::LinkEndpoint,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<Vec<u8>> {
        endpoint.tick();
        cx.run_until_parked();
        host_sent(endpoint)
    }

    /// The whole Live gesture, forwarded: press, move and release reach
    /// the cart as device px with the buttons held, and the host neither
    /// selects nor moves anything of its own.
    #[gpui::test]
    async fn a_live_gesture_is_forwarded_and_changes_nothing_host_side(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cart_frame(&endpoint, 3);
        cx.run_until_parked();
        host_sent(&endpoint);
        // The 800x600 canvas the helper stamped fits the 320x240 frame at
        // scale 2, with the frame origin at (80, 60).
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.snap = true;
        });

        panel.update(cx, |panel, _| {
            assert!(panel.canvas_button_down(
                [100.0, 80.0],
                MouseButton::Left,
                &Modifiers::shift()
            ));
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(10, 10, 1, 1, true)],
            "device px, left held, shift, and the host's snap toggle"
        );

        panel.update(cx, |panel, _| {
            assert!(panel.canvas_pointer_move(
                [140.0, 100.0],
                Some(MouseButton::Left),
                &Modifiers::none()
            ));
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(30, 20, 1, 0, true)],
            "the drag keeps the button held"
        );

        panel.update(cx, |panel, _| {
            assert!(panel.canvas_button_up([140.0, 100.0], MouseButton::Left, &Modifiers::none()));
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(30, 20, 0, 0, true)],
            "and the release lets it go"
        );

        panel.read_with(cx, |panel, _| {
            let open = open_of(panel);
            assert!(open.selected.is_empty(), "the host never hit-tested");
            assert!(open.marquee.is_none(), "nor started a band");
            assert!(
                !open.store.state().dirty,
                "nor moved anything: the cart owns the gesture"
            );
        });
    }

    /// A press and the release that ends it inside ONE tick both reach the
    /// cart, in order and on separate frames: it derives its press and
    /// release edges from consecutive samples, so folding them would lose
    /// the click and swapping them would leave a press it never sees
    /// released.
    #[gpui::test]
    async fn a_press_and_release_in_one_tick_reach_the_cart_in_order(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);

        panel.update(cx, |panel, _| {
            panel.canvas_button_down([100.0, 80.0], MouseButton::Left, &Modifiers::none());
            panel.canvas_pointer_move([104.0, 80.0], Some(MouseButton::Left), &Modifiers::none());
            panel.canvas_button_up([104.0, 80.0], MouseButton::Left, &Modifiers::none());
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(10, 10, 1, 0, false)],
            "the press goes first, at the point it was made"
        );
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(12, 10, 1, 0, false)],
            "then the drag inside it"
        );
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(12, 10, 0, 0, false)],
            "and the release last, so no press is left held"
        );
        // An edge is the one sample a dropped datagram would cost the
        // cart outright, so it goes out twice more while nothing else is
        // queued. Identical, so the cart sees no second release.
        for _ in 0..2 {
            assert_eq!(
                pointers_in(&live_tick_sent(&endpoint, cx)),
                vec![(12, 10, 0, 0, false)],
                "the release is repeated"
            );
        }
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            Vec::new(),
            "and then an idle tick sends nothing"
        );
    }

    /// A release the canvas is not under still reaches the cart: gpui
    /// hands `on_mouse_up` to the hovered element only, so without the
    /// out-of-bounds path the cart would drag on until the pointer came
    /// back.
    #[gpui::test]
    async fn a_release_off_the_canvas_still_reaches_the_cart(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, _| {
            panel.canvas_button_down([100.0, 80.0], MouseButton::Left, &Modifiers::none());
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(10, 10, 1, 0, false)]
        );

        // Up at a point the canvas never covered: the device px go with
        // it, out of the frame and saturating if need be.
        panel.update(cx, |panel, _| {
            panel.canvas_button_up_out([-40.0, 900.0], MouseButton::Left, &Modifiers::none());
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(-60, 420, 0, 0, false)],
            "the release goes out with no button held"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).pointer_buttons,
                0,
                "and the mask is clear, so the next press is a fresh press"
            );
        });

        // A release for a button that never went down on this canvas is
        // not the cart's business: an out-of-bounds up fires for every
        // click anywhere in the window. (The wire still carries the edge
        // repeats, so what is asserted is that nothing NEW was queued.)
        panel.update(cx, |panel, _| {
            panel.canvas_button_up_out([-40.0, 900.0], MouseButton::Left, &Modifiers::none());
        });
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).pending_pointer.is_empty());
        });
    }

    /// The cursor leaving the canvas with a button held ends the cart's
    /// gesture there, at the last point the cart was told about -- gpui
    /// stops delivering moves once it is outside, so a gesture left open
    /// would freeze and then jump to wherever the button came up.
    #[gpui::test]
    async fn leaving_the_canvas_releases_the_button_the_cart_is_holding(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, _| {
            panel.canvas_button_down([100.0, 80.0], MouseButton::Left, &Modifiers::none());
            panel.canvas_pointer_move([140.0, 100.0], Some(MouseButton::Left), &Modifiers::none());
            assert!(panel.live_release_held(), "a button was held");
            assert!(
                !panel.live_release_held(),
                "and nothing is held the second time"
            );
        });
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.extend(pointers_in(&live_tick_sent(&endpoint, cx)));
        }
        assert_eq!(
            seen,
            vec![
                (10, 10, 1, 0, false),
                (30, 20, 1, 0, false),
                (30, 20, 0, 0, false)
            ],
            "the release lands at the last point the cart was told about"
        );
    }

    /// A command queued while a blob window is open waits for it: four
    /// chunks and a command are five datagrams into a receive queue four
    /// deep, and a swallowed command is a keypress the user has to guess
    /// went missing.
    #[gpui::test]
    async fn a_command_waits_for_the_carts_receive_queue(cx: &mut TestAppContext) {
        use emerald_editor_runtime::wire::command;
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).mailbox.busy(), "a world blob is in flight");
        });

        // Not drained here: the chunks on the wire are what the cart has
        // to answer for the transfer to finish at all.
        panel.update(cx, |panel, cx| panel.select_all_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).pending_commands.len(),
                1,
                "still owed: nothing goes out on top of the transfer"
            );
        });

        // Answer the transfer the way the cart does; the first tick with a
        // free queue is the one that sends it.
        let sent = settle_live(&panel, &endpoint, cx);
        assert_eq!(commands_in(&sent), vec![(command::SELECT_ALL, 0, 0)]);
    }

    /// The arrows under the brush stay the brush's, in Live as in Design:
    /// a `Nudge` would move the cart's selection out from under it.
    #[gpui::test]
    async fn arrows_while_painting_in_live_send_no_command(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_with_background(cx).await;
        panel.update(cx, |panel, cx| {
            panel.enter_paint_mode(PaintTarget::BgSlot(0), cx);
        });
        cx.run_until_parked();
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| {
            panel.nudge_impl("ArrowRight", false, cx);
            panel.nudge_impl("ArrowUp", true, cx);
        });
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).pending_commands.is_empty());
        });
        assert_eq!(commands_in(&live_tick_sent(&endpoint, cx)), Vec::new());
    }

    /// A middle-drag in Live is forwarded like any other sample -- the
    /// cart pans its own camera from it -- and the design pan is left
    /// exactly where the design view had it.
    #[gpui::test]
    async fn a_middle_drag_in_live_is_forwarded_and_never_touches_the_design_pan(
        cx: &mut TestAppContext,
    ) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_camera(&endpoint, 100.0, 50.0);
        cart_frame(&endpoint, 3);
        cx.run_until_parked();
        host_sent(&endpoint);
        let design_pan_before = panel.read_with(cx, |panel, _| panel.test_design_pan());

        panel.update(cx, |panel, _| {
            assert!(panel.canvas_button_down(
                [100.0, 80.0],
                MouseButton::Middle,
                &Modifiers::none()
            ));
        });
        let sent = live_tick_sent(&endpoint, cx);
        assert_eq!(pointers_in(&sent), vec![(10, 10, 2, 0, false)]);
        assert!(
            sent.iter().all(|message| message.first() != Some(&0x03)),
            "the camera is the cart's own to move now"
        );

        panel.update(cx, |panel, _| {
            assert!(panel.canvas_pointer_move(
                [160.0, 100.0],
                Some(MouseButton::Middle),
                &Modifiers::none()
            ));
        });
        let sent = live_tick_sent(&endpoint, cx);
        assert_eq!(pointers_in(&sent), vec![(40, 20, 2, 0, false)]);
        assert!(sent.iter().all(|message| message.first() != Some(&0x03)));

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.test_design_pan(), design_pan_before);
            assert_eq!(
                live_of(panel).pending_camera,
                None,
                "the host owes the cart no camera"
            );
        });
    }

    /// A release the canvas never gets a mouse-up for -- the button came
    /// up somewhere else -- still reaches the cart on the next move, or
    /// the cart would drag until the end of the session.
    #[gpui::test]
    async fn a_move_with_no_button_held_releases_the_cart_s_press(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, _| {
            panel.canvas_button_down([100.0, 80.0], MouseButton::Left, &Modifiers::none());
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(10, 10, 1, 0, false)]
        );
        panel.update(cx, |panel, _| {
            panel.canvas_pointer_move([120.0, 80.0], None, &Modifiers::none());
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(20, 10, 0, 0, false)]
        );
    }

    /// Delete, duplicate, select-all and clear in Live are commands to the
    /// cart against ITS selection; the host applies no op of its own, not
    /// even to a selection the mirror left behind.
    #[gpui::test]
    async fn live_edit_actions_go_to_the_cart_as_commands(cx: &mut TestAppContext) {
        use emerald_editor_runtime::wire::command;
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        let entities_before =
            panel.read_with(cx, |panel, _| open_of(panel).store.state().entities.len());
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected = vec![Selection::Entity(0)];
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.select_all_impl(cx);
            panel.duplicate_impl(cx);
            panel.delete_selected_impl(window, cx);
            panel.clear_selection_impl(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            commands_in(&live_tick_sent(&endpoint, cx)),
            vec![
                (command::SELECT_ALL, 0, 0),
                (command::DUPLICATE, 0, 0),
                (command::DELETE, 0, 0),
                (command::CLEAR_SELECTION, 0, 0),
            ],
            "every command of the tick goes out, in the order asked for"
        );
        panel.read_with(cx, |panel, _| {
            let open = open_of(panel);
            assert_eq!(
                open.store.state().entities.len(),
                entities_before,
                "the host deleted and duplicated nothing"
            );
            assert!(!open.store.state().dirty);
            assert_eq!(
                open.selected,
                vec![Selection::Entity(0)],
                "and left the mirrored selection for the cart to replace"
            );
        });
    }

    /// Paint mode keeps the LEFT button in both modes -- the brush is the
    /// host's -- but nothing else: the middle drag that pans the cart's
    /// camera has to keep working while a map is under the brush.
    #[gpui::test]
    async fn painting_in_live_keeps_the_brush_but_not_the_camera(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_with_background(cx).await;
        panel.update(cx, |panel, cx| {
            panel.enter_paint_mode(PaintTarget::BgSlot(0), cx);
        });
        cx.run_until_parked();
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, _| {
            assert!(
                !panel.canvas_button_down([100.0, 80.0], MouseButton::Left, &Modifiers::none()),
                "the brush keeps the left button"
            );
            assert!(
                panel.canvas_button_down([100.0, 80.0], MouseButton::Middle, &Modifiers::none()),
                "the camera pan is still the cart's"
            );
        });
        assert_eq!(
            pointers_in(&live_tick_sent(&endpoint, cx)),
            vec![(10, 10, 2, 0, false)]
        );
    }

    /// A cart being replaced takes the host's idea of the mouse with it: a
    /// press queued for the outgoing cart would reach the new one as a
    /// press it never gets a release for. The one sample that does go out
    /// is the RELEASE of whatever the host thought was held -- a greeting
    /// resets the link's mirrors, not the cart's pointer fields, so a
    /// button left down there is one the new session still believes is.
    #[gpui::test]
    async fn a_greeting_drops_the_input_owed_the_previous_cart(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        panel.update(cx, |panel, _| {
            panel.canvas_button_down([100.0, 80.0], MouseButton::Left, &Modifiers::none());
            live_mut_of(panel).status = LiveStatus::Connecting;
        });
        let sent = live_tick_sent(&endpoint, cx);
        assert_eq!(
            pointers_in(&sent),
            vec![(10, 10, 0, 0, false)],
            "the press never left, and the new cart is told nothing is held"
        );
        panel.read_with(cx, |panel, _| {
            let live = live_of(panel);
            assert!(live.pending_pointer.is_empty());
            assert_eq!(live.pointer_buttons, 0);
        });
    }

    /// The wheel in Live steps the integer picture scale; it never touches
    /// the design zoom ladder and sends nothing to the cart.
    #[gpui::test]
    async fn wheel_in_live_steps_the_integer_scale(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        let design_zoom_before = panel.read_with(cx, |panel, _| open_of(panel).view.borrow().zoom);
        // Through the real wheel handler: the design ladder stays put and
        // the cart hears nothing -- the scale is the host's own.
        panel.update(cx, |panel, cx| {
            panel.wheel_zoom(&wheel_event(40.0, 30.0, 20.0), cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).view.borrow().zoom, design_zoom_before);
            assert_eq!(live_of(panel).pending_camera, None);
        });
        endpoint.tick();
        cx.run_until_parked();
        assert!(
            host_sent(&endpoint)
                .iter()
                .all(|message| message.first() != Some(&0x03)),
            "a wheel notch is not a camera move"
        );

        panel.update(cx, |panel, _| live_mut_of(panel).scale = None);
        panel.update(cx, |panel, cx| panel.live_wheel(1, [640.0, 480.0], cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_scale([640.0, 480.0])),
            3,
            "fit 2 -> 3"
        );
        panel.update(cx, |panel, cx| {
            for _ in 0..10 {
                panel.live_wheel(-1, [640.0, 480.0], cx);
            }
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_scale([640.0, 480.0])),
            1
        );
    }

    /// Painting a background slot re-sends that slot: the cart holds the
    /// layers as its own buffers, not as part of the world document.
    #[gpui::test]
    async fn painting_in_live_resends_the_layer(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel_with_background(cx).await;
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| {
            panel.enter_paint_mode(PaintTarget::BgSlot(0), cx);
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, _, cx| {
            panel.canvas_primary_down_with(live_screen_of(panel, [1.0, 1.0]), false, cx);
            panel.canvas_primary_up(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                live_of(panel).layers_dirty.any(),
                "the stroke marked the slot"
            );
        });
        endpoint.tick();
        cx.run_until_parked();

        assert!(
            host_sent(&endpoint)
                .iter()
                .any(|message| message.first() == Some(&0x08) && message.get(1) == Some(&1)),
            "a Layer BlobBegin"
        );
    }

    /// The layer payloads are read from DISK, and a `PaintSession` holds
    /// its cells in memory until it is saved -- so the save is what makes
    /// the stroke's bytes reachable, and it has to re-arm the slot itself.
    /// Without that the stroke only ever reached the cart if some LATER
    /// paint happened to re-dirty the slot.
    #[gpui::test]
    async fn saving_a_painted_map_ships_the_painted_cells_to_the_cart(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_with_background(cx).await;

        panel.update(cx, |panel, cx| {
            panel.enter_paint_mode(PaintTarget::BgSlot(0), cx);
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, _, cx| {
            panel.canvas_primary_down_with(live_screen_of(panel, [1.0, 1.0]), false, cx);
            panel.canvas_primary_up(cx);
        });
        // Everything the stroke itself pushed, so what is asserted below
        // is what the SAVE sent.
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();
        // The save waits on the cart's world. Its layer 0 answer is the
        // PRE-stroke map (nothing has told it about the stroke yet), which
        // is exactly what must not be folded back over the session.
        answer_cart_save(
            &panel,
            &endpoint,
            cx,
            &[(0, vec![transform_bag([4.0, 4.0], 0)])],
            &[(0, 2, 2, vec![live::BLANK_TILE; 4])],
        );
        let sent = settle_live(&panel, &endpoint, cx);

        let saved = io::open_map(dir.path(), "maps/test.bg0.map").expect("the painted map saved");
        assert_ne!(
            saved.cells.first().copied(),
            Some(live::BLANK_TILE),
            "the stroke must actually have painted a tile"
        );
        assert_eq!(
            layer_blob(&sent, 0),
            Some(live::layer_bytes(saved.w, saved.h, &saved.cells)),
            "the save's layer blob carries the painted cells, not the pre-stroke map"
        );
    }

    /// A `CartBlobBegin` the cart then gives up on: `0x92 CartBlobEnd`
    /// carrying [`CART_BLOB_ABANDONED`], which is the only way the host
    /// learns that bytes it asked for are not coming.
    fn cart_blob_abandoned(endpoint: &ggo_common::LinkEndpoint, kind: u8, index: u32) {
        let mut begin = vec![0x90, kind];
        begin.extend_from_slice(&16u32.to_le_bytes());
        begin.extend_from_slice(&index.to_le_bytes());
        cart_says(endpoint, begin);
        let mut end = vec![0x92];
        end.extend_from_slice(&emerald_editor_runtime::wire::CART_BLOB_ABANDONED.to_le_bytes());
        cart_says(endpoint, end);
    }

    /// The cart owns the live world, so a save in Live writes what the
    /// CART holds: an entity one of its own systems moved -- with no row
    /// report the mirror could have folded -- lands in the TOML at the
    /// position the snapshot carried, components and all.
    #[gpui::test]
    async fn a_save_in_live_writes_the_carts_entities(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_flat(cx).await;
        // Something to save, and a position the snapshot will overrule.
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [8.0, 8.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        show_panel(cx);
        assert!(
            cx.debug_bounds("ggo-world-saving").is_none(),
            "nothing is in flight yet"
        );
        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                open_of(panel).save_pending(),
                "the save waits on the cart's world"
            );
            assert!(
                panel.dirty_world_name().is_some(),
                "and nothing is written until it lands"
            );
        });
        assert!(
            cx.debug_bounds("ggo-world-saving").is_some(),
            "the status row says the save is in flight"
        );
        answer_cart_save(
            &panel,
            &endpoint,
            cx,
            &[(0, vec![transform_bag([40.0, 12.0], 3)])],
            &[],
        );

        panel.read_with(cx, |panel, _| {
            assert!(!open_of(panel).save_pending(), "the save settled");
            assert_eq!(open_of(panel).save_error, None);
            assert!(panel.dirty_world_name().is_none(), "and cleared dirty");
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("ggo-world-saving").is_none(),
            "and the row goes with it"
        );
        let on_disk = world_file::read_world(dir.path(), "worlds/flat.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([40, 12]),
            "the TOML has the position the cart reported, not the document's"
        );
        assert_eq!(
            on_disk.entities[0].components["Transform"]["z"],
            json!(3),
            "and every other field of the snapshot's bag"
        );
    }

    /// A snapshot index the document's flattening does not name is an
    /// entity one of the cart's own systems spawned: it is APPENDED rather
    /// than dropped, so a save keeps what the cart added.
    #[gpui::test]
    async fn a_save_appends_an_entity_the_cart_added(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_flat(cx).await;
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [8.0, 8.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();
        answer_cart_save(
            &panel,
            &endpoint,
            cx,
            &[
                (0, vec![transform_bag([8.0, 8.0], 0)]),
                (1, vec![transform_bag([64.0, 32.0], 1)]),
            ],
            &[],
        );

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).store.state().entities.len(),
                2,
                "the document gained the cart's entity"
            );
        });
        let on_disk = world_file::read_world(dir.path(), "worlds/flat.toml").unwrap();
        assert_eq!(on_disk.entities.len(), 2, "and so did the file");
        assert_eq!(
            on_disk.entities[1].components["Transform"]["pos"],
            json!([64, 32])
        );
    }

    /// A slot's readback is written into the `.map` behind it, so cells a
    /// user edit system painted survive the save. A slot the cart answers
    /// `0 x 0` for has no map at all, and its file is left alone.
    #[gpui::test]
    async fn a_save_writes_the_carts_layer_cells(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_with_background(cx).await;
        let before = io::open_map(dir.path(), "maps/test.bg0.map").expect("the linked map");
        assert_eq!(before.cells.first().copied(), Some(live::BLANK_TILE));

        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();
        // 2 x 2 of the cart's shadow over a 16 x 16 document map: the
        // dimensions it reports are what it clamped the map to, so the
        // cells land in the top-left and the rest of the file stands.
        answer_cart_save(
            &panel,
            &endpoint,
            cx,
            &[(0, vec![transform_bag([4.0, 4.0], 0)])],
            &[(0, 2, 2, vec![7, live::BLANK_TILE, live::BLANK_TILE, 9])],
        );

        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).save_error, None);
        });
        let saved = io::open_map(dir.path(), "maps/test.bg0.map").expect("the map saved");
        assert_eq!(
            saved.w, before.w,
            "the cart's shadow does not resize the map"
        );
        assert_eq!(saved.cells.first().copied(), Some(7), "the painted cell");
        assert_eq!(
            saved.cells.get(usize::from(saved.w) + 1).copied(),
            Some(9),
            "placed by the readback's own row stride, not the file's"
        );
    }

    /// A cart that gives up on a blob the save asked for fails the save:
    /// the file is left exactly as it was, the document stays dirty, and
    /// the status row says why. A half-written world is worse than none.
    #[gpui::test]
    async fn a_failed_cart_blob_fails_the_save_and_keeps_the_file(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_flat(cx).await;
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [8.0, 8.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();
        cart_blob_abandoned(&endpoint, 0, 0);
        cart_frames(&panel, &endpoint, cx, 1);

        panel.read_with(cx, |panel, _| {
            assert!(!open_of(panel).save_pending(), "the save is over");
            assert_eq!(
                open_of(panel).save_error.as_deref(),
                Some("the cart could not send its world")
            );
            assert!(
                panel.dirty_world_name().is_some(),
                "a failed save keeps the document dirty"
            );
        });
        let on_disk = world_file::read_world(dir.path(), "worlds/flat.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([4, 4]),
            "the file is untouched"
        );
    }

    /// A save that the cart never answers gives up after
    /// [`live::SAVE_DEADLINE_FRAMES`] of ITS clock, with the same
    /// file-untouched discipline as a failed transfer.
    #[gpui::test]
    async fn a_save_the_cart_never_answers_times_out(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_flat(cx).await;
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [8.0, 8.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();
        cart_frames(&panel, &endpoint, cx, 301);

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).save_error.as_deref(),
                Some("the cart did not answer in time")
            );
            assert!(panel.dirty_world_name().is_some());
        });
        let on_disk = world_file::read_world(dir.path(), "worlds/flat.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([4, 4])
        );
    }

    /// Play is the GAME's world: its entities are where the play-through
    /// put them, so a save is refused outright rather than persisting a
    /// run. Nothing is asked of the cart and nothing is written.
    #[gpui::test]
    async fn a_save_in_play_is_refused(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_flat(cx).await;
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [8.0, 8.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Play, cx));
        cx.run_until_parked();
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).save_error.as_deref(),
                Some("stop the game to save")
            );
            assert!(!open_of(panel).save_pending(), "nothing was asked for");
            assert!(panel.dirty_world_name().is_some(), "and nothing written");
        });
        assert!(
            host_sent(&endpoint).is_empty(),
            "no snapshot or readback goes out"
        );
        assert!(
            !dir.path().join("worlds/flat.toml").exists()
                || world_file::read_world(dir.path(), "worlds/flat.toml")
                    .unwrap()
                    .entities[0]
                    .components["Transform"]["pos"]
                    == json!([4, 4]),
            "the file is untouched"
        );
    }

    /// The world tab's `Item::save` -- what the pane's close prompt
    /// awaits -- settles on the cart's answer, not ahead of it: an `Ok`
    /// returned before the write would let "Save" discard a document
    /// nothing had written.
    #[gpui::test]
    async fn the_tabs_save_waits_for_the_carts_world(cx: &mut TestAppContext) {
        use workspace::item::{Item, SaveOptions};

        let (panel, endpoint, dir, cx) = connected_live_panel_flat(cx).await;
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let item = cx.update(|_, cx| cx.new(|cx| WorldCanvasItem::new(panel.clone(), cx)));
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [8.0, 8.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        let save = cx.update(|window, cx| {
            item.update(cx, |item, cx| {
                item.save(SaveOptions::default(), project, window, cx)
            })
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(open_of(panel).save_pending(), "still waiting on the cart");
        });
        answer_cart_save(
            &panel,
            &endpoint,
            cx,
            &[(0, vec![transform_bag([40.0, 12.0], 0)])],
            &[],
        );
        save.await.expect("the save landed");

        let on_disk = world_file::read_world(dir.path(), "worlds/flat.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([40, 12]),
            "and wrote the cart's world"
        );
        item.read_with(cx, |item, cx| {
            assert!(!item.is_dirty(cx), "the tab is clean again");
        });
    }

    /// Design saves the document, as it always did: no snapshot, no
    /// readback, and the write lands on the spot.
    #[gpui::test]
    async fn a_save_in_design_writes_the_document_without_the_cart(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel_flat(cx).await;
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [8.0, 8.0],
                    gesture: None,
                },
                cx,
            );
        });
        settle_live(&panel, &endpoint, cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Design, window, cx);
        });
        cx.run_until_parked();
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.save_impl(cx));
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).save_error, None);
            assert!(panel.dirty_world_name().is_none(), "written on the spot");
        });
        let on_disk = world_file::read_world(dir.path(), "worlds/flat.toml").unwrap();
        assert_eq!(
            on_disk.entities[0].components["Transform"]["pos"],
            json!([8, 8]),
            "the DOCUMENT's position, which is the only truth in Design"
        );
        assert!(
            !host_sent(&endpoint)
                .iter()
                .any(|message| message.first() == Some(&0x12)),
            "no snapshot is asked for"
        );
    }

    /// The slot cycle runs one layer per tick, and dirtiness is PER SLOT:
    /// a slot re-dirtied while the queue is draining is re-read from disk
    /// and moved to the back, while the slots ahead of it keep their
    /// place. A single flag rebuilt the whole queue instead, so a
    /// continuous stroke -- which re-dirties every tick -- never got past
    /// the queue's front and only ever shipped slot 0.
    #[gpui::test]
    async fn a_re_dirtied_slot_moves_to_the_back_without_restarting_the_cycle(
        cx: &mut TestAppContext,
    ) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).layer_queue.is_empty(), "settled");
        });
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.refresh_backgrounds(cx));
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let live = live_of(panel);
            assert_eq!(live.layer_queue.len(), 3, "one slot went out, three wait");
            assert!(
                !live.layers_dirty.any(),
                "the flags are spent on the queue they built"
            );
        });

        panel.update(cx, |panel, _| {
            live_mut_of(panel).layers_dirty.mark(2);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).layer_queue.len(),
                3,
                "the queue is only rebuilt when a slot can actually go out"
            );
        });

        let sent = settle_live(&panel, &endpoint, cx);
        let layers = sent_layers(&sent);
        assert_eq!(
            layers,
            [0, 1, 3, 2],
            "only slot 2 was re-read; 1 and 3 kept their place and 0 was not re-sent"
        );
    }

    /// The stroke case the per-slot flags exist for: a slot that is not 0
    /// has to reach the cart while the stroke is still in flight, which a
    /// whole-queue rebuild per tick never let happen.
    #[gpui::test]
    async fn painting_slot_two_mid_stroke_ships_slot_two(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        write_test_tileset(dir.path(), "tiles/bg.til");
        panel.update(cx, |panel, cx| {
            panel.add_background_impl(2, "tiles/bg.til".into(), cx)
        });
        cx.run_until_parked();
        settle_live(&panel, &endpoint, cx);
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| {
            panel.enter_paint_mode(PaintTarget::BgSlot(2), cx);
        });
        cx.run_until_parked();
        // Pressed and NOT released: the stroke is still in flight.
        panel.update_in(cx, |panel, _, cx| {
            panel.canvas_primary_down_with(live_screen_of(panel, [1.0, 1.0]), false, cx);
        });
        endpoint.tick();
        cx.run_until_parked();

        let layers = sent_layers(&host_sent(&endpoint));
        assert_eq!(
            layers,
            [2],
            "the painted slot goes out, and the three clean ones stay off the wire"
        );
    }

    /// The status row is cleared by the update that works, not only set by
    /// the one that fails.
    #[gpui::test]
    async fn a_successful_update_clears_a_stale_live_error(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.live_error = Some("live update: something was wrong".to_string());
            if let Some(live) = &mut open.live {
                live.pending_camera = Some([0.0, 0.0]);
            }
            cx.notify();
        });
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(open_of(panel).live_error, None);
        });
    }

    /// The overlay is drawn from rows, and rows outlive the world they
    /// were published for: until the cart has FRAMED past the blob's ack,
    /// the canvas shows the frame alone.
    #[gpui::test]
    async fn the_overlay_waits_for_the_cart_to_republish_after_a_world_load(
        cx: &mut TestAppContext,
    ) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_rows(&endpoint, &[(0, 4.0, 4.0), (1, 40.0, 8.0)]);
        cx.run_until_parked();
        let published = panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).loaded());
            assert_eq!(overlay_of(panel).len(), 2);
            live_of(panel).rows.clone()
        });

        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                !live_of(panel).loaded(),
                "the blob went out; the rows are the old world's"
            );
            assert!(
                overlay_of(panel).is_empty(),
                "nothing is outlined over a frame that may not hold it"
            );
        });

        // Every chunk acked, and NOT one frame published: the bytes have
        // landed but the cart has not drawn since, so the rows are still
        // unproven.
        for _ in 0..64 {
            if !ack_blobs(&endpoint).1 {
                break;
            }
            endpoint.tick();
            cx.run_until_parked();
        }
        let frame = panel.read_with(cx, |panel, _| {
            let live = live_of(panel);
            assert!(!live.mailbox.busy(), "the transfer finished");
            assert!(
                !live.loaded(),
                "an ack is not a republish -- the cart has not framed since"
            );
            live.mailbox.frame_seq()
        });

        // One frame, and the cart republishes a BYTE-IDENTICAL table (the
        // new entity has no Transform of its own to report). Nothing about
        // the ROWS says the reload finished; the frame after the ack does.
        cart_frame(&endpoint, frame + 1);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).rows,
                published,
                "the table really is unchanged"
            );
            assert!(live_of(panel).loaded(), "and the overlay comes back anyway");
            assert_eq!(overlay_of(panel).len(), 2);
        });
    }

    /// A world the encoder refuses is a document problem, not a link one:
    /// the flag has to survive so the next tick retries, and the overlay
    /// must not be blanked for a blob that never left.
    #[gpui::test]
    async fn a_failed_encode_keeps_the_world_dirty_and_retries(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        // The open document instances `worlds/sub`; the encoder reads it
        // to flatten the world, so removing it makes every encode fail.
        let sub = dir.path().join("worlds/sub.toml");
        let saved = std::fs::read(&sub).unwrap();
        std::fs::remove_file(&sub).unwrap();

        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();

        let sent = host_sent(&endpoint);
        assert!(
            sent.iter().all(|message| message.first() != Some(&0x08)),
            "nothing went out: {sent:?}"
        );
        panel.read_with(cx, |panel, _| {
            assert!(
                live_of(panel).world_dirty,
                "the failed send re-arms itself instead of being swallowed"
            );
            assert!(
                live_of(panel).loaded(),
                "and a blob that never left must not blank the overlay"
            );
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("live update"),
                "{:?}",
                open_of(panel).live_error
            );
        });

        std::fs::write(&sub, saved).unwrap();
        // The retry is armed by the document moving, not by the tick --
        // see `a_failed_encode_is_not_re_attempted_every_tick`.
        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        assert!(
            host_sent(&endpoint)
                .iter()
                .any(|message| message.first() == Some(&0x08) && message.get(1) == Some(&0)),
            "the retry sends the world once the document reads again"
        );
        panel.read_with(cx, |panel, _| {
            assert!(!live_of(panel).world_dirty);
            assert_eq!(open_of(panel).live_error, None, "and the row clears");
        });
    }

    /// Each attempt re-reads every instanced world file on the UI thread,
    /// and the poll wakes on every cart frame -- so a document the encoder
    /// keeps refusing must not be re-encoded per tick. An edit is what
    /// re-arms it, immediately.
    #[gpui::test]
    async fn a_failed_encode_is_not_re_attempted_every_tick(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        // The open document instances `worlds/sub`; the encoder reads it
        // to flatten the world, so removing it makes every encode fail.
        std::fs::remove_file(dir.path().join("worlds/sub.toml")).unwrap();

        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("live update"),
                "the first attempt ran and failed"
            );
        });

        // A push that does not go through the encoder still runs -- and
        // must not take the unsent world's failure off the row.
        panel.update(cx, |panel, _| {
            live_mut_of(panel).pending_camera = Some([0.0, 0.0]);
        });
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("live update"),
                "the camera push does not clear a world that is still owed: {:?}",
                open_of(panel).live_error
            );
        });

        // Clearing the row is what makes a further attempt visible: one
        // that ran would fail the same way and put the message straight
        // back.
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.live_error = None;
        });
        for _ in 0..3 {
            endpoint.tick();
            cx.run_until_parked();
        }
        let sent = host_sent(&endpoint);
        assert!(
            sent.iter().all(|message| message.first() != Some(&0x08)),
            "nothing went out: {sent:?}"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).live_error,
                None,
                "three ticks, and the encode was not attempted again"
            );
            assert!(live_of(panel).world_dirty, "the re-send is still owed");
        });

        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("live update"),
                "an edit retries on the very next tick"
            );
        });
    }

    /// The backoff's other end: a failure the DOCUMENT cannot fix (an
    /// instanced world file being rewritten under the panel) still has to
    /// be retried, so the same generation is tried again once
    /// [`live::ENCODE_RETRY`] of cart time has passed.
    #[gpui::test]
    async fn a_failed_encode_retries_the_same_document_once_the_backoff_expires(
        cx: &mut TestAppContext,
    ) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        std::fs::remove_file(dir.path().join("worlds/sub.toml")).unwrap();

        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        endpoint.tick();
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            let live = live_mut_of(panel);
            let (generation, retry_at) = live.world_retry_at.expect("the failure was recorded");
            assert!(retry_at > live.cart_now(), "and it is not due yet");
            // The cart clock only moves when the emulator publishes a
            // frame, which a headless test never does -- so the deadline
            // is brought to it.
            live.world_retry_at = Some((generation, live.cart_now()));
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.live_error = None;
        });
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("live update"),
                "the expired backoff re-attempted the unchanged document: {:?}",
                open_of(panel).live_error
            );
        });
    }

    /// A greeting resets the cart's whole view, so the world handshake has
    /// to start over with it. Otherwise an `Acked(N)` recorded before the
    /// greeting flips to `Loaded` on the post-reset frame counter -- and
    /// the overlay would be drawn from the OLD session's rows over a cart
    /// that has not been given the world back.
    #[gpui::test]
    async fn a_greeting_re_arms_the_world_handshake(cx: &mut TestAppContext) {
        let (panel, endpoint, dir, cx) = connected_live_panel(cx).await;
        host_sent(&endpoint);
        // Every re-send will now fail to encode, so nothing can quietly
        // repair the handshake behind the assertion.
        std::fs::remove_file(dir.path().join("worlds/sub.toml")).unwrap();

        // What a stale session leaves behind: the panel re-greeted (which
        // resets the mailbox's mirror, frame counter included) and is
        // waiting for the cart to answer.
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            let live = open.live.as_mut().expect("a live session");
            live.status = LiveStatus::Connecting;
            live.world_sync = live::WorldSync::Acked(0);
            live.mailbox.hello().expect("the link accepts a greeting");
        });
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let live = live_of(panel);
            assert_eq!(live.status, LiveStatus::Connected);
            assert!(live.world_dirty, "the greeting re-armed the world");
            assert!(!live.loaded());
        });

        // The cart frames on. Those frames say nothing about a blob it was
        // never given.
        cart_frame(&endpoint, 1);
        cart_frame(&endpoint, 2);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                !live_of(panel).loaded(),
                "frames after the GREETING are not frames after an ack"
            );
            assert!(live_of(panel).world_dirty, "and the re-send is still owed");
        });
    }

    /// The camera the cart is pointed at when the session opens is the
    /// DOCUMENT's own framing -- the camera origin the Design renderer
    /// frames -- not wherever the design pan happens to sit, and not
    /// (0, 0).
    #[gpui::test]
    async fn the_greeting_hands_the_document_framing_to_the_cart(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cx.run_until_parked();
        let origin = panel.read_with(cx, |panel, _| {
            active_camera_origin(&open_of(panel).store.state())
        });
        assert_ne!(origin, [0.0, 0.0], "the fixture is framed somewhere");

        let sent = settle_live(&panel, &endpoint, cx);
        let camera = sent
            .iter()
            .find(|message| message.first() == Some(&0x03))
            .expect("the greeting reached the cart");
        match emerald_editor_runtime::wire::decode_host(camera) {
            Some(emerald_editor_runtime::wire::HostMsg::Camera { x, y }) => {
                assert_eq!(
                    (live::from_raw(x), live::from_raw(y)),
                    (origin[0], origin[1])
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // ---------------------------------- mode switch, edit rail, status

    /// The `-on`/`-off` halves of each stateful `debug_selector` this
    /// module asserts on. Named because `debug_bounds` takes `&'static
    /// str`, so the halves cannot be built from a prefix at the call site.
    const MODE_DESIGN: (&str, &str) = ("ggo-world-mode-design-on", "ggo-world-mode-design-off");
    const MODE_LIVE: (&str, &str) = ("ggo-world-mode-live-on", "ggo-world-mode-live-off");
    /// The Live boot screen's `debug_selector`.
    const LIVE_LOADING: &str = "ggo-world-live-loading";
    /// The cart's own `Edit | Play` switch on the edit rail -- not the
    /// `Design | Live` one above it, which picks the RENDERER.
    const MODE_EDIT: (&str, &str) = ("ggo-world-mode-edit-on", "ggo-world-mode-edit-off");
    const MODE_PLAY: (&str, &str) = ("ggo-world-mode-play-on", "ggo-world-mode-play-off");
    const TOOL_0: (&str, &str) = ("ggo-world-tool-0-on", "ggo-world-tool-0-off");
    const TOOL_1: (&str, &str) = ("ggo-world-tool-1-on", "ggo-world-tool-1-off");
    const TOOL_2: (&str, &str) = ("ggo-world-tool-2-on", "ggo-world-tool-2-off");
    /// The marker the inspector renders in Play, where the cart owns the
    /// world and the document is not the user's to edit.
    const INSPECTOR_DISABLED: &str = "ggo-world-inspector-disabled";

    /// The state a stateful `debug_selector` pair is rendering, or `None`
    /// when neither half is on screen.
    fn toggle_of(
        cx: &mut gpui::VisualTestContext,
        (on, off): (&'static str, &'static str),
    ) -> Option<bool> {
        match (cx.debug_bounds(on), cx.debug_bounds(off)) {
            (Some(_), None) => Some(true),
            (None, Some(_)) => Some(false),
            _ => None,
        }
    }

    fn assert_mode(
        panel: &Entity<WorldPanel>,
        cx: &mut gpui::VisualTestContext,
        expected: CanvasMode,
    ) {
        panel.read_with(cx, |panel, _| assert_eq!(panel.canvas_mode, expected));
    }

    /// Focuses the dock and lets it paint the panel, so `debug_bounds`
    /// can resolve the panel's elements.
    fn show_panel(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| window.dispatch_action(ToggleFocus.boxed_clone(), cx));
        cx.run_until_parked();
    }

    /// The one host message of `kind` the session has queued, or a panic
    /// naming what was on the wire instead. Returned undecoded: a
    /// `HostMsg` borrows the bytes it was decoded from.
    fn sent_host_msg(endpoint: &ggo_common::LinkEndpoint, kind: u8) -> Vec<u8> {
        let sent = host_sent(endpoint);
        sent.iter()
            .find(|message| message.first() == Some(&kind))
            .cloned()
            .unwrap_or_else(|| panic!("no host message of kind {kind:#04x} on the wire: {sent:?}"))
    }

    /// Give the panel a selection, so the inspector has something to draw.
    fn select_first_entity(panel: &Entity<WorldPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.selected = vec![Selection::Entity(0)];
            cx.notify();
        });
        cx.run_until_parked();
    }

    /// The rail's `Edit | Play` switch is the CART's mode: the click goes
    /// out as `SetMode`, and Play greys the inspector -- the entities are
    /// the game's while it runs, and a field edit would be written over.
    #[gpui::test]
    async fn clicking_play_sends_the_mode_and_greys_the_inspector(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) =
            connected_live_panel_with_tools(cx, &["Select", "paint"]).await;
        select_first_entity(&panel, cx);
        show_panel(cx);
        assert_eq!(
            toggle_of(cx, MODE_EDIT),
            Some(true),
            "a session comes up editing"
        );
        assert_eq!(toggle_of(cx, MODE_PLAY), Some(false));
        assert!(
            cx.debug_bounds(INSPECTOR_DISABLED).is_none(),
            "and the inspector takes edits in Edit"
        );
        host_sent(&endpoint);

        let play = cx.debug_bounds(MODE_PLAY.1).expect("the Play half");
        cx.simulate_click(play.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        let sent = sent_host_msg(&endpoint, 0x0D);
        match emerald_editor_runtime::wire::decode_host(&sent) {
            Some(emerald_editor_runtime::wire::HostMsg::SetMode { mode }) => assert_eq!(
                mode,
                emerald_editor_runtime::wire::mode::PLAY,
                "the cart is told to run its game table"
            ),
            other => panic!("{other:?}"),
        }
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).mode, EditorMode::Play)
        });
        assert_eq!(toggle_of(cx, MODE_PLAY), Some(true));
        assert_eq!(
            toggle_of(cx, MODE_EDIT),
            Some(false),
            "and both halves move together"
        );
        assert!(
            cx.debug_bounds(INSPECTOR_DISABLED).is_some(),
            "Play greys the inspector"
        );
        panel.read_with(cx, |panel, cx| {
            let open = open_of(panel);
            assert!(
                !open.inspector.is_empty(),
                "the fields are still listed, just not editable"
            );
            assert!(
                open.inspector
                    .iter()
                    .all(|entry| entry.editor.read(cx).read_only(cx)),
                "and every field editor refuses typing"
            );
        });

        host_sent(&endpoint);
        let edit = cx.debug_bounds(MODE_EDIT.1).expect("the Edit half");
        cx.simulate_click(edit.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        let sent = sent_host_msg(&endpoint, 0x0D);
        match emerald_editor_runtime::wire::decode_host(&sent) {
            Some(emerald_editor_runtime::wire::HostMsg::SetMode { mode }) => {
                assert_eq!(mode, emerald_editor_runtime::wire::mode::EDIT)
            }
            other => panic!("{other:?}"),
        }
        assert!(
            cx.debug_bounds(INSPECTOR_DISABLED).is_none(),
            "and Edit hands the inspector back"
        );
    }

    /// Play is the CART's: every affordance that would mutate the
    /// document is greyed out, and the actions behind them refuse too --
    /// a keybinding is not stopped by a disabled button. The one edit that
    /// still works is Delete, which is forwarded to the cart.
    #[gpui::test]
    async fn play_blocks_every_document_mutation(cx: &mut TestAppContext) {
        const MUTATORS: [(&str, &str); 6] = [
            ("ggo-world-undo-on", "ggo-world-undo-off"),
            ("ggo-world-redo-on", "ggo-world-redo-off"),
            ("ggo-world-add-entity-on", "ggo-world-add-entity-off"),
            // The layers rail is a document edit per slot, so it is greyed
            // out too: the linked slot's paint entry and its unlink, and an
            // empty slot's "Add…".
            ("ggo-world-bg-paint-0-on", "ggo-world-bg-paint-0-off"),
            ("ggo-world-bg-clear-0-on", "ggo-world-bg-clear-0-off"),
            ("ggo-world-bg-slot-1-on", "ggo-world-bg-slot-1-off"),
        ];

        let (panel, endpoint, dir, cx) =
            connected_live_panel_with_tools(cx, &["Select", "paint"]).await;
        // Slot 0 linked while the session is still in Edit, so Play has a
        // background to refuse to unlink.
        write_test_tileset(dir.path(), "tiles/bg.til");
        panel.update(cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx)
        });
        cx.run_until_parked();
        settle_live(&panel, &endpoint, cx);
        select_first_entity(&panel, cx);
        show_panel(cx);
        for pair in MUTATORS {
            assert_eq!(
                toggle_of(cx, pair),
                Some(true),
                "{} is live in Edit",
                pair.0
            );
        }
        assert_eq!(
            toggle_of(cx, TOOL_1),
            Some(false),
            "and the cart's tools are pickable"
        );

        // Something to undo.
        panel.update(cx, |panel, cx| {
            panel.apply_op(
                WorldOp::MoveEntity {
                    entity: 0,
                    pos: [12.0, 12.0],
                    gesture: None,
                },
                cx,
            )
        });
        settle_live(&panel, &endpoint, cx);
        let moved = panel.read_with(cx, |panel, _| entity_pos_of(panel, 0));
        assert_eq!(moved, [12.0, 12.0]);

        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Play, cx));
        cx.run_until_parked();
        show_panel(cx);
        host_sent(&endpoint);
        let generation = panel.read_with(cx, |panel, _| open_of(panel).doc_generation);

        for pair in MUTATORS {
            assert_eq!(
                toggle_of(cx, pair),
                Some(false),
                "{} is greyed out while the game runs",
                pair.0
            );
        }
        // The tool radio is greyed too -- the tools ARE the edit table --
        // but its wrapper's suffix carries which tool is PICKED, so what
        // proves the button dead is that clicking it picks nothing.
        let paint = cx.debug_bounds(TOOL_1.1).expect("the second tool");
        cx.simulate_click(paint.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).tool, 0, "no tool is pickable in Play")
        });

        cx.update(|window, cx| window.dispatch_action(Undo.boxed_clone(), cx));
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.add_entity_impl(cx));
        panel.update(cx, |panel, cx| {
            panel.add_instance_impl("worlds/sub".to_string(), cx)
        });
        // The layers rail and the clipboard reach the store through
        // `apply_op` like everything else, and neither may move a document
        // the running cart has left behind.
        panel.update(cx, |panel, cx| panel.clear_background_impl(0, cx));
        panel.update(cx, |panel, cx| {
            panel.add_background_impl(1, "tiles/bg.til".into(), cx)
        });
        panel.update(cx, |panel, cx| {
            panel.copy_impl(cx);
            panel.paste_impl(cx);
        });
        assert!(
            !panel.update(cx, |panel, cx| panel
                .enter_paint_mode(PaintTarget::BgSlot(0), cx)),
            "and the brush does not come out over a play-through"
        );
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let backgrounds = open_of(panel).store.state().backgrounds;
            assert_eq!(
                backgrounds.iter().map(|bg| bg.layer).collect::<Vec<_>>(),
                vec![0],
                "slot 0 is still linked and slot 1 was never added"
            );
            assert_eq!(entity_pos_of(panel, 0), moved, "the undo did nothing");
            assert_eq!(
                open_of(panel).doc_generation,
                generation,
                "and nothing else moved the document either"
            );
            assert!(!live_of(panel).world_dirty, "so the cart is owed nothing");
        });
        let (transforms, blobbed) = sent_transforms(&settle_live(&panel, &endpoint, cx));
        assert!(!blobbed, "no world went out to the playing cart");
        assert!(transforms.is_empty(), "and no transforms: {transforms:?}");
    }

    /// A replayed transform that comes back somewhere ELSE is the cart
    /// having adjusted it, not the host's undo landing: it folds like any
    /// other motion, and the entry is retired either way -- a held one
    /// would suppress an honest fold at that exact position forever.
    #[gpui::test]
    async fn a_replayed_row_that_lands_elsewhere_folds_and_retires(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cart_fixture_rows(&endpoint);
        cx.run_until_parked();

        panel.update(cx, |panel, _| {
            live_mut_of(panel).queue_edits(vec![live::CartEdit::Transform {
                index: 0,
                x: live::to_raw(40.0),
                y: live::to_raw(50.0),
            }]);
        });
        settle_live(&panel, &endpoint, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).replayed_rows.len(),
                1,
                "the transform went out and is owed an answer"
            );
        });

        // One raw unit off what was asked for.
        let landed = 50.0 + live::from_raw(1);
        cart_rows(
            &endpoint,
            &[
                (0, 40.0, landed),
                (1, 40.0, 8.0),
                (2, 0.0, 0.0),
                (3, 42.0, 26.0),
            ],
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                entity_pos_of(panel, 0),
                [40.0, landed],
                "the cart's own adjustment folds like any other motion"
            );
            assert!(
                live_of(panel).replayed_rows.is_empty(),
                "and the entry is retired all the same"
            );
        });
    }

    /// A click on the canvas blurs whatever inspector field had focus, and
    /// the blur commits. In Play that must build no op at all; in Edit an
    /// unchanged field is an op the store folds away, and neither may put
    /// the world back on the wire.
    #[gpui::test]
    async fn an_unchanged_blur_never_re_sends_the_world(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        select_first_entity(&panel, cx);
        show_panel(cx);
        let editor = field_editor(&panel, cx, "Transform", "z");
        let generation = panel.read_with(cx, |panel, _| open_of(panel).doc_generation);

        panel.update(cx, |panel, cx| panel.commit_editor(editor.entity_id(), cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).doc_generation,
                generation,
                "a commit of unchanged text is not a document change"
            );
            assert!(!live_of(panel).world_dirty, "so the cart is owed nothing");
        });

        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Play, cx));
        cx.run_until_parked();
        host_sent(&endpoint);
        panel.update_in(cx, |panel, window, cx| {
            editor.update(cx, |editor, cx| editor.set_text("9", window, cx));
            panel.commit_editor(editor.entity_id(), cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                open_of(panel).doc_generation,
                generation,
                "a blur in Play commits nothing, whatever the field says"
            );
            assert!(!live_of(panel).world_dirty);
        });
        let (_, blobbed) = sent_transforms(&settle_live(&panel, &endpoint, cx));
        assert!(!blobbed, "and no world blob reset the play-through");
    }

    /// A mode push that failed left the rail showing a mode the cart is
    /// not in, so pressing the same half again has to try once more --
    /// the state is optimistic, and treating the retry as a no-op is what
    /// would strand the session.
    #[gpui::test]
    async fn a_failed_mode_push_is_retried_by_the_same_pick(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Play, cx));
        host_sent(&endpoint);

        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Play, cx));
        assert!(
            host_sent(&endpoint)
                .iter()
                .all(|message| message.first() != Some(&0x0D)),
            "the mode the cart already took is not re-sent"
        );

        panel.update(cx, |panel, _| {
            live_mut_of(panel).mode_push_failed = true;
        });
        panel.update(cx, |panel, cx| panel.set_live_mode(EditorMode::Play, cx));
        assert_eq!(
            sent_host_msg(&endpoint, 0x0D).first(),
            Some(&0x0D),
            "but a pick whose push never left goes out again"
        );
        panel.read_with(cx, |panel, _| {
            assert!(
                !live_of(panel).mode_push_failed,
                "and the retry that worked clears the mark"
            );
        });
    }

    /// The tool radio is the cart's greeting: one entry per name it sent,
    /// tool 0 (its built-in select) active until the user picks another,
    /// and the pick goes out as `SetTool`.
    #[gpui::test]
    async fn the_tool_radio_lists_the_carts_tools_and_sends_the_pick(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) =
            connected_live_panel_with_tools(cx, &["Select", "paint"]).await;
        show_panel(cx);
        assert_eq!(
            toggle_of(cx, TOOL_0),
            Some(true),
            "select until told otherwise"
        );
        assert_eq!(toggle_of(cx, TOOL_1), Some(false));
        assert_eq!(toggle_of(cx, TOOL_2), None, "only the tools the cart named");
        host_sent(&endpoint);

        let paint = cx.debug_bounds(TOOL_1.1).expect("the second tool");
        cx.simulate_click(paint.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        let sent = sent_host_msg(&endpoint, 0x0E);
        match emerald_editor_runtime::wire::decode_host(&sent) {
            Some(emerald_editor_runtime::wire::HostMsg::SetTool { tool }) => assert_eq!(tool, 1),
            other => panic!("{other:?}"),
        }
        panel.read_with(cx, |panel, _| assert_eq!(live_of(panel).tool, 1));
        assert_eq!(toggle_of(cx, TOOL_1), Some(true));
        assert_eq!(
            toggle_of(cx, TOOL_0),
            Some(false),
            "a radio, not a checkbox"
        );
    }

    /// The switch is a real mode change on both sides: Design ends the
    /// session and the viewer run behind it, and Live starts a new one
    /// rather than reviving the corpse. The DOCUMENT is untouched either
    /// way -- the mode only decides which renderer draws it.
    #[gpui::test]
    async fn switching_to_design_keeps_the_document_and_back_to_live_starts_a_session(
        cx: &mut TestAppContext,
    ) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        let entities = panel.read_with(cx, |panel, _| open_of(panel).store.state().entities.len());

        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Design, window, cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(
                open_of(panel).live.is_none(),
                "the session goes with the mode"
            );
            assert_eq!(
                open_of(panel).store.state().entities.len(),
                entities,
                "the document is the same document"
            );
        });
        assert!(
            endpoint.stop_requested(),
            "and the viewer run behind it is stopped"
        );

        let before = BOOTED.with(|booted| booted.borrow().len());
        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Live, window, cx)
        });
        cx.run_until_parked();

        assert_eq!(
            BOOTED.with(|booted| booted.borrow().len()),
            before + 1,
            "asks the booter again; the emu panel decides whether to rebuild"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Live);
            let live = open_of(panel).live.as_ref().expect("a new session");
            assert!(
                matches!(live.status, LiveStatus::Building | LiveStatus::Connecting),
                "a fresh session rather than the old one: {:?}",
                live.status
            );
            assert!(
                !Arc::ptr_eq(&live.endpoint, &endpoint),
                "over a fresh endpoint"
            );
        });
    }

    /// The Live tab shows a boot screen instead of the canvas until the
    /// cart is both connected and drawing.
    #[gpui::test]
    async fn live_shows_a_loading_screen_until_connected_with_a_frame(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_loading_text()),
            Some("Building viewer cart…".into())
        );

        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_loading_text()),
            Some("Booting viewer for test…".into())
        );

        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_loading_text()),
            Some("Booting viewer for test…".into()),
            "connected but no frame yet"
        );
        assert!(
            cx.debug_bounds(LIVE_LOADING).is_some(),
            "the tab renders the boot screen in place of the canvas"
        );

        // A published frame: what the emu side does per presented frame.
        let image = ggo_common::to_render_image(&vec![0u8; 320 * 240 * 4], 320, 240)
            .expect("a 320x240 RGBA buffer");
        *endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((1, image));
        endpoint.tick();
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_loading_text()),
            None
        );
        assert!(
            cx.debug_bounds(LIVE_LOADING).is_none(),
            "and gives the canvas back once the cart is drawing"
        );

        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Design, window, cx)
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_loading_text()),
            None,
            "Design never loads"
        );
    }

    /// A rebuild retires the picture the viewer was drawing: the emu panel
    /// hands that atlas tile back, so painting it again would re-insert a
    /// dropped image. The boot screen covers the gap.
    #[gpui::test]
    async fn a_rebuilding_endpoint_drops_the_live_frame(cx: &mut TestAppContext) {
        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        let image = ggo_common::to_render_image(&vec![0u8; 320 * 240 * 4], 320, 240)
            .expect("a 320x240 RGBA buffer");
        *endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((1, image));
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(live_of(panel).frame.is_some());
            assert_eq!(panel.live_loading_text(), None);
        });

        endpoint.set_state(ggo_common::ViewerState::Building);
        endpoint.tick();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                live_of(panel).frame.is_none(),
                "the rebuild retired the image"
            );
            assert_eq!(
                panel.live_loading_text(),
                Some("Building viewer cart…".into())
            );
        });
    }

    /// A watch rebuild swaps the cart under a live session: `Building`,
    /// then `Running` again with a cart that has never been greeted. The
    /// session has to go back to square one and greet the new cart --
    /// otherwise it stays "connected" to a cart that has never heard of
    /// the document, and nothing is ever sent to it again.
    #[gpui::test]
    async fn a_rebuilt_viewer_is_greeted_again_and_re_sent_the_world(cx: &mut TestAppContext) {
        use emerald_editor_runtime::wire::{self, BlobKind, HostMsg};

        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).status, LiveStatus::Connected)
        });
        host_sent(&endpoint);

        endpoint.set_state(ggo_common::ViewerState::Building);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).status,
                LiveStatus::Building,
                "the session is back where it started"
            )
        });

        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        assert!(
            host_sent(&endpoint)
                .iter()
                .any(|message| message.first() == Some(&0x01)),
            "the replacement cart is greeted"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).status,
                LiveStatus::Connecting,
                "and the session waits for its answer"
            )
        });

        cart_says(&endpoint, hello_ack(wire::LINK_PROTO_VERSION, &[]));
        cx.run_until_parked();
        let sent = settle_live(&panel, &endpoint, cx);
        assert!(
            sent.iter().any(|message| matches!(
                wire::decode_host(message),
                Some(HostMsg::BlobBegin {
                    kind: BlobKind::World,
                    ..
                })
            )),
            "and the whole world goes out to it again"
        );
        panel.read_with(cx, |panel, _| {
            let live = live_of(panel);
            assert_eq!(live.status, LiveStatus::Connected);
            assert!(live.loaded(), "the new cart is showing this document");
        });
    }

    /// The build deadline runs on the executor's clock from the moment
    /// the session started, and a rebuild's first `Building` poll can
    /// arrive on a session that has been live far longer than it. The
    /// re-baseline in the `Building` arm is what keeps that poll from
    /// timing the NEW build out before it has begun.
    #[gpui::test]
    async fn a_rebuild_of_a_long_lived_session_survives_the_build_deadline(
        cx: &mut TestAppContext,
    ) {
        use emerald_editor_runtime::wire;

        let (panel, endpoint, _dir, cx) = connected_live_panel(cx).await;
        cx.executor()
            .advance_clock(live::BUILD_DEADLINE + std::time::Duration::from_secs(1));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).status,
                LiveStatus::Connected,
                "an idle live session outlives the build deadline"
            )
        });
        host_sent(&endpoint);

        endpoint.set_state(ggo_common::ViewerState::Building);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Live, "still live");
            assert_eq!(
                open_of(panel).live_error,
                None,
                "the rebuild is not the boot's build timing out"
            );
            assert_eq!(live_of(panel).status, LiveStatus::Building);
        });

        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        assert!(
            host_sent(&endpoint)
                .iter()
                .any(|message| message.first() == Some(&0x01)),
            "and the replacement cart is greeted"
        );
        cart_says(&endpoint, hello_ack(wire::LINK_PROTO_VERSION, &[]));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).status, LiveStatus::Connected)
        });
    }

    /// Entering Live defers the boot, so there is a render with the mode
    /// already flipped and no session behind it. That render must be the
    /// boot screen: painting the design renderer through it flashes the
    /// picture the user just switched away from.
    #[gpui::test]
    async fn entering_live_paints_the_boot_screen_not_a_design_frame(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, _endpoint, cx) = live_panel(cx, &dir).await;
        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Design, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.live_loading_text()),
            None
        );

        // Asserted INSIDE the update, which is the only place the state
        // between `enter_live` arming its deferral and the booter
        // answering can be seen: the deferral runs on the way out.
        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Live, window, cx);
            assert!(open_of(panel).live.is_none(), "the boot is deferred");
            assert_eq!(
                panel.live_loading_text(),
                Some("Booting viewer for test…".into()),
                "the boot screen stands in for the canvas"
            );
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(open_of(panel).live.is_some(), "and the boot lands");
        });
    }

    /// The rail is Live's alone, and only once the cart has greeted: the
    /// mode is the cart's and the tool names arrive on the `HelloAck`.
    #[gpui::test]
    async fn the_edit_rail_is_live_only(cx: &mut TestAppContext) {
        let (panel, _endpoint, _dir, cx) =
            connected_live_panel_with_tools(cx, &["Select", "paint"]).await;
        show_panel(cx);
        assert_eq!(toggle_of(cx, MODE_EDIT), Some(true));
        assert_eq!(toggle_of(cx, TOOL_0), Some(true));

        // The mode alone decides, not the session: a connected cart the
        // design renderer is drawing over has no mode to switch.
        panel.update(cx, |panel, cx| {
            panel.canvas_mode = CanvasMode::Design;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            toggle_of(cx, MODE_EDIT),
            None,
            "Design mode never shows the rail"
        );
        assert_eq!(toggle_of(cx, TOOL_0), None);

        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Live, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            toggle_of(cx, MODE_EDIT),
            Some(true),
            "and back again with the session still up"
        );
    }

    /// The rail is one row: the mode switch, then the tools beside it.
    #[gpui::test]
    async fn the_edit_rail_is_one_row(cx: &mut TestAppContext) {
        let (_panel, _endpoint, _dir, cx) =
            connected_live_panel_with_tools(cx, &["Select", "paint"]).await;
        show_panel(cx);
        let edit = cx.debug_bounds(MODE_EDIT.0).expect("the Edit half");
        let first = cx.debug_bounds(TOOL_0.0).expect("tool 0");
        let second = cx.debug_bounds(TOOL_1.1).expect("tool 1");
        assert_eq!(first.origin.y, second.origin.y, "same row");
        assert!(second.origin.x > first.origin.x, "side by side");
        assert!(first.origin.x > edit.origin.x, "the mode switch leads");
    }

    /// The status line belongs to LIVE mode, not to the session: it
    /// tracks the session while there is one (and says an idle "not
    /// running" when there is not, which
    /// `live_with_no_session_says_so_and_offers_a_start` covers), and
    /// Design mode is silent about a session either way -- a Design panel
    /// is not "connecting" to anything.
    #[gpui::test]
    async fn the_status_line_follows_the_session(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        show_panel(cx);
        assert!(
            cx.debug_bounds("ggo-world-live-status").is_some(),
            "the build is worth saying out loud -- it can take a minute"
        );
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(
            &endpoint,
            hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]),
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(live_of(panel).status, LiveStatus::Connected)
        });
        assert!(cx.debug_bounds("ggo-world-live-status").is_some());

        // Design mode says nothing about a session even while one is up:
        // the toolbar's `live_error` row is Design's channel.
        panel.update(cx, |panel, cx| {
            panel.canvas_mode = CanvasMode::Design;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("ggo-world-live-status").is_none());

        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Design, window, cx)
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("ggo-world-live-status").is_none(),
            "and Design mode stays quiet with the session gone too"
        );
    }

    /// A windowless load (the close prompt's reload, the MCP `world_open`)
    /// reaches Ready without ever entering Live, so the sticky mode is on
    /// Live with nothing behind it. That is a state the user has to be
    /// able to see -- and leave.
    #[gpui::test]
    async fn live_with_no_session_says_so_and_offers_a_start(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Live);
            assert!(open_of(panel).live.is_none(), "nothing was ever started");
        });
        assert!(
            cx.debug_bounds("ggo-world-live-status").is_some(),
            "the idle line is the only sign the canvas is set to Live"
        );
        let start = cx
            .debug_bounds("ggo-world-live-start")
            .expect("a way to start one");

        // This panel has no workspace to boot a cart in, so the click can
        // only get as far as the fallback -- which is proof enough that
        // the button reaches `enter_live`.
        cx.simulate_click(start.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.canvas_mode, CanvasMode::Design);
            assert!(
                open_of(panel)
                    .live_error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("no workspace"),
                "{:?}",
                open_of(panel).live_error
            );
        });
        assert!(
            cx.debug_bounds("ggo-world-live-status").is_none(),
            "and the idle line goes with the mode"
        );
    }

    /// The tool the user picked outlives the session it was picked in:
    /// leaving Live to look at the design view and coming back must not
    /// silently hand the pointer back to the select tool.
    #[gpui::test]
    async fn the_tool_survives_a_trip_through_design(cx: &mut TestAppContext) {
        let (panel, first, _dir, cx) =
            connected_live_panel_with_tools(cx, &["Select", "paint"]).await;
        panel.update(cx, |panel, cx| panel.set_live_tool(1, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.live_tool, 1, "the panel remembers it")
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Design, window, cx)
        });
        panel.update_in(cx, |panel, window, cx| {
            panel.set_canvas_mode(CanvasMode::Live, window, cx)
        });
        cx.run_until_parked();
        let endpoint = BOOTED
            .with(|booted| booted.borrow().last().map(|(_, e)| e.clone()))
            .expect("a second viewer");
        assert!(!Arc::ptr_eq(&endpoint, &first), "a second viewer");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                live_of(panel).tool,
                1,
                "the new session starts where the old one left off"
            )
        });

        // And the CART is told, which is the half that matters: a greeting
        // resets the cart to its select tool, and nothing else re-arms it.
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        host_sent(&endpoint);
        cart_says(
            &endpoint,
            hello_ack(
                emerald_editor_runtime::wire::LINK_PROTO_VERSION,
                &["Select", "paint"],
            ),
        );
        cx.run_until_parked();
        let sent = sent_host_msg(&endpoint, 0x0E);
        match emerald_editor_runtime::wire::decode_host(&sent) {
            Some(emerald_editor_runtime::wire::HostMsg::SetTool { tool }) => assert_eq!(tool, 1),
            other => panic!("the greeting must re-arm the tool: {other:?}"),
        }
    }

    /// A session that failed must not read as "Live": the status line
    /// names the reason and offers the way back.
    #[gpui::test]
    async fn a_failed_session_offers_a_retry_on_the_status_line(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        show_panel(cx);
        endpoint.set_state(ggo_common::ViewerState::Stopped("gone".into()));
        cx.run_until_parked();
        // The fallback already flipped the switch to Design; the retry
        // affordance is for the user who asks for Live again anyway.
        panel.update(cx, |panel, cx| {
            panel.canvas_mode = CanvasMode::Live;
            cx.notify();
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel.live_active(),
                "a failed session is not a live canvas"
            )
        });

        let retry = cx
            .debug_bounds("ggo-world-live-retry")
            .expect("a failed session offers a way back");
        let before = BOOTED.with(|booted| booted.borrow().len());
        cx.simulate_click(retry.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            BOOTED.with(|booted| booted.borrow().len()),
            before + 1,
            "Retry starts a new session"
        );
    }

    /// The keymap entry is part of the feature: an action nothing binds
    /// is a command-palette entry, not a shortcut.
    #[gpui::test]
    async fn the_canvas_mode_keystroke_flips_the_switch(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        focus_the_panel(&panel, cx);
        // A windowless load never enters Live, so the panel is sitting in
        // the sticky Live mode with no session -- which is exactly the
        // state ctrl-alt-l has to be able to leave.
        assert_mode(&panel, cx, CanvasMode::Live);

        cx.simulate_keystrokes("ctrl-alt-l");
        cx.run_until_parked();

        assert_mode(&panel, cx, CanvasMode::Design);
    }

    /// The switch is on the view-control row in both modes, its buttons
    /// are wired, and the actions behind them flip the same state.
    #[gpui::test]
    async fn the_mode_switch_and_its_actions_flip_the_canvas(cx: &mut TestAppContext) {
        let (panel, _endpoint, _dir, cx) = connected_live_panel(cx).await;
        show_panel(cx);
        assert_eq!(
            toggle_of(cx, MODE_LIVE),
            Some(true),
            "the switch shows which renderer is drawing"
        );
        assert_eq!(toggle_of(cx, MODE_DESIGN), Some(false));
        let design = cx
            .debug_bounds("ggo-world-mode-design-off")
            .expect("the switch renders in Live mode too");
        cx.simulate_click(design.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_mode(&panel, cx, CanvasMode::Design);
        assert_eq!(toggle_of(cx, MODE_DESIGN), Some(true));
        assert_eq!(
            toggle_of(cx, MODE_LIVE),
            Some(false),
            "and both halves move together"
        );

        cx.dispatch_action(ToggleCanvasMode);
        cx.run_until_parked();
        assert_mode(&panel, cx, CanvasMode::Live);
        cx.dispatch_action(ToggleCanvasMode);
        cx.run_until_parked();
        assert_mode(&panel, cx, CanvasMode::Design);
        cx.dispatch_action(ToggleLive);
        cx.run_until_parked();
        assert_mode(&panel, cx, CanvasMode::Live);
        cx.dispatch_action(ToggleDesign);
        cx.run_until_parked();
        assert_mode(&panel, cx, CanvasMode::Design);
    }
}
