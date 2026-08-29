//! GGO Map panel (F5.1 Task M2): the fork's ONE art-authoring surface --
//! a tile-placing editor for `.map` files.
//!
//! It is the deliberate exception to F5's import-only art rule: tilesets
//! come in from external editors through `ggo_import_panel`, sprites are
//! assembled from those tiles in the sprite panel, and LEVELS are painted
//! here. Painting a level places *tiles*, never pixels, so this stays
//! inside the no-pixel-painting decision (spec, "The art pipeline"). Maps
//! are **authored, never imported** -- user's explicit call -- so unlike
//! `ggo_tileset_panel` (a read-only sheet viewer) this panel edits, and
//! carries the full editing-panel treatment: a doc store, undo/redo, save,
//! a dirty dot, the unsaved-document guard on document switch, and
//! `Panel::prepare_to_close` on window close.
//!
//! **No editing logic lives here.** Every mutation is a
//! `ggo_worldlib::sprites::map_doc::MapOp` applied to a `MapDocStore`, and
//! the stamp/selection maths are that module's `palette_sel_rect` +
//! `build_stamp` + `pack_cell`/`unpack_cell`. All of it was extracted and
//! unit-tested in worldlib during F1 round 2; this module is a gpui view
//! over tested logic. What IS this module's own is the gpui wiring, and
//! even there the geometry (cell hit-testing under zoom/pan, resize
//! clamping, the strip's tile indexing) lives in [`geom`] as pure,
//! separately-tested functions -- spec risk 2's explicit instruction, and
//! the discipline `ggo_world_panel::canvas` set.
//!
//! Which map is open is driven ENTIRELY by the file explorer: clicking a
//! `.map` routes here through [`intercept_map_open`] (the fork's FIFTH
//! `register_path_open_interceptor` predicate), and "New Map…" on an
//! assets directory creates one and opens it here. There is no in-panel
//! file picker.
//!
//! Split: [`loader`] owns everything off the UI thread (the `.map` open,
//! the bound tileset, both composes); [`geom`] owns the pure geometry;
//! [`paint_session`] owns the document, the tileset cache and the tool
//! state machine; this module owns the panel entity, the camera and the
//! gpui glue. That last line moved: map editing is coming to
//! `ggo_world_panel` (spec 2026-08-29), so everything that is not a gpui
//! view now lives in [`PaintSession`] where a second host can drive it.

mod geom;
pub mod loader;
mod map_item;
pub mod paint_session;

pub use map_item::MapEditorItem;
pub use paint_session::{MapTool, PaintSession};

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Context, Corners, Entity, FocusHandle, Focusable, Hsla,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Render, RenderImage, ScrollWheelEvent, Styled, Task, WeakEntity, Window, actions, bounds, div,
    fill, outline, point, px, size,
};
use project::ProjectPath;
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, DropdownMenu, ToggleState, Tooltip};

use ggo_worldlib::sprites::terrain::{self, Terrain};
use ggo_worldlib::sprites::tileset_meta::{load_tileset_meta, save_tileset_meta};
use workspace::Workspace;

use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::map_doc::{
    CELL_BLANK, MapDocStore, MapOp, Stamp, palette_sel_rect, unpack_cell,
};

actions!(
    ggo_map,
    [
        /// Copies the selected cells.
        Copy,
        /// Pastes the copied cells at the cursor (or the selection).
        Paste,
        /// Blanks the selected cells.
        DeleteSelection,
        /// Clears the cell selection.
        ClearSelection,
        /// Undoes the last edit to the open map.
        Undo,
        /// Redoes the last undone edit to the open map.
        Redo,
        /// Saves the open map to its `.map` file.
        Save,
        /// Applies the width/height fields to the open map (bound to Enter
        /// inside those fields).
        ApplyResize
    ]
);

/// The panel's key-dispatch context (`.key_context`), which the
/// [`bind_panel_keys`] bindings are scoped to.
const KEY_CONTEXT: &str = "GgoMapPanel";

/// The tileset strip's height. Two rows of 16px tiles at
/// [`geom::STRIP_ZOOM`] plus room to scroll a taller sheet.
const STRIP_HEIGHT: Pixels = px(104.);

/// The map extension this panel claims from the file explorer.
const MAP_EXT: &str = "map";

/// The assets subdirectory hanging off an emerald project root. Hardcoded
/// upstream -- it is NOT a configurable `emerald.toml` key. Same constant
/// `ggo_sprite_panel` resolves a `.spr`'s sidecars with; the project-root
/// walk itself is `ggo_common::emerald_project_root`.
const ASSETS_DIR: &str = "assets";

/// Empty-state text. The panel has no picker of its own by design: maps
/// arrive by clicking a `.map` in the project panel, or from "New Map…".
const EMPTY_MESSAGE: &str = "Open a .map file from the project panel";

pub fn init(cx: &mut App) {
    // Explorer-driven routing: clicking a `.map` in the project panel loads
    // it HERE instead of opening a (binary, unreadable) editor tab. This is
    // the panel's only way in -- there is no in-panel file picker.
    workspace::register_path_open_interceptor(cx, intercept_map_open);

    // Right-clicking an assets DIRECTORY offers "New Map…" -- maps are
    // authored, never imported, so this is how one comes into existence.
    workspace::register_context_menu_contributor(cx, contribute_map_menu);
}

/// Write a blank, unbound `.map` into the worktree-relative directory
/// `dir_rel`, returning the new file's worktree-relative path.
///
/// Split out of [`MapPanel::new_map`] so the write is one step that either
/// happens or doesn't: the caller runs it only after the unsaved-edits
/// guard has resolved, and a failed write leaves nothing behind to open.
/// `None` on a write failure (already logged).
/// The worktree-relative `.map` rel a typed inline name lands at, or
/// `Err(message)` for a name the editor must refuse -- the same stem
/// rules `ggo_emerald_panel`'s tileset applies: a separator would move
/// the write out of the clicked directory, `.`/`..` are not names, and a
/// retyped extension is accepted without doubling it.
fn map_rel(dir_rel: &str, typed: &str) -> Result<String, String> {
    let typed = typed.trim();
    let stem = typed
        .strip_suffix(&format!(".{MAP_EXT}"))
        .unwrap_or(typed)
        .trim();
    if stem.is_empty() {
        return Err("name cannot be empty".to_string());
    }
    if stem.contains('/') || stem.contains('\\') {
        return Err("name cannot contain a path separator".to_string());
    }
    if stem == "." || stem == ".." {
        return Err(format!("{stem} is not a name"));
    }
    let file = format!("{stem}.{MAP_EXT}");
    Ok(if dir_rel.is_empty() {
        file
    } else {
        format!("{}/{file}", dir_rel.trim_end_matches('/'))
    })
}

fn create_blank_map(project_root: &Path, source_rel: &str) -> Option<()> {
    let (root, rel_path) = split_map_path(project_root, source_rel);
    if root.join(&rel_path).exists() {
        // The inline editor pre-checks this; re-checked here so a race
        // can never truncate an existing map.
        log::error!("GGO: refusing to overwrite existing map {source_rel}");
        return None;
    }
    if let Err(e) = io::save_new_map(&root, &rel_path, geom::NEW_MAP_DIM, geom::NEW_MAP_DIM) {
        // No toast surface yet (F5.2 owns notifications), but a silent
        // no-op would be indistinguishable from a bug. Upstream logs AND
        // toasts at the same point.
        log::error!("GGO: failed to create map {source_rel}: {e}");
        return None;
    }
    Some(())
}

/// Does `path` name a map? The one rule, shared by the open interceptor and
/// (indirectly) by everything else that asks.
fn is_map_path(path: &ProjectPath) -> bool {
    path.path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(MAP_EXT))
}

/// `workspace::PathOpenInterceptor` for `*.map`: claim the path, open the
/// panel, and load it. Declines (so the normal open path runs) for any
/// other file, for a path outside the primary worktree of a LOCAL project,
/// and when no panel is docked.
///
/// One of five such predicates: `**/worlds/**/*.toml` (`ggo_world_panel`),
/// `*.cart` (`ggo_emu_panel`), `*.spr` (`ggo_sprite_panel`),
/// `*.til` (`ggo_tileset_panel`), and `*.map`. All five key off disjoint
/// paths, so registration order between them doesn't matter -- and it
/// isn't documented here because it drifts with alphabetical crate init
/// order in `crates/zed/src/main.rs`. (The `.cart` one was missed in this
/// module's first count -- fix round 1, FOLD IN 4.)
fn intercept_map_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !is_map_path(path) {
        return false;
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    open_map_item(workspace, rel, window, cx);
    true
}

/// Activate the tab already editing `rel`, or open one in the active
/// pane (one tab per `.map`, the tileset rule).
pub fn open_map_item(
    workspace: &mut Workspace,
    rel: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<MapEditorItem>(cx)
        .find(|item| item.read(cx).rel() == rel);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let item = cx.new(|cx| MapEditorItem::new(rel, weak, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

/// `workspace::ContextMenuContributor` for an assets DIRECTORY: "New
/// Map…".
///
/// Gated to directories inside an emerald project's `assets/` tree, which
/// is the frame a `.map`'s own `til_path` resolves in -- a map written
/// anywhere else could not name a tileset correctly (the F4 `ggo-sprfix`
/// contract). That check is a pair of `is_file`/`is_dir` stats on the
/// clicked path's ancestors: cheap, and legal in a contributor.
///
/// MUST NOT touch the project panel or any GGO panel: contributors run
/// while `ProjectPanel` is leased (see
/// `Workspace::context_menu_contributions`). Everything panel-shaped is
/// deferred into the entry's handler via
/// [`ggo_common::panel_entry_handler`], which runs after the lease is
/// released.
fn contribute_map_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    if !is_dir {
        return Vec::new();
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    let Some(worktree_root) = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    else {
        return Vec::new();
    };
    let dir_abs = worktree_root.join(&rel);
    if !is_assets_dir(&dir_abs) {
        return Vec::new();
    }
    vec![
        ui::ContextMenuEntry::new("New Map…")
            .icon(ui::IconName::SquareDot)
            .handler(new_map_handler(
                cx.weak_entity(),
                path.worktree_id,
                rel,
                dir_abs,
            ))
            .into(),
    ]
}

/// The "New Map…" entry's handler: seed the project panel's inline name
/// editor (New File's UX) in the clicked directory; the commit reveals
/// this panel and creates + opens the named blank map. Split out from
/// [`contribute_map_menu`] so a test can invoke exactly what the menu
/// invokes -- `ContextMenuEntry` keeps its handler private, so a
/// contributed entry cannot be fired from a test any other way.
fn new_map_handler(
    workspace: WeakEntity<Workspace>,
    worktree_id: project::WorktreeId,
    dir_rel: String,
    dir_abs: PathBuf,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(
        workspace.clone(),
        move |panel: &Entity<project_panel::ProjectPanel>, window, cx| {
            let workspace = workspace.clone();
            let dir_rel = dir_rel.clone();
            let dir_abs = dir_abs.clone();
            panel.update(cx, |panel, cx| {
                let Some(path) = ggo_common::inline_project_path(worktree_id, &dir_rel) else {
                    return;
                };
                panel.ggo_new_entry_inline(
                    &path,
                    map_validate(dir_abs),
                    new_map_commit(workspace, dir_rel.clone()),
                    window,
                    cx,
                );
            });
        },
    )
}

/// The inline map-name gate: [`map_rel`]'s stem rules plus the
/// already-exists refusal, surfaced while typing.
fn map_validate(dir_abs: PathBuf) -> impl Fn(&str) -> Option<String> + 'static {
    move |typed| match map_rel("", typed) {
        Err(error) => Some(error),
        Ok(file) => dir_abs
            .join(&file)
            .exists()
            .then(|| format!("{file} already exists here.")),
    }
}

/// The inline map commit: reveal + focus this panel, then create and open
/// the named map -- same shape as `ggo_emerald_panel`'s world commit.
fn new_map_commit(
    workspace: WeakEntity<Workspace>,
    dir_rel: String,
) -> impl FnOnce(String, &mut Window, &mut App) + 'static {
    move |typed, window, cx| {
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let Some(project_root) = workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            else {
                return;
            };
            let Ok(source_rel) = map_rel(&dir_rel, &typed) else {
                return;
            };
            if create_blank_map(&project_root, &source_rel).is_none() {
                return;
            }
            open_map_item(workspace, source_rel, window, cx);
        });
    }
}

/// Walk up from `dir` (inclusive) to the nearest emerald project root
/// (`ggo_common::emerald_project_root`), returning that project's `assets/`
/// dir.
///
/// The `ggo_sprite_panel` twin takes a FILE and starts at its parent;
/// this one takes a directory and starts at the directory itself, because
/// "New Map…" is offered on `assets/` itself as well as on subdirectories.
fn emerald_asset_root(dir: &Path) -> Option<PathBuf> {
    let assets = ggo_common::emerald_project_root(dir)?.join(ASSETS_DIR);
    assets.is_dir().then_some(assets)
}

/// Is `dir` the asset root of an emerald project, or a directory under it?
fn is_assets_dir(dir: &Path) -> bool {
    emerald_asset_root(dir).is_some_and(|assets| dir.starts_with(&assets))
}

/// The asset root a `.map` resolves its `til_path`/`pal_path` against, plus
/// the map's path relative to THAT root.
///
/// Same rule (and same reason) as `ggo_sprite_panel::split_sprite_path`:
/// emerald treats an asset's sidecar rels as **asset-root-relative**, where
/// the asset root is `<project>/assets`. Clicking `<proj>/assets/maps/a.map`
/// therefore has to yield root `<proj>/assets` and rel `maps/a.map`, so the
/// `til_path` stored inside reads `tiles/x.til`, not `assets/tiles/x.til` --
/// writing the latter is precisely the bug `ggo-sprfix` exists to repair.
///
/// Falls back to `(project_root, rel)` when the file isn't inside an
/// emerald project's `assets/` tree, so a bare `.map` in a non-emerald
/// worktree still opens.
fn split_map_path(project_root: &Path, rel: &str) -> (PathBuf, String) {
    let abs = project_root.join(rel);
    if let Some(assets) = abs.parent().and_then(emerald_asset_root)
        && let Ok(under) = abs.strip_prefix(&assets)
    {
        let under = under
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        return (assets, under);
    }
    (project_root.to_path_buf(), rel.to_string())
}

// ------------------------------------------------------------- view state

/// An in-flight middle-mouse pan drag on the map canvas.
#[derive(Clone, Copy)]
struct PanDrag {
    start_cursor: [f32; 2],
    start_pan: [f32; 2],
}

/// The two resize fields.
struct ResizeFields {
    w: Entity<Editor>,
    h: Entity<Editor>,
}

/// A loaded map plus everything the VIEW needs. The document, the tileset
/// cache and the tool state machine are the embedded [`PaintSession`]
/// (which outlives this panel -- see that module's doc); what is left here
/// is camera, hit-test bounds and the gpui editors.
///
/// Both halves are dropped with the document, so an already-open re-click
/// -- which does not rebuild this -- preserves all of it.
struct OpenMap {
    /// The worktree-relative path as CLICKED. This is what identifies the
    /// file to the explorer and to the user: it answers "is this click the
    /// map that is already open?" and it is what the unsaved-edits prompt
    /// names.
    source_rel: String,
    session: PaintSession,
    /// Every `.til` under the session's asset root, for the bind picker.
    tilesets: Vec<String>,
    /// The composed map. Rebuilt after every mutation from the LIVE store
    /// (`PaintSession::live_image`), never per render.
    image: Option<Arc<RenderImage>>,
    show_grid: bool,
    zoom: usize,
    pan: [f32; 2],
    /// The canvas elements' on-screen bounds, recorded at prepaint so the
    /// mouse handlers can map window coords to cell hits
    /// (`ggo_world_panel`'s `last_bounds` idiom). `None` until the first
    /// Ready-state paint.
    canvas_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    strip_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    pan_drag: Option<PanDrag>,
    pal_dragging: bool,
    /// The cell under the cursor while it is over the canvas -- where a
    /// paste lands.
    hover_cell: Option<(i32, i32)>,
    /// The 8-neighbour mask the terrain editor assigns next.
    mask_draft: u8,
    /// The terrain editor's name input, created on the first Ready render.
    terrain_name: Option<Entity<Editor>>,
    /// True between the canvas's own primary-down and its matching up, for
    /// EVERY tool -- ggo-ide's single `painting` flag, ported as one flag
    /// for the same reason: it gates brush/eraser continuing to apply,
    /// eyedropper continuing to pick, AND rect-fill continuing to extend.
    painting: bool,
    resize: Option<ResizeFields>,
}

impl OpenMap {
    fn new(
        source_rel: String,
        rel_path: String,
        root: PathBuf,
        mut loaded: loader::LoadedMap,
    ) -> Self {
        OpenMap {
            source_rel,
            tilesets: std::mem::take(&mut loaded.tilesets),
            image: loaded.image.take(),
            session: PaintSession::new(rel_path, root, loaded),
            show_grid: true,
            zoom: geom::DEFAULT_ZOOM,
            pan: [0.0, 0.0],
            canvas_bounds: Rc::new(RefCell::new(None)),
            strip_bounds: Rc::new(RefCell::new(None)),
            pan_drag: None,
            pal_dragging: false,
            hover_cell: None,
            mask_draft: 0,
            terrain_name: None,
            painting: false,
            resize: None,
        }
    }

    /// Canvas-local px for a window-space position, or `None` before the
    /// first layout.
    fn canvas_local(&self, position: gpui::Point<Pixels>) -> Option<[f32; 2]> {
        let bounds = (*self.canvas_bounds.borrow())?;
        Some([
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ])
    }

    fn strip_local(&self, position: gpui::Point<Pixels>) -> Option<[f32; 2]> {
        let bounds = (*self.strip_bounds.borrow())?;
        Some([
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ])
    }

    /// The map cell under a window-space position.
    fn cell_at(&self, position: gpui::Point<Pixels>) -> Option<(i32, i32)> {
        let local = self.canvas_local(position)?;
        let state = self.session.store.state();
        geom::grid_cell_at(local, self.zoom, self.pan, state.w, state.h)
    }

    /// The strip cell under a window-space position, gated to tiles the
    /// bound tileset actually has (ggo-ide's single `>= tile_count`
    /// early-return, shared by the anchor and every dragged-over cell --
    /// a drag into the sheet's zero-filled partial-row padding must not
    /// move the selection there).
    fn strip_cell_at(&self, position: gpui::Point<Pixels>) -> Option<(i32, i32)> {
        let tileset = self.session.tileset.as_ref()?;
        let local = self.strip_local(position)?;
        let (c, r) = geom::grid_cell_at(
            local,
            geom::STRIP_ZOOM,
            [0.0, 0.0],
            tileset.cols as u16,
            tileset.rows() as u16,
        )?;
        let cols = tileset.cols.max(1);
        (r as usize * cols + (c as usize) < tileset.tile_count).then_some((c, r))
    }
}

enum ViewerState {
    /// Nothing opened yet.
    Empty,
    Loading {
        rel_path: String,
    },
    Ready(Box<OpenMap>),
    Error(String),
}

pub struct MapPanel {
    focus_handle: FocusHandle,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// Copied cells; panel-level so it survives switching maps.
    clipboard: Option<Stamp>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl MapPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            root_override: None,
            project_root: None,
            clipboard: None,
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree). MUST NOT run while the workspace itself is mid-update (it
    /// reads the workspace entity) -- see the deferral in `set_active` and
    /// in [`Self::open_rel_path`].
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        cx.notify();
    }

    /// Load the worktree-relative `.map` path `rel`, prompting FIRST if the
    /// open map has unsaved edits -- Cancel leaves the current document
    /// loaded and dirty and abandons the open.
    ///
    /// Everything after the guard runs on a spawned task, deliberately: the
    /// interceptor calls this from INSIDE the workspace's own update, and
    /// [`Self::refresh_root`] has to read that same workspace entity.
    /// Load `rel`, guarding any unsaved edits first.
    ///
    /// Since the editor became a tab per `.map` ([`MapEditorItem`]) every
    /// panel opens exactly one document, so in practice the guard sees a
    /// clean panel; it stays because this is `pub` and a second open on a
    /// live panel must not drop edits. Closing a dirty TAB is the pane's
    /// prompt, driven by `Item::is_dirty` -- not this.
    pub fn open_rel_path(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Clicking the file that is ALREADY open is how you bring the panel
        // back into focus, and upstream's semantics for that click on a tab
        // are "activate the existing item", not "reload it". The interceptor
        // has already revealed and focused the dock by the time we get here,
        // so there is nothing left to do -- and doing anything would either
        // prompt (offering a "Don't Save" the user never asked for) or drop
        // the undo stack, the stamp selection and the camera on the floor.
        if let ViewerState::Ready(open) = &self.state
            && open.source_rel == rel
        {
            return;
        }
        let rel = rel.to_string();
        let proceed = self.prepare_to_close(window, cx);
        cx.spawn(async move |this, cx| {
            if !proceed.await {
                return;
            }
            this.update(cx, |this, cx| {
                this.refresh_root(cx);
                this.load_rel_path(&rel, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Kick off the off-thread load of the worktree-relative path `rel`,
    /// against the asset root DERIVED from it ([`split_map_path`]). A stale
    /// result (superseded by a later open) is dropped by generation check.
    /// Re-read `rel` from disk, DISCARDING any unsaved edits -- the
    /// "Don't Save" answer to the tab's close prompt. Unlike
    /// `open_rel_path` this asks nothing: the user already answered.
    pub(crate) fn reload_from_disk(&mut self, rel: &str, cx: &mut Context<Self>) {
        self.refresh_root(cx);
        self.load_rel_path(rel, cx);
    }

    fn load_rel_path(&mut self, rel: &str, cx: &mut Context<Self>) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let (root, rel_path) = split_map_path(&project_root, rel);
        let source_rel = rel.to_string();
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            rel_path: source_rel.clone(),
        };
        cx.notify();

        let load = {
            let (root, rel_path) = (root.clone(), rel_path.clone());
            cx.background_spawn(async move { loader::load_map(&root, &rel_path, &project_root) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => ViewerState::Ready(Box::new(OpenMap::new(
                        source_rel, rel_path, root, loaded,
                    ))),
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Only the tests reach this now: the "New Map…" commit writes the
    /// file and opens its tab directly (`open_map_item`).
    #[cfg(test)]
    /// Create a blank `.map` in the worktree-relative directory `dir_rel`
    /// and open it -- the body of the project panel's "New Map…" entry.
    ///
    /// **The new map is UNBOUND** (`til_path`/`pal_path` empty), and the
    /// panel's tileset picker is how one gets attached. That is worldlib's
    /// own documented contract for a fresh map (`io::save_new_map`: "not
    /// bound to any tileset yet -- `MapOp::BindTileset` is how the editor
    /// attaches one afterward"), and it is the right call here for three
    /// reasons. (1) A map's cells are pool INDICES: bound to the wrong
    /// tileset, every cell in it means something else, so a guessed
    /// binding ("the nearest `.til`") is worse than none -- it produces a
    /// map that looks authored and is wrong. (2) `window.prompt` is
    /// button-choice only, so there is no picker to run at creation time
    /// without inventing a dialog system the spec explicitly rules out.
    /// (3) The panel needs the rebind affordance anyway (`BindTileset` is
    /// in this task's scope), so the follow-up bind step costs no extra
    /// surface -- it IS the surface.
    ///
    /// **The unsaved-edits guard runs BEFORE anything is written** (fix
    /// round 1, BLOCKING 2). Creating first and prompting afterwards --
    /// which is what routing straight into [`Self::open_rel_path`] did --
    /// meant a Cancel left a `map.map` orphaned on disk while the old
    /// document stayed on screen, and the next attempt then created
    /// `map-2.map` beside it. ggo-ide guards first too
    /// (`pages/assets/mod.rs`'s `OpenNewMapClicked` -> `confirm_task` ->
    /// `PendingAction::OpenNewMap`). The continuation calls
    /// [`Self::load_rel_path`] rather than [`Self::open_rel_path`], since
    /// the guard has already been satisfied and prompting twice would be
    /// worse than not prompting at all.
    ///
    /// Refreshes the root FIRST because `project_root` is only
    /// re-discovered on panel activation, and a right-click in the
    /// explorer can reach a panel that has never been activated. Safe
    /// here: the caller is a context-menu entry handler, which runs
    /// outside both the project panel's lease and any `Workspace` update.
    /// Create the inline-named blank map in worktree-relative `dir_rel`
    /// and open it, keeping [`new_map`]'s dirty-map guard: an unsaved open
    /// map gets its save/discard prompt before the new one replaces it.
    pub fn create_map_inline(
        &mut self,
        dir_rel: &str,
        typed: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_root(cx);
        if self.project_root.is_none() {
            return;
        }
        // Re-checked here (the inline editor already validated) so this
        // method is safe to call from anywhere.
        let Ok(source_rel) = map_rel(dir_rel, typed) else {
            return;
        };
        let proceed = ggo_common::prepare_to_close_dirty(
            self.dirty_map_name(),
            window,
            cx,
            Self::save_for_close,
        );
        cx.spawn(async move |this, cx| {
            if !proceed.await {
                return;
            }
            this.update(cx, |this, cx| {
                this.refresh_root(cx);
                let Some(project_root) = this.project_root.clone() else {
                    return;
                };
                if create_blank_map(&project_root, &source_rel).is_none() {
                    return;
                }
                this.load_rel_path(&source_rel, cx);
            })
            .ok();
        })
        .detach();
    }

    // ------------------------------------------------------------ editing

    /// Apply one op to the open map's store, recompose, and repaint. Every
    /// mutation funnels through here.
    fn apply_op(&mut self, op: MapOp, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.session.apply(op);
            Self::rebuild_image(open);
            cx.notify();
        }
    }

    /// Refresh the canvas image from the session's live compose. Called
    /// once per mutation, never per render -- see
    /// [`PaintSession::live_image`] for why that matters.
    fn rebuild_image(open: &mut OpenMap) {
        open.image = open.session.live_image();
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        self.step_history(MapDocStore::undo, cx);
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        self.step_history(MapDocStore::redo, cx);
    }

    /// Undo or redo, then repaint. The tileset-cache resync a step across
    /// a `BindTileset` needs is [`PaintSession::step_history`]'s job.
    fn step_history(&mut self, step: fn(&mut MapDocStore) -> bool, cx: &mut Context<Self>) {
        let project_root = self.project_root.clone();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // An exhausted stack changes nothing, so it must not cost a
        // recompose or a repaint.
        if !open.session.step_history(step, project_root.as_deref()) {
            return;
        }
        Self::rebuild_image(open);
        cx.notify();
    }

    /// Write the document back to the file it was read from, then repaint
    /// (the dirty dot and any error banner both come off this).
    /// [`PaintSession::save`] is the write itself.
    fn save_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // The failure is already recorded on the session, where the error
        // banner renders it and `save_for_close` reads it back, so there is
        // nothing to propagate from here -- only to make visible in the log.
        if let Err(error) = open.session.save() {
            log::warn!(
                "ggo map panel: {} not saved: {error}",
                open.session.rel_path
            );
        }
        cx.notify();
    }

    /// Save on behalf of a "Save" answer to the unsaved-edits prompt,
    /// reporting whether the write actually landed (a failed write must not
    /// let the caller discard the document).
    fn save_for_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_impl(cx);
        match &self.state {
            ViewerState::Ready(open) => open.session.save_error.is_none(),
            _ => true,
        }
    }

    /// The open map's display path when it has unsaved edits, else `None`.
    /// Drives both the close guard and (indirectly) the title's dirty dot.
    /// The dirty guard a tab close (or a map switch) runs: prompt, save
    /// on request, and answer whether to proceed.
    pub(crate) fn prepare_to_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        ggo_common::prepare_to_close_dirty(self.dirty_map_name(), window, cx, Self::save_for_close)
    }

    /// Whether the open map has unsaved edits (the tab's dirty dot).
    pub fn dirty(&self) -> bool {
        self.dirty_map_name().is_some()
    }

    fn dirty_map_name(&self) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        // The CLICKED path, not the asset-root-relative one: the prompt has
        // to name the file the way the user sees it in the explorer.
        open.session.store.dirty().then(|| open.source_rel.clone())
    }

    /// Bind (or rebind) the tileset at asset-root-relative `til_rel`, then
    /// recompose. [`PaintSession::bind_tileset`] is the resolve-then-apply
    /// rule itself.
    fn bind_tileset(&mut self, til_rel: String, cx: &mut Context<Self>) {
        let project_root = self.project_root.clone();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // A REFUSED bind leaves the document -- and so the composed pixels
        // -- untouched; only the error banner is new, so this repaints
        // without recomposing.
        if open.session.bind_tileset(til_rel, project_root.as_deref()) {
            Self::rebuild_image(open);
        }
        cx.notify();
    }

    /// One tool application at `cell`, then a recompose if the document
    /// moved -- [`PaintSession::paint_at`] is the tool state machine.
    ///
    /// An inert gesture ([`PaintSession::can_paint`]: no bound tileset, or
    /// the Terrain tool with no terrain selected) returns without even a
    /// repaint, because a drag over an unbound map fires this on EVERY
    /// mouse move.
    fn paint_at(&mut self, cell: (i32, i32), cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if !open.session.can_paint() {
            return;
        }
        if open.session.paint_at(cell) {
            Self::rebuild_image(open);
        }
        cx.notify();
    }

    /// Release: settle a select drag, commit a pending rect-fill, and end
    /// the gesture.
    fn end_paint(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.painting = false;
        open.session.store.end_stroke();
        open.session.commit_selection();
        if open.session.commit_rect() {
            Self::rebuild_image(open);
        }
        cx.notify();
    }

    // ------------------------------------------------- selection clipboard

    /// The selected cells as a stamp, clipped to the map.
    fn selection_stamp(open: &OpenMap) -> Option<Stamp> {
        let (x0, y0, x1, y1) = open.session.selection?;
        let state = open.session.store.state();
        let (x0, y0) = (x0.max(0), y0.max(0));
        let (x1, y1) = (x1.min(state.w as i32 - 1), y1.min(state.h as i32 - 1));
        if x1 < x0 || y1 < y0 {
            return None;
        }
        let (w, h) = ((x1 - x0 + 1) as u16, (y1 - y0 + 1) as u16);
        let mut cells = Vec::with_capacity(w as usize * h as usize);
        for y in y0..=y1 {
            for x in x0..=x1 {
                cells.push(state.cells[y as usize * state.w as usize + x as usize]);
            }
        }
        Some(Stamp { w, h, cells })
    }

    fn copy_impl(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.ready_map() else { return };
        if let Some(stamp) = Self::selection_stamp(open) {
            self.clipboard = Some(stamp);
            cx.notify();
        }
    }

    /// Where a paste lands: the cell under the cursor when it is over the
    /// canvas, else the selection's top-left, else the origin.
    fn paste_origin(open: &OpenMap) -> (i32, i32) {
        open.hover_cell
            .or_else(|| open.session.selection.map(|(x0, y0, _, _)| (x0, y0)))
            .unwrap_or((0, 0))
    }

    fn paste_impl(&mut self, cx: &mut Context<Self>) {
        let Some(stamp) = self.clipboard.clone() else {
            return;
        };
        let Some(open) = self.ready_map() else { return };
        if open.session.tileset.is_none() {
            return;
        }
        let (x, y) = Self::paste_origin(open);
        let (w, h) = (stamp.w as i32, stamp.h as i32);
        if let ViewerState::Ready(open) = &mut self.state {
            // A paste is its own undo step, never part of a drag that
            // ended off-canvas.
            open.session.store.end_stroke();
        }
        self.apply_op(MapOp::Brush { x, y, stamp }, cx);
        if let ViewerState::Ready(open) = &mut self.state {
            open.session.selection = Some((x, y, x + w - 1, y + h - 1));
        }
    }

    fn delete_selection_impl(&mut self, cx: &mut Context<Self>) {
        let Some((x0, y0, x1, y1)) = self
            .ready_map()
            .filter(|open| open.session.tileset.is_some())
            .and_then(|open| open.session.selection)
        else {
            return;
        };
        self.apply_op(
            MapOp::RectFill {
                x0,
                y0,
                x1,
                y1,
                cell: CELL_BLANK,
            },
            cx,
        );
    }

    fn clear_selection_impl(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.session.selection = None;
            open.session.sel_pending = None;
            cx.notify();
        }
    }

    // ------------------------------------------------------------ terrains

    /// The tile the terrain editor assigns: the stamp's first cell.
    fn anchor_tile(open: &OpenMap) -> Option<u16> {
        let cell = open.session.fill_cell();
        (open.session.tileset.is_some() && cell != CELL_BLANK).then(|| unpack_cell(cell).tile)
    }

    fn terrain_name_text(&self, cx: &App) -> String {
        self.ready_map()
            .and_then(|open| open.terrain_name.as_ref())
            .map(|editor| editor.read(cx).text(cx).trim().to_string())
            .unwrap_or_default()
    }

    fn add_terrain(&mut self, name: String, cx: &mut Context<Self>) {
        if name.is_empty() {
            return;
        }
        if let ViewerState::Ready(open) = &mut self.state
            && open.session.tileset.is_some()
        {
            open.session.terrains.push(Terrain {
                name,
                tiles: vec![],
            });
            open.session.terrain = Some(open.session.terrains.len() - 1);
        }
        self.save_terrains(cx);
    }

    fn rename_terrain(&mut self, name: String, cx: &mut Context<Self>) {
        if name.is_empty() {
            return;
        }
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(terrain) = open
                .session
                .terrain
                .and_then(|i| open.session.terrains.get_mut(i))
        {
            terrain.name = name;
            self.save_terrains(cx);
        }
    }

    fn remove_terrain(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(index) = open.session.terrain.take()
            && index < open.session.terrains.len()
        {
            open.session.terrains.remove(index);
            self.save_terrains(cx);
        }
    }

    fn select_terrain(&mut self, index: usize, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.session.terrain = (index < open.session.terrains.len()).then_some(index);
            cx.notify();
        }
    }

    /// Give the anchor tile the drafted mask in the selected terrain.
    fn assign_anchor_tile(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(tile) = Self::anchor_tile(open)
            && let Some(terrain) = open
                .session
                .terrain
                .and_then(|i| open.session.terrains.get_mut(i))
        {
            terrain.assign(tile, terrain::canonical(open.mask_draft));
            self.save_terrains(cx);
        }
    }

    fn unassign_tile(&mut self, tile: u16, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(terrain) = open
                .session
                .terrain
                .and_then(|i| open.session.terrains.get_mut(i))
        {
            terrain.remove_tile(tile);
            self.save_terrains(cx);
        }
    }

    /// Write the terrains into the bound tileset's editor sidecar,
    /// keeping whatever else (zoom, cols) the tileset panel stored there.
    fn save_terrains(&mut self, cx: &mut Context<Self>) {
        let project_root = self.project_root.clone();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.session.terrain_error = match (&project_root, &open.session.til_meta_rel) {
            (Some(root), Some(rel)) => {
                let mut meta = load_tileset_meta(root, rel);
                meta.terrains = open.session.terrains.clone();
                save_tileset_meta(root, rel, &meta).err()
            }
            _ => Some("tileset is outside the worktree; terrains not saved".to_string()),
        };
        cx.notify();
    }

    /// The terrain editor, shown while the Terrain tool is active.
    fn render_terrains(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let open = self
            .ready_map()
            .filter(|open| open.session.tool == MapTool::Terrain)?;
        let selected = open
            .session
            .terrain
            .and_then(|i| open.session.terrains.get(i));
        let anchor = Self::anchor_tile(open);
        let mask = open.mask_draft;
        let name_field = open.terrain_name.as_ref().map(|editor| {
            div()
                .w(px(120.))
                .px_1()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .rounded_sm()
                .child(editor.clone())
        });
        // Rows of the 3x3 neighbour pad; the centre is the anchor tile.
        let pad = [
            [
                Some(terrain::NORTH_WEST),
                Some(terrain::NORTH),
                Some(terrain::NORTH_EAST),
            ],
            [Some(terrain::WEST), None, Some(terrain::EAST)],
            [
                Some(terrain::SOUTH_WEST),
                Some(terrain::SOUTH),
                Some(terrain::SOUTH_EAST),
            ],
        ];
        Some(
            v_flex()
                .gap_1()
                .p_1()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .children(name_field)
                        .child(
                            Button::new("ggo-map-terrain-add", "Add")
                                .disabled(open.session.tileset.is_none())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let name = this.terrain_name_text(cx);
                                    this.add_terrain(name, cx);
                                })),
                        )
                        .child(
                            Button::new("ggo-map-terrain-rename", "Rename")
                                .disabled(selected.is_none())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let name = this.terrain_name_text(cx);
                                    this.rename_terrain(name, cx);
                                })),
                        )
                        .child(
                            Button::new("ggo-map-terrain-remove", "Remove")
                                .disabled(selected.is_none())
                                .on_click(cx.listener(|this, _, _, cx| this.remove_terrain(cx))),
                        )
                        .children(open.session.terrains.iter().enumerate().map(|(i, t)| {
                            Button::new(("ggo-map-terrain", i), t.name.clone())
                                .toggle_state(open.session.terrain == Some(i))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.select_terrain(i, cx)),
                                )
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(v_flex().children(pad.iter().enumerate().map(|(row, bits)| {
                            h_flex().children(bits.iter().enumerate().map(|(col, bit)| {
                                let id = ("ggo-map-mask", row * 3 + col);
                                match bit {
                                    Some(bit) => {
                                        let bit = *bit;
                                        Button::new(id, if mask & bit != 0 { "■" } else { "·" })
                                            .toggle_state(mask & bit != 0)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let ViewerState::Ready(open) = &mut this.state {
                                                    open.mask_draft ^= bit;
                                                    cx.notify();
                                                }
                                            }))
                                    }
                                    None => Button::new(
                                        id,
                                        anchor.map_or("—".to_string(), |t| format!("T{t}")),
                                    )
                                    .disabled(true),
                                }
                            }))
                        })))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Button::new("ggo-map-terrain-assign", "Assign")
                                        .disabled(selected.is_none() || anchor.is_none())
                                        .tooltip(Tooltip::text(
                                            "Give the stamp's first tile this neighbour mask",
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.assign_anchor_tile(cx)
                                        })),
                                )
                                .child(
                                    Label::new(terrain::mask_glyphs(terrain::canonical(mask)))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(h_flex().gap_1().flex_wrap().children(
                            selected.into_iter().flat_map(|t| t.tiles.iter()).map(|tt| {
                                let tile = tt.tile;
                                h_flex()
                                    .gap_0p5()
                                    .child(
                                        Label::new(format!(
                                            "T{} {}",
                                            tile,
                                            terrain::mask_glyphs(tt.mask)
                                        ))
                                        .size(LabelSize::XSmall),
                                    )
                                    .child(
                                        IconButton::new(
                                            ("ggo-map-terrain-tile", tile as usize),
                                            IconName::Close,
                                        )
                                        .icon_size(IconSize::XSmall)
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.unassign_tile(tile, cx)
                                            }),
                                        ),
                                    )
                            }),
                        )),
                )
                .children(open.session.terrain_error.as_ref().map(|e| {
                    Label::new(e.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Warning)
                }))
                .into_any_element(),
        )
    }

    // -------------------------------------------------------- resize field

    /// Keep the two resize inputs in sync with the document: create them on
    /// the first Ready render (seeded from the current size), and afterwards
    /// refresh any UNFOCUSED one whose text no longer matches the document
    /// -- which is how an undo/redo of a `Resize`, or the clamp a resize
    /// applied, shows up in the fields.
    ///
    /// Skipping the focused editor is `ggo_world_panel::ensure_inspector`'s
    /// rule and matters for the same reason: a render must never yank the
    /// digits out from under someone mid-type.
    ///
    /// There are exactly two, fixed, targets, so there is no
    /// target-set-changed rebuild the way the world panel's inspector has.
    fn ensure_resize_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let state = open.session.store.state();
        let Some(fields) = &open.resize else {
            let w = Self::new_size_field(state.w, window, cx);
            let h = Self::new_size_field(state.h, window, cx);
            if let ViewerState::Ready(open) = &mut self.state {
                open.resize = Some(ResizeFields { w, h });
            }
            return;
        };
        for (editor, value) in [(&fields.w, state.w), (&fields.h, state.h)] {
            if editor.focus_handle(cx).is_focused(window) {
                continue;
            }
            let text = value.to_string();
            if editor.read(cx).text(cx) != text {
                editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
        }
    }

    fn new_size_field(value: u16, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(value.to_string(), window, cx);
            editor
        })
    }

    /// Apply the resize fields. Explicit (the button, or Enter in a field),
    /// never on blur: a stray focus change must not resize the document.
    /// Unparsable text is a no-op; an out-of-range NUMBER clamps
    /// ([`geom::parse_dim`]).
    fn resize_impl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(fields) = &open.resize else {
            return;
        };
        let (Some(w), Some(h)) = (
            geom::parse_dim(&fields.w.read(cx).text(cx)),
            geom::parse_dim(&fields.h.read(cx).text(cx)),
        ) else {
            return;
        };
        let (w_editor, h_editor) = (fields.w.clone(), fields.h.clone());
        self.apply_op(MapOp::Resize { w, h }, cx);
        // The clamp may have changed what the user typed, and the field is
        // still focused (they just pressed Enter in it), so `ensure_resize_fields`
        // would skip it -- write the applied value back here.
        for (editor, value) in [(w_editor, w), (h_editor, h)] {
            editor.update(cx, |editor, cx| {
                editor.set_text(value.to_string(), window, cx)
            });
        }
    }

    // ------------------------------------------------------------- render

    /// [`Self::render_message`] for FAILURES: same centered layout, but
    /// the text is copyable so the error can be pasted into a report.
    fn render_load_error(&self, message: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(
                ggo_common::CopyableText::new("ggo-map-load-error-copy", message)
                    .size(LabelSize::Default),
            )
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
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

    /// Title + dirty dot + undo/redo/save.
    fn render_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_header is only called in the Ready state");
        };
        let dirty = open.session.store.dirty();
        let state = open.session.store.state();
        let title = format!("{}{}", open.source_rel, if dirty { " ●" } else { "" });
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
            .child(
                Label::new(format!("{}x{}", state.w, state.h))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("ggo-map-undo", IconName::Undo)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Undo"))
                    .on_click(cx.listener(|this, _, _, cx| this.undo_impl(cx))),
            )
            .child(
                IconButton::new("ggo-map-redo", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Redo"))
                    .on_click(cx.listener(|this, _, _, cx| this.redo_impl(cx))),
            )
            .child(
                Button::new("ggo-map-save", "Save")
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, _, cx| this.save_impl(cx))),
            )
            .into_any_element()
    }

    /// Tools + flips + palSub + grid + zoom.
    fn render_tools(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_tools is only called in the Ready state");
        };
        let (tool, hflip, vflip, pal_sub, grid, zoom) = (
            open.session.tool,
            open.session.hflip,
            open.session.vflip,
            open.session.pal_sub,
            open.show_grid,
            open.zoom,
        );
        let weak = cx.weak_entity();
        h_flex()
            .gap_1()
            .p_1()
            .flex_wrap()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .children(MapTool::ALL.map(|t| {
                IconButton::new(t.id(), t.icon())
                    .icon_size(IconSize::Small)
                    .toggle_state(tool == t)
                    .tooltip(Tooltip::text(t.label()))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tool(t, cx)))
            }))
            .child(
                IconButton::new("ggo-map-hflip", IconName::ArrowRightLeft)
                    .icon_size(IconSize::Small)
                    .toggle_state(hflip)
                    .tooltip(Tooltip::text("Flip stamp horizontally"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &mut this.state {
                            open.session.hflip = !open.session.hflip;
                            cx.notify();
                        }
                    })),
            )
            .child(
                IconButton::new("ggo-map-vflip", IconName::ExpandVertical)
                    .icon_size(IconSize::Small)
                    .toggle_state(vflip)
                    .tooltip(Tooltip::text("Flip stamp vertically"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &mut this.state {
                            open.session.vflip = !open.session.vflip;
                            cx.notify();
                        }
                    })),
            )
            .child(
                Label::new("pal")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(pal_sub.to_string()).size(LabelSize::XSmall))
            .child(
                ui::Slider::new(
                    "ggo-map-pal-sub",
                    ui::slider_fraction(
                        pal_sub as usize,
                        geom::PAL_SUB_MIN as usize,
                        geom::PAL_SUB_MAX as usize,
                    ),
                )
                .width(px(64.))
                .on_change({
                    let weak = cx.weak_entity();
                    move |value, _window, cx| {
                        let pal_sub = ui::slider_step(
                            value,
                            geom::PAL_SUB_MIN as usize,
                            geom::PAL_SUB_MAX as usize,
                        ) as u16;
                        weak.update(cx, |this, cx| this.set_pal_sub(pal_sub, cx))
                            .ok();
                    }
                }),
            )
            .child(
                Checkbox::new("ggo-map-grid", ToggleState::from(grid))
                    .label("Grid")
                    .on_click(move |toggle, _window, cx| {
                        let on = matches!(toggle, ToggleState::Selected);
                        weak.update(cx, |this, cx| {
                            if let ViewerState::Ready(open) = &mut this.state {
                                open.show_grid = on;
                                cx.notify();
                            }
                        })
                        .ok();
                    }),
            )
            .child(Label::new(format!("{zoom}x")).size(LabelSize::XSmall))
            .child(
                ui::Slider::new(
                    "ggo-map-zoom",
                    ui::slider_fraction(zoom, geom::MIN_ZOOM, geom::MAX_ZOOM),
                )
                .width(px(64.))
                .on_change({
                    let weak = cx.weak_entity();
                    move |value, _window, cx| {
                        let zoom = ui::slider_step(value, geom::MIN_ZOOM, geom::MAX_ZOOM);
                        weak.update(cx, |this, cx| this.set_zoom(zoom, cx)).ok();
                    }
                }),
            )
            .into_any_element()
    }

    /// Switch the active tool (the toolbar buttons). Always discards a
    /// pending rect-fill preview: a half-dragged rect must not survive into
    /// another tool's gesture, nor arm the next RectFill click with a stale
    /// anchor.
    fn set_tool(&mut self, tool: MapTool, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.session.tool = tool;
            open.session.rect_pending = None;
            open.session.sel_pending = None;
            cx.notify();
        }
    }

    #[cfg(test)]
    fn step_zoom(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            let next = geom::zoom_by(open.zoom, delta);
            if next != open.zoom {
                open.zoom = next;
                cx.notify();
            }
        }
    }

    /// Set the zoom outright (the slider), clamped to the ladder.
    fn set_zoom(&mut self, zoom: usize, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            let next = zoom.clamp(geom::MIN_ZOOM, geom::MAX_ZOOM);
            if next != open.zoom {
                open.zoom = next;
                cx.notify();
            }
        }
    }

    fn set_pal_sub(&mut self, pal_sub: u16, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            let next = pal_sub.clamp(geom::PAL_SUB_MIN, geom::PAL_SUB_MAX);
            if next != open.session.pal_sub {
                open.session.pal_sub = next;
                cx.notify();
            }
        }
    }

    /// The map canvas: the composed image at integer zoom, an optional cell
    /// grid, and the rect-fill drag preview. Left-drag paints with the
    /// active tool; middle-drag pans.
    fn render_canvas(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_canvas is only called in the Ready state");
        };
        let state = open.session.store.state();
        let scene = MapScene {
            image: open.image.clone(),
            pan: open.pan,
            zoom: open.zoom,
            cols: state.w,
            rows: state.h,
            grid: open.show_grid,
            // A select drag previews like a rect-fill; the settled
            // selection keeps drawing until cleared.
            rect: open.session.rect_pending.or(open.session.sel_pending),
            selection: open.session.selection,
            background: cx.theme().colors().editor_background,
            border: cx.theme().colors().border,
            accent: gpui::rgb(0xebcb8b).into(),
            selection_color: cx.theme().colors().border_focused,
        };
        let bounds_slot = open.canvas_bounds.clone();
        let element = gpui::canvas(
            move |canvas_bounds, _window, _cx| {
                *bounds_slot.borrow_mut() = Some(canvas_bounds);
                scene
            },
            move |canvas_bounds, scene, window, _cx| paint_map(&scene, canvas_bounds, window),
        )
        .size_full();

        div()
            .id("ggo-map-canvas")
            .size_full()
            .overflow_hidden()
            .child(element)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.canvas_primary_down(event.position, event.modifiers.shift, window, cx);
                }),
            )
            .on_hover(cx.listener(|this, hovered: &bool, _window, _cx| {
                if !hovered && let ViewerState::Ready(open) = &mut this.state {
                    open.hover_cell = None;
                }
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if let ViewerState::Ready(open) = &mut this.state {
                    open.hover_cell = open.cell_at(event.position);
                }
                if this.handle_pan_move(event, cx) {
                    return;
                }
                let Some((painting, cell)) = this
                    .ready_map()
                    .map(|open| (open.painting, open.cell_at(event.position)))
                else {
                    return;
                };
                if !painting {
                    return;
                }
                // The button came up outside the canvas, so no mouse-up
                // reached this element: end the gesture here instead of
                // painting a trail the next time the cursor passes over.
                if event.pressed_button != Some(MouseButton::Left) {
                    this.end_paint(cx);
                    return;
                }
                // A flood fill is a click, not a drag: refilling per move
                // would stack one undo entry per region crossed.
                if this
                    .ready_map()
                    .is_some_and(|open| open.session.tool == MapTool::Fill)
                {
                    return;
                }
                let Some(cell) = cell else { return };
                this.paint_at(cell, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _window, cx| this.end_paint(cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    if let ViewerState::Ready(open) = &mut this.state {
                        open.pan_drag = Some(PanDrag {
                            start_cursor: [
                                f32::from(event.position.x),
                                f32::from(event.position.y),
                            ],
                            start_pan: open.pan,
                        });
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseUpEvent, _window, _cx| {
                    if let ViewerState::Ready(open) = &mut this.state {
                        open.pan_drag = None;
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                let dy = f32::from(event.delta.pixel_delta(px(20.)).y);
                if dy == 0.0 {
                    return;
                }
                let Some(local) = this
                    .ready_map()
                    .and_then(|open| open.canvas_local(event.position))
                else {
                    return;
                };
                this.zoom_at_cursor(if dy > 0.0 { 1 } else { -1 }, local, cx);
                if let ViewerState::Ready(open) = &mut this.state {
                    open.hover_cell = open.cell_at(event.position);
                }
            }))
            .into_any_element()
    }

    /// Wheel zoom anchored on the cursor -- the same affordance
    /// `ggo_world_panel` gives its canvas, over this panel's integer zoom
    /// ladder. The pan adjustment is [`geom::zoom_at`].
    fn zoom_at_cursor(&mut self, delta: isize, cursor: [f32; 2], cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let next = geom::zoom_by(open.zoom, delta);
        if next == open.zoom {
            return;
        }
        open.pan = geom::zoom_at(open.pan, open.zoom, cursor, next);
        open.zoom = next;
        cx.notify();
    }

    /// Left-mouse down on the canvas at window-space `position`: start a
    /// gesture and dispatch it to the active tool. Its own method (rather
    /// than an inline listener body) so the gesture-start rules below are
    /// reachable from a test -- an element's `on_mouse_down` closure is
    /// not.
    fn canvas_primary_down(
        &mut self,
        position: gpui::Point<Pixels>,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Take focus so the panel's Undo/Redo/Save bindings apply (and any
        // in-progress resize edit stops winning the key context).
        window.focus(&self.focus_handle, cx);
        let Some(cell) = self.ready_map().and_then(|open| open.cell_at(position)) else {
            return;
        };
        if let ViewerState::Ready(open) = &mut self.state {
            open.painting = true;
            open.session.paint_erase = shift;
            open.session.sel_pending = None;
            // Everything this gesture paints folds into one undo entry.
            open.session.store.begin_stroke();
            // A NEW gesture starts from no pending rect (fix round 1,
            // BLOCKING 3). `paint_at`'s RectFill arm EXTENDS an existing
            // pending rect, so a rect whose release never reached this
            // element -- the button came up outside the canvas, or the
            // window lost focus mid-drag -- would otherwise survive and
            // turn the next single click into a fill from the abandoned
            // anchor to wherever you clicked. (ggo-ide's
            // cursor-leaves-canvas arm commits the pending rect instead;
            // committing a gesture the user walked away from would be its
            // own surprise, so this discards.)
            open.session.rect_pending = None;
        }
        self.paint_at(cell, cx);
    }

    fn ready_map(&self) -> Option<&OpenMap> {
        match &self.state {
            ViewerState::Ready(open) => Some(open),
            _ => None,
        }
    }

    /// Middle-mouse pan handling for a move event. Returns true if the
    /// event belonged to an in-flight pan (handled or cancelled).
    fn handle_pan_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
            return false;
        };
        let Some(drag) = open.pan_drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Middle) {
            open.pan_drag = None;
            return true;
        }
        open.pan = [
            drag.start_pan[0] + f32::from(event.position.x) - drag.start_cursor[0],
            drag.start_pan[1] + f32::from(event.position.y) - drag.start_cursor[1],
        ];
        cx.notify();
        true
    }

    /// The bound tileset's tiles, rect-selectable as a stamp.
    fn render_strip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_strip is only called in the Ready state");
        };
        let Some(tileset) = &open.session.tileset else {
            return div()
                .h(STRIP_HEIGHT)
                .p_1()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new(
                        open.session
                            .tileset_error
                            .clone()
                            .unwrap_or_else(|| "No tileset bound".to_string()),
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .into_any_element();
        };
        let (w, h) =
            geom::grid_pixel_size(tileset.cols as u16, tileset.rows() as u16, geom::STRIP_ZOOM);
        let scene = StripScene {
            image: open.session.strip.clone(),
            sel: palette_sel_rect(open.session.pal_anchor, open.session.pal_far),
            accent: gpui::rgb(0xebcb8b).into(),
            background: cx.theme().colors().editor_background,
        };
        let bounds_slot = open.strip_bounds.clone();
        let element = gpui::canvas(
            move |canvas_bounds, _window, _cx| {
                *bounds_slot.borrow_mut() = Some(canvas_bounds);
                scene
            },
            move |canvas_bounds, scene, window, _cx| paint_strip(&scene, canvas_bounds, window),
        )
        .w(px(w))
        .h(px(h));

        div()
            .id("ggo-map-strip")
            .h(STRIP_HEIGHT)
            .overflow_scroll()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(element)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.strip_primary_down(event.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                this.strip_drag_move(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _window, _cx| {
                    this.strip_primary_up();
                }),
            )
            .into_any_element()
    }

    /// Strip primary-down at window-space `position`: arm a drag-select
    /// anchored on the hit tile. A miss -- off the strip, or in the sheet's
    /// zero-filled partial-row padding -- arms nothing.
    fn strip_primary_down(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(cell) = self
            .ready_map()
            .and_then(|open| open.strip_cell_at(position))
        else {
            return;
        };
        if let ViewerState::Ready(open) = &mut self.state {
            open.pal_dragging = true;
            open.session.pal_anchor = cell;
            open.session.pal_far = cell;
            cx.notify();
        }
    }

    /// Extend an in-flight strip drag-select to the tile under the cursor.
    /// The drag ends when the left button is no longer held (it came up
    /// outside the strip); a miss while dragging leaves the selection where
    /// it was rather than smearing it to the edge.
    fn strip_drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some((dragging, cell)) = self
            .ready_map()
            .map(|open| (open.pal_dragging, open.strip_cell_at(event.position)))
        else {
            return;
        };
        if !dragging {
            return;
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            open.pal_dragging = false;
            return;
        }
        if let Some(cell) = cell {
            open.session.pal_far = cell;
            cx.notify();
        }
    }

    /// Release over the strip: the drag-select is finished, keeping its
    /// selection.
    fn strip_primary_up(&mut self) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.pal_dragging = false;
        }
    }

    /// The bind picker + the resize fields.
    fn render_footer(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_footer is only called in the Ready state");
        };
        let bound = open.session.store.state().til_path;
        let candidates = open.tilesets.clone();
        let weak = cx.weak_entity();
        let tileset_menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for til in candidates {
                let weak = weak.clone();
                let label = til.clone();
                menu = menu.entry(SharedString::from(label), None, move |_window, cx| {
                    let til = til.clone();
                    weak.update(cx, |this, cx| this.bind_tileset(til, cx)).ok();
                });
            }
            menu
        });
        let label = if bound.is_empty() {
            "Bind tileset…".to_string()
        } else {
            bound
        };
        let resize = open.resize.as_ref();
        h_flex()
            .gap_1()
            .p_1()
            .flex_wrap()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(DropdownMenu::new("ggo-map-bind", label, tileset_menu))
            .child(div().flex_1())
            .children(resize.map(|fields| Self::size_field("W", &fields.w, cx)))
            .children(resize.map(|fields| Self::size_field("H", &fields.h, cx)))
            .child(
                Button::new("ggo-map-resize", "Resize")
                    .on_click(cx.listener(|this, _, window, cx| this.resize_impl(window, cx))),
            )
            .children(open.session.save_error.as_ref().map(|e| {
                ggo_common::CopyableText::new(
                    "ggo-map-save-error-copy",
                    format!("save failed: {e}"),
                )
                .size(LabelSize::Small)
            }))
            .children(open.session.tileset_error.as_ref().map(|e| {
                ggo_common::CopyableText::new("ggo-map-tileset-error-copy", e.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            // Worth saying out loud, same as `ggo_tileset_panel` says it:
            // with no readable `.pal`, the colors on screen are worldlib's
            // 16-gray fallback, not the asset's own.
            .children(
                open.session
                    .tileset
                    .as_ref()
                    .filter(|ts| ts.missing_pal)
                    .map(|_| {
                        Label::new("no .pal — 16-gray fallback")
                            .size(LabelSize::XSmall)
                            .color(Color::Warning)
                    }),
            )
            .into_any_element()
    }

    /// One labelled resize input, in the minimal bordered box the world
    /// panel's inspector fields use (primitive gpui/ui components only --
    /// no widget framework).
    fn size_field(label: &str, editor: &Entity<Editor>, cx: &Context<Self>) -> gpui::AnyElement {
        h_flex()
            .gap_0p5()
            .items_center()
            .child(
                Label::new(label.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .w(px(44.))
                    .px_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .child(editor.clone()),
            )
            .into_any_element()
    }

    // ------------------------------------------------------ test hooks

    /// A loaded map is on screen. `test-support` only, for `ggo_smoke`'s
    /// map journeys -- read-only, like every hook below it.
    #[cfg(feature = "test-support")]
    pub fn test_is_ready(&self) -> bool {
        matches!(self.state, ViewerState::Ready(_))
    }

    /// The open map has unsaved edits. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_is_dirty(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.session.store.dirty())
    }

    /// The map canvas's on-screen bounds, recorded at the last prepaint --
    /// `None` before the first Ready paint. Cell `(x, y)`'s centre is
    /// `origin + ((x + 0.5) * TILE_PX * zoom, (y + 0.5) * TILE_PX * zoom)`
    /// while the pan is at the origin. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_canvas_bounds(&self) -> Option<Bounds<Pixels>> {
        match &self.state {
            ViewerState::Ready(open) => *open.canvas_bounds.borrow(),
            _ => None,
        }
    }

    /// The canvas's integer zoom, i.e. how many screen px one map pixel
    /// occupies ([`geom::DEFAULT_ZOOM`] until the user zooms).
    /// `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_zoom(&self) -> usize {
        match &self.state {
            ViewerState::Ready(open) => open.zoom,
            _ => geom::DEFAULT_ZOOM,
        }
    }

    /// The PACKED cell at `(x, y)` -- `map_doc::pack_cell`'s `u16`, so a
    /// painted cell reads back as `pack_cell(tile, pal_sub, hflip, vflip)`
    /// and an unpainted one as the blank sentinel
    /// [`ggo_worldlib::sprites::map_doc::CELL_BLANK`] (`0x03FF`, which is
    /// also the tile-index mask -- blank is NOT zero). `None` outside the
    /// map or before it loads. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_cell(&self, x: usize, y: usize) -> Option<u16> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let state = open.session.store.state();
        (x < state.w as usize && y < state.h as usize)
            .then(|| state.cells.get(y * state.w as usize + x).copied())
            .flatten()
    }

    /// The asset-root-relative `.til` the open map is bound to (`""` when
    /// it is unbound). `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_til_path(&self) -> Option<String> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.session.store.state().til_path),
            _ => None,
        }
    }

    /// The settled cell selection as normalized inclusive corners
    /// `(x0, y0, x1, y1)`, or `None` when nothing is selected (which is
    /// what `escape` leaves behind, and what makes `delete` a no-op).
    /// `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_selection(&self) -> Option<(i32, i32, i32, i32)> {
        match &self.state {
            ViewerState::Ready(open) => open.session.selection,
            _ => None,
        }
    }

    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let header = self.render_header(cx);
        let tools = self.render_tools(cx);
        let terrains = self.render_terrains(cx);
        let canvas = self.render_canvas(cx);
        let strip = self.render_strip(cx);
        let footer = self.render_footer(window, cx);
        v_flex()
            .size_full()
            .child(header)
            .child(tools)
            .children(terrains)
            .child(div().flex_1().min_h_0().child(canvas))
            .child(strip)
            .child(footer)
            .into_any_element()
    }
}

// --------------------------------------------------------------- painting

/// Everything the map canvas's paint closure needs, captured at render
/// time.
struct MapScene {
    image: Option<Arc<RenderImage>>,
    pan: [f32; 2],
    zoom: usize,
    cols: u16,
    rows: u16,
    grid: bool,
    rect: Option<(i32, i32, i32, i32)>,
    /// The settled cell selection, drawn in the focus colour.
    selection: Option<(i32, i32, i32, i32)>,
    background: Hsla,
    border: Hsla,
    accent: Hsla,
    selection_color: Hsla,
}

struct StripScene {
    image: Option<Arc<RenderImage>>,
    sel: (i32, i32, i32, i32),
    accent: Hsla,
    background: Hsla,
}

/// Rect covering cells `[c0..=c1] x [r0..=r1]` in canvas space.
fn cell_rect(
    canvas: Bounds<Pixels>,
    pan: [f32; 2],
    zoom: usize,
    c0: i32,
    r0: i32,
    c1: i32,
    r1: i32,
) -> Bounds<Pixels> {
    let step = (ggo_worldlib::sprites::tileset_doc::TILE_PX * zoom.max(1)) as f32;
    let x = canvas.origin.x + px(pan[0] + c0 as f32 * step);
    let y = canvas.origin.y + px(pan[1] + r0 as f32 * step);
    bounds(
        point(x, y),
        size(
            px((c1 - c0 + 1) as f32 * step),
            px((r1 - r0 + 1) as f32 * step),
        ),
    )
}

fn paint_map(scene: &MapScene, canvas: Bounds<Pixels>, window: &mut Window) {
    window.with_content_mask(Some(ContentMask { bounds: canvas }), |window| {
        window.paint_quad(fill(canvas, scene.background));
        if scene.cols == 0 || scene.rows == 0 {
            return;
        }
        let map_bounds = cell_rect(
            canvas,
            scene.pan,
            scene.zoom,
            0,
            0,
            scene.cols as i32 - 1,
            scene.rows as i32 - 1,
        );
        if let Some(image) = &scene.image {
            let _ = window.paint_image(
                map_bounds,
                map_bounds,
                Corners::default(),
                image.clone(),
                0,
                false,
                true,
            );
        }
        window.paint_quad(outline(map_bounds, scene.border, BorderStyle::default()));
        if scene.grid {
            let step = (ggo_worldlib::sprites::tileset_doc::TILE_PX * scene.zoom.max(1)) as f32;
            for c in 1..scene.cols as i32 {
                window.paint_quad(fill(
                    bounds(
                        point(
                            map_bounds.origin.x + px(c as f32 * step),
                            map_bounds.origin.y,
                        ),
                        size(px(1.), map_bounds.size.height),
                    ),
                    scene.border,
                ));
            }
            for r in 1..scene.rows as i32 {
                window.paint_quad(fill(
                    bounds(
                        point(
                            map_bounds.origin.x,
                            map_bounds.origin.y + px(r as f32 * step),
                        ),
                        size(map_bounds.size.width, px(1.)),
                    ),
                    scene.border,
                ));
            }
        }
        if let Some((x0, y0, x1, y1)) = scene.rect {
            let r = cell_rect(
                canvas,
                scene.pan,
                scene.zoom,
                x0.min(x1),
                y0.min(y1),
                x0.max(x1),
                y0.max(y1),
            );
            window.paint_quad(outline(r, scene.accent, BorderStyle::default()));
        }
        if let Some((x0, y0, x1, y1)) = scene.selection {
            let r = cell_rect(canvas, scene.pan, scene.zoom, x0, y0, x1, y1);
            window.paint_quad(outline(r, scene.selection_color, BorderStyle::default()));
        }
    });
}

fn paint_strip(scene: &StripScene, canvas: Bounds<Pixels>, window: &mut Window) {
    window.with_content_mask(Some(ContentMask { bounds: canvas }), |window| {
        window.paint_quad(fill(canvas, scene.background));
        if let Some(image) = &scene.image {
            let _ = window.paint_image(
                canvas,
                canvas,
                Corners::default(),
                image.clone(),
                0,
                false,
                true,
            );
        }
        let (c0, r0, c1, r1) = scene.sel;
        let r = cell_rect(canvas, [0.0, 0.0], geom::STRIP_ZOOM, c0, r0, c1, r1);
        window.paint_quad(outline(r, scene.accent, BorderStyle::default()));
    });
}

impl Render for MapPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_resize_fields(window, cx);
        if let ViewerState::Ready(open) = &mut self.state
            && open.terrain_name.is_none()
        {
            open.terrain_name = Some(cx.new(|cx| Editor::single_line(window, cx)));
        }
        let body = match &self.state {
            ViewerState::Empty => self.render_message(EMPTY_MESSAGE.to_string(), cx),
            ViewerState::Loading { rel_path } => {
                self.render_message(format!("Loading {rel_path}…"), cx)
            }
            ViewerState::Error(e) => self.render_load_error(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(window, cx),
        };
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Undo, _window, cx| this.undo_impl(cx)))
            .on_action(cx.listener(|this, _: &Redo, _window, cx| this.redo_impl(cx)))
            .on_action(cx.listener(|this, _: &Save, _window, cx| this.save_impl(cx)))
            .on_action(cx.listener(|this, _: &Copy, _window, cx| this.copy_impl(cx)))
            .on_action(cx.listener(|this, _: &Paste, _window, cx| this.paste_impl(cx)))
            .on_action(
                cx.listener(|this, _: &DeleteSelection, _window, cx| {
                    this.delete_selection_impl(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ClearSelection, _window, cx| this.clear_selection_impl(cx)),
            )
            .on_action(
                cx.listener(|this, _: &ApplyResize, window, cx| this.resize_impl(window, cx)),
            )
            .bg(cx.theme().colors().panel_background)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for MapPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::map_doc::{MapState, pack_cell};
    use ggo_worldlib::sprites::palette565::PAL_SLOTS;
    use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, WorktreeId};
    use workspace::{AppState, MultiWorkspace};

    /// The fixture tileset's tile count. 4 is deliberately not the 8-column
    /// fallback, so the strip lays out at exactly 4x1 and the stamp
    /// indexing is unambiguous in the assertions below.
    const FIXTURE_TILES: usize = 4;
    /// The second fixture tileset: 9 tiles, so `grid_cols` gives it the
    /// 8-column fallback across 2 rows -- a different shape from
    /// `world.til`'s 4x1.
    const WIDE_TILES: usize = 9;
    const FIXTURE_W: u16 = 4;
    const FIXTURE_H: u16 = 3;

    /// A real-fs emerald project: `emerald.toml` at the root, everything
    /// else under `assets/`.
    ///
    /// The layout is the point of the fixture, not decoration: the asset
    /// root is `<root>/assets`, so the `.map`'s `til_path` must read
    /// `tiles/world.til` with NO `assets/` segment (the F4 `ggo-sprfix`
    /// contract). `fixture_writes_assets_root_relative_sidecars` asserts
    /// exactly that, so a regression in the fixture itself can't quietly
    /// make the panel's own tests meaningless.
    pub(crate) fn write_project(root: &Path) -> PathBuf {
        std::fs::write(root.join(ggo_common::EMERALD_MANIFEST), "[project]\n").unwrap();
        let assets = root.join(ASSETS_DIR);
        std::fs::create_dir_all(assets.join("tiles")).unwrap();
        std::fs::create_dir_all(assets.join("maps")).unwrap();

        // Tile N is solid palette index N, so a composed cell's color
        // identifies which tile landed in it.
        let mut indices = vec![0u8; FIXTURE_TILES * TILE_PIXELS];
        for (t, chunk) in indices.chunks_exact_mut(TILE_PIXELS).enumerate() {
            chunk.fill(t as u8);
        }
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800;
        palette[2] = 0x07E0;
        palette[3] = 0x001F;
        io::save_tileset(
            &assets,
            "tiles/world.til",
            &indices,
            FIXTURE_TILES,
            &palette,
        )
        .unwrap();

        // A SECOND tileset with a different tile count AND a different
        // column count (9 tiles -> the 8-wide fallback, 2 rows), so a
        // rebind changes the stamp coordinate system and not just the
        // pixels -- which is what makes the bind-undo resync observable.
        io::save_tileset(
            &assets,
            "tiles/wide.til",
            &vec![1u8; WIDE_TILES * TILE_PIXELS],
            WIDE_TILES,
            &palette,
        )
        .unwrap();

        write_map(&assets, "maps/level.map", blank_cells());
        write_map(&assets, "maps/other.map", blank_cells());
        assets
    }

    fn blank_cells() -> Vec<u16> {
        vec![CELL_BLANK; FIXTURE_W as usize * FIXTURE_H as usize]
    }

    fn write_map(assets: &Path, rel: &str, cells: Vec<u16>) {
        io::save_map(
            assets,
            rel,
            &MapState {
                w: FIXTURE_W,
                h: FIXTURE_H,
                cells,
                til_path: "tiles/world.til".to_string(),
                pal_path: "tiles/world.pal".to_string(),
                dirty: false,
            },
        )
        .unwrap();
    }

    fn new_panel(cx: &mut TestAppContext, root: &Path) -> Entity<MapPanel> {
        let root = root.to_path_buf();
        cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = MapPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        })
    }

    /// Load the fixture map into a fresh panel and return it Ready.
    async fn ready_panel(cx: &mut TestAppContext, root: &Path) -> Entity<MapPanel> {
        write_project(root);
        let panel = new_panel(cx, root);
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/level.map", cx);
        });
        cx.executor().run_until_parked();
        panel
    }

    fn ready(panel: &MapPanel) -> &OpenMap {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
    }

    fn cells(panel: &MapPanel) -> Vec<u16> {
        ready(panel).session.store.state().cells
    }

    // --------------------------------------------------------- pure rules

    /// The asset root is derived on disk (a `.map` has no `worlds/`-style
    /// anchor in its path), and the map's rel is relative to THAT -- so a
    /// sidecar written from it can never carry an `assets/` segment.
    /// Outside an emerald project's `assets/` tree, the worktree root
    /// stands in and the rel passes through unchanged.
    #[test]
    fn split_map_path_derives_the_asset_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let assets = write_project(root);

        assert_eq!(
            split_map_path(root, "assets/maps/level.map"),
            (assets.clone(), "maps/level.map".to_string())
        );
        assert_eq!(
            split_map_path(root, "assets/level.map"),
            (assets, "level.map".to_string())
        );
        // Inside the project but OUTSIDE assets/: no asset root applies.
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        assert_eq!(
            split_map_path(root, "scratch/level.map"),
            (root.to_path_buf(), "scratch/level.map".to_string())
        );
        // Not an emerald project at all.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            split_map_path(bare.path(), "a.map"),
            (bare.path().to_path_buf(), "a.map".to_string())
        );
    }

    /// Guards the fixture itself: the `.map` on disk names its tileset
    /// asset-root-relative, which is the contract every downstream reader
    /// (and `ggo-sprfix`) depends on.
    #[test]
    fn fixture_writes_assets_root_relative_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let assets = write_project(dir.path());
        let map = io::open_map(&assets, "maps/level.map").unwrap();
        assert_eq!(map.til_path, "tiles/world.til");
        assert_eq!(map.pal_path, "tiles/world.pal");
        assert!(
            !map.til_path.contains(ASSETS_DIR),
            "a sidecar rel must never carry an assets/ segment"
        );
    }

    /// [`map_rel`]'s stem rules: the same refusals the tileset stem makes,
    /// a retyped `.map` extension accepted without doubling, and the
    /// clicked dir joined in.
    #[test]
    fn map_rel_applies_the_stem_rules() {
        assert_eq!(
            map_rel("assets/maps", "level"),
            Ok("assets/maps/level.map".to_string())
        );
        assert_eq!(
            map_rel("assets/maps", "level.map"),
            Ok("assets/maps/level.map".to_string())
        );
        assert_eq!(map_rel("", "level"), Ok("level.map".to_string()));
        for bad in &["", "  ", "a/b", "a\\b", ".", "..", ".map"] {
            assert!(
                map_rel("assets/maps", bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn is_assets_dir_accepts_the_root_and_its_children_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let assets = write_project(root);
        assert!(is_assets_dir(&assets));
        assert!(is_assets_dir(&assets.join("maps")));
        assert!(
            !is_assets_dir(root),
            "the project root is not the asset root"
        );
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        assert!(!is_assets_dir(&root.join("scratch")));
    }

    // ------------------------------------------------------- registration

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
        ggo_common::bind_default_keymap(cx);
    }

    // --------------------------------------------------------------- load

    /// End-to-end load against a real-fs temp project: the asset root is
    /// derived, the doc reaches the store at its stored size, the bound
    /// tileset resolves, and both surfaces compose.
    #[gpui::test]
    async fn test_open_map_reaches_ready_with_a_composed_image(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);

        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/maps/level.map");
            assert_eq!(open.session.rel_path, "maps/level.map");
            assert_eq!(open.session.root, assets);
            let state = open.session.store.state();
            assert_eq!((state.w, state.h), (FIXTURE_W, FIXTURE_H));
            assert!(!state.dirty);
            let tileset = open
                .session
                .tileset
                .as_ref()
                .expect("the fixture binds a tileset");
            assert_eq!(tileset.tile_count, FIXTURE_TILES);
            assert_eq!(tileset.cols, FIXTURE_TILES, "4 tiles lay out in one row");
            assert_eq!(tileset.rows(), 1);
            assert!(!tileset.missing_pal);
            assert!(open.session.tileset_error.is_none());
            assert!(open.image.is_some(), "the map must compose");
            assert!(open.session.strip.is_some(), "the strip must compose");
            assert_eq!(
                open.tilesets,
                vec!["tiles/wide.til".to_string(), "tiles/world.til".to_string()],
                "the bind picker offers every .til under the asset root"
            );
            assert_eq!(open.zoom, geom::DEFAULT_ZOOM);
            assert_eq!(open.session.tool, MapTool::Brush);
        });
    }

    /// A malformed `.map` lands in Error, not a panic.
    #[gpui::test]
    async fn test_a_malformed_map_reports_an_error(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        std::fs::write(dir.path().join("assets/maps/bad.map"), b"not a map").unwrap();
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/bad.map", cx);
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(matches!(&panel.state, ViewerState::Error(_)));
        });
    }

    /// The view-layer glue the pure hit-test feeds: a WINDOW position maps
    /// through the recorded canvas bounds, the pan and the zoom onto a
    /// cell -- and off the map is a miss, not a clamped edge cell.
    #[gpui::test]
    async fn test_cell_at_maps_a_window_position_through_pan_and_zoom(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready")
            };
            *open.canvas_bounds.borrow_mut() =
                Some(bounds(point(px(20.), px(10.)), size(px(400.), px(300.))));
            open.zoom = 2;
            open.pan = [8.0, 4.0];
            let step = 32.0; // TILE_PX * zoom
            let at = |x: f32, y: f32| open.cell_at(point(px(x), px(y)));
            assert_eq!(at(20.0 + 8.0, 10.0 + 4.0), Some((0, 0)));
            assert_eq!(at(20.0 + 8.0 + step, 10.0 + 4.0 + step), Some((1, 1)));
            assert_eq!(at(20.0, 10.0), None, "left of the panned grid is a miss");
            assert_eq!(
                at(20.0 + 8.0 + step * FIXTURE_W as f32, 10.0 + 4.0),
                None,
                "past the right edge is a miss"
            );
        });
    }

    // -------------------------------------------------------------- tools

    /// Brush places the current stamp and undo restores the cells --
    /// through `MapOp::Brush`, which is worldlib's tested op; what this
    /// pins is the panel routing the gesture to it.
    #[gpui::test]
    async fn test_brush_places_a_cell_and_undo_restores_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let before = cells(panel);
            panel.paint_at((1, 1), cx);
            let after = cells(panel);
            assert_ne!(before, after);
            assert_eq!(
                after[FIXTURE_W as usize + 1],
                pack_cell(0, 0, false, false),
                "the default 1x1 stamp is tile 0, unflipped, palSub 0"
            );
            assert!(ready(panel).session.store.dirty());
            panel.undo_impl(cx);
            assert_eq!(cells(panel), before, "undo must restore the cells");
            assert!(!ready(panel).session.store.dirty());
        });
    }

    /// A rect drag over the strip selects a multi-tile stamp, and a brush
    /// then lays that whole rectangle down in one op.
    #[gpui::test]
    async fn test_a_multi_tile_stamp_from_the_strip_paints_a_block(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready")
            };
            // Drag-select tiles 1..=2 out of the 4x1 strip, with both flips
            // and a palSub, so the packed cells carry all of it.
            open.session.pal_anchor = (1, 0);
            open.session.pal_far = (2, 0);
            open.session.hflip = true;
            open.session.vflip = true;
            open.session.pal_sub = 5;
            let stamp = open.session.current_stamp();
            assert_eq!((stamp.w, stamp.h), (2, 1));
            assert_eq!(
                stamp.cells,
                vec![pack_cell(1, 5, true, true), pack_cell(2, 5, true, true)]
            );

            panel.paint_at((0, 0), cx);
            let after = cells(panel);
            assert_eq!(after[0], pack_cell(1, 5, true, true));
            assert_eq!(after[1], pack_cell(2, 5, true, true));
            assert_eq!(after[2], CELL_BLANK, "the stamp is 2 wide, not 3");
        });
    }

    /// Rect fill commits ONCE, on release, over the dragged rectangle --
    /// and undo takes the whole rectangle back in one step.
    #[gpui::test]
    async fn test_rect_fill_commits_on_release_and_undo_restores(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let before = cells(panel);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.session.tool = MapTool::RectFill;
                open.painting = true;
            }
            panel.paint_at((0, 0), cx);
            panel.paint_at((2, 1), cx); // drag extends the pending rect
            assert_eq!(
                cells(panel),
                before,
                "a rect-fill drag must not touch the document until release"
            );
            assert_eq!(ready(panel).session.rect_pending, Some((0, 0, 2, 1)));

            panel.end_paint(cx);
            let after = cells(panel);
            assert!(ready(panel).session.rect_pending.is_none());
            let filled = pack_cell(0, 0, false, false);
            for y in 0..2usize {
                for x in 0..3usize {
                    assert_eq!(after[y * FIXTURE_W as usize + x], filled, "({x},{y})");
                }
            }
            assert_eq!(after[2 * FIXTURE_W as usize], CELL_BLANK, "row 2 untouched");

            panel.undo_impl(cx);
            assert_eq!(
                cells(panel),
                before,
                "one undo must take the whole rectangle back"
            );
        });
    }

    /// Erase blanks a painted cell; undo brings it back.
    #[gpui::test]
    async fn test_erase_blanks_a_cell_and_undo_restores(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.paint_at((2, 0), cx);
            let painted = cells(panel);
            assert_ne!(painted[2], CELL_BLANK);

            if let ViewerState::Ready(open) = &mut panel.state {
                open.session.tool = MapTool::Eraser;
            }
            panel.paint_at((2, 0), cx);
            assert_eq!(cells(panel)[2], CELL_BLANK);

            panel.undo_impl(cx);
            assert_eq!(cells(panel), painted, "undo must restore the erased cell");
        });
    }

    /// Eyedropper adopts the picked cell's flips and palSub and moves the
    /// strip selection onto its source tile -- and a BLANK cell leaves the
    /// selection alone (worldlib's own rule, ported from ggo-ide).
    #[gpui::test]
    async fn test_eyedropper_adopts_the_cell_and_moves_the_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        let assets = dir.path().join(ASSETS_DIR);
        let mut seeded = blank_cells();
        seeded[1] = pack_cell(2, 5, true, false);
        write_map(&assets, "maps/level.map", seeded);
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/level.map", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.session.tool = MapTool::Eyedropper;
            }
            panel.paint_at((1, 0), cx);
            let open = ready(panel);
            assert_eq!(open.session.pal_sub, 5);
            assert!(open.session.hflip);
            assert!(!open.session.vflip);
            assert_eq!(
                open.session.pal_anchor,
                (2, 0),
                "tile 2 in a 4-column strip"
            );
            assert_eq!(open.session.pal_far, (2, 0));
            assert!(
                !open.session.store.dirty(),
                "picking must not edit the document"
            );

            // A blank cell keeps the flips it reads (all false) but leaves
            // the tile selection where it was.
            panel.paint_at((3, 2), cx);
            let open = ready(panel);
            assert_eq!(
                open.session.pal_anchor,
                (2, 0),
                "a blank cell has no source tile"
            );
            assert_eq!(open.session.pal_sub, 0);
        });
    }

    /// With nothing bound there is no tile pool, so a cell index would mean
    /// nothing: painting is inert rather than writing unresolvable indices.
    #[gpui::test]
    async fn test_painting_an_unbound_map_is_inert(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        let assets = dir.path().join(ASSETS_DIR);
        io::save_new_map(&assets, "maps/fresh.map", 8, 8).unwrap();
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/fresh.map", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            assert!(ready(panel).session.tileset.is_none());
            panel.paint_at((0, 0), cx);
            assert!(
                !ready(panel).session.store.dirty(),
                "painting without a tileset must not dirty the document"
            );
        });
    }

    /// Binding resolves the tileset FIRST and applies `BindTileset` only on
    /// success; a binding that can't be opened leaves the document clean
    /// and reports why.
    #[gpui::test]
    async fn test_bind_tileset_applies_the_op_only_on_success(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        let assets = dir.path().join(ASSETS_DIR);
        io::save_new_map(&assets, "maps/fresh.map", 8, 8).unwrap();
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/fresh.map", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.bind_tileset("tiles/nope.til".to_string(), cx);
            assert!(ready(panel).session.tileset.is_none());
            assert!(ready(panel).session.tileset_error.is_some());
            assert!(
                !ready(panel).session.store.dirty(),
                "a failed bind must not touch the document"
            );

            panel.bind_tileset("tiles/world.til".to_string(), cx);
            let open = ready(panel);
            assert!(open.session.tileset_error.is_none());
            assert_eq!(open.session.store.state().til_path, "tiles/world.til");
            assert_eq!(
                open.session.store.state().pal_path,
                "tiles/world.pal",
                "the .pal rel comes from worldlib's own pairing rule"
            );
            assert!(open.session.store.dirty(), "a successful bind is an edit");
            assert_eq!(
                open.session.tileset.as_ref().map(|ts| ts.tile_count),
                Some(FIXTURE_TILES),
                "the cache holds the tileset that was just bound"
            );
            assert!(open.session.strip.is_some(), "and recomposes the strip");

            panel.undo_impl(cx);
            let open = ready(panel);
            assert_eq!(open.session.store.state().til_path, "");
            assert!(
                open.session.tileset.is_none() && open.session.strip.is_none(),
                "the CACHE must unbind with the store too"
            );
        });
    }

    /// **Regression, fix round 1 BLOCKING 1a.** Undoing a bind must clear
    /// the panel's cached tileset, not just the store's `til_path`.
    ///
    /// With the cache left behind, `paint_at`'s `tileset.is_none()` gate
    /// PASSED on an unbound document -- so you could paint cell indices
    /// into a map with nothing to resolve them against and save it, the
    /// exact artifact `new_map`'s unbound-by-design rationale exists to
    /// prevent. Painting after the undo is the load-bearing assertion; the
    /// cache assertions say why.
    #[gpui::test]
    async fn test_undo_of_a_bind_unbinds_the_panel_too(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        let assets = dir.path().join(ASSETS_DIR);
        io::save_new_map(&assets, "maps/fresh.map", 8, 8).unwrap();
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/fresh.map", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.bind_tileset("tiles/world.til".to_string(), cx);
            assert!(ready(panel).session.tileset.is_some());
            assert!(ready(panel).session.strip.is_some());
            assert!(ready(panel).image.is_some());

            panel.undo_impl(cx);
            let open = ready(panel);
            assert_eq!(open.session.store.state().til_path, "", "the store unbinds");
            assert!(
                open.session.tileset.is_none(),
                "and the panel's cached tileset must unbind WITH it"
            );
            assert!(open.session.strip.is_none(), "the strip is gone too");
            assert!(open.image.is_none(), "and there is nothing to compose");
            assert!(open.session.tileset_error.is_some(), "with a reason shown");

            let before = cells(panel);
            panel.paint_at((0, 0), cx);
            assert_eq!(
                cells(panel),
                before,
                "painting an unbound map must be inert again after the undo"
            );
            assert!(!ready(panel).session.store.dirty());
        });
    }

    /// **Regression, fix round 1 BLOCKING 1b.** Undoing a REBIND must put
    /// the previous tileset back, not leave the newer one cached.
    ///
    /// The two fixtures differ in tile count AND column count (4 tiles/4
    /// cols vs 9 tiles/8 cols), so a stale cache would leave the canvas,
    /// the strip and `build_stamp`'s `row * cols + col` all computed
    /// against the wrong sheet. The stamp assertion is the load-bearing
    /// one: strip cell (0,1) is tile 8 under an 8-wide sheet and does not
    /// exist at all under a 4-wide, 4-tile one.
    #[gpui::test]
    async fn test_undo_of_a_rebind_restores_the_previous_tileset(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // The fixture opens bound to world.til (4 tiles, 4 cols).
            assert_eq!(
                ready(panel).session.tileset.as_ref().unwrap().cols,
                FIXTURE_TILES
            );

            panel.bind_tileset("tiles/wide.til".to_string(), cx);
            let open = ready(panel);
            assert_eq!(open.session.store.state().til_path, "tiles/wide.til");
            assert_eq!(
                open.session.tileset.as_ref().unwrap().tile_count,
                WIDE_TILES
            );
            assert_eq!(open.session.tileset.as_ref().unwrap().cols, 8);
            assert_eq!(open.session.tileset.as_ref().unwrap().rows(), 2);

            panel.undo_impl(cx);
            let open = ready(panel);
            assert_eq!(open.session.store.state().til_path, "tiles/world.til");
            let tileset = open
                .session
                .tileset
                .as_ref()
                .expect("undoing a rebind rebinds the previous tileset");
            assert_eq!(
                (tileset.tile_count, tileset.cols),
                (FIXTURE_TILES, FIXTURE_TILES),
                "the cached tileset must follow the store back"
            );
            assert!(
                open.session.strip.is_some(),
                "and the strip recomposes for it"
            );

            // The stamp coordinate system followed too: a selection is
            // resolved against 4 tiles at 4 cols, so row 1 is off the end
            // of the sheet and packs blank rather than naming tile 8.
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready")
            };
            open.session.pal_anchor = (0, 1);
            open.session.pal_far = (0, 1);
            assert_eq!(
                open.session.current_stamp().cells,
                vec![CELL_BLANK],
                "row 1 does not exist in a 4-tile, 4-column sheet"
            );

            // Redo puts the wide sheet back, cache included.
            panel.redo_impl(cx);
            let open = ready(panel);
            assert_eq!(open.session.store.state().til_path, "tiles/wide.til");
            assert_eq!(
                open.session.tileset.as_ref().unwrap().tile_count,
                WIDE_TILES
            );
        });
    }

    /// **Regression, fix round 1 BLOCKING 3.** An abandoned rect-fill must
    /// not arm the NEXT click.
    ///
    /// `paint_at`'s RectFill arm extends an existing pending rect, so a
    /// gesture whose release never reached the canvas left `rect_pending`
    /// alive: a subsequent one-cell click became a fill from the stale
    /// anchor. Probed shape -- press at (0,0), gesture abandoned, click at
    /// (3,2) -- filled 12 cells instead of 1. Driven through the REAL
    /// mouse-down entry point, which is where the fix lives.
    #[gpui::test]
    async fn test_an_abandoned_rect_fill_does_not_arm_the_next_click(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready")
            };
            open.session.tool = MapTool::RectFill;
            open.zoom = 1;
            open.pan = [0.0, 0.0];
            *open.canvas_bounds.borrow_mut() =
                Some(bounds(point(px(0.), px(0.)), size(px(400.), px(300.))));
        });

        let step = ggo_worldlib::sprites::tileset_doc::TILE_PX as f32;
        let press = |x: i32, y: i32, cx: &mut gpui::VisualTestContext| {
            let position = point(
                px(x as f32 * step + step / 2.0),
                px(y as f32 * step + step / 2.0),
            );
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.canvas_primary_down(position, false, window, cx)
                })
            });
        };

        // Gesture 1: press at (0,0), then abandoned -- no mouse-up ever
        // reaches the canvas, so `rect_pending` stays armed.
        press(0, 0, cx);
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).session.rect_pending, Some((0, 0, 0, 0)));
        });

        // Gesture 2: a fresh one-cell click at (3,2), then its release.
        press(3, 2, cx);
        panel.update(cx, |panel, cx| {
            assert_eq!(
                ready(panel).session.rect_pending,
                Some((3, 2, 3, 2)),
                "a new press must start a NEW rect, not extend the abandoned one"
            );
            panel.end_paint(cx);
            let filled = pack_cell(0, 0, false, false);
            let after = cells(panel);
            assert_eq!(
                after.iter().filter(|&&c| c == filled).count(),
                1,
                "a one-cell click must fill exactly one cell"
            );
            assert_eq!(after[2 * FIXTURE_W as usize + 3], filled);
            assert_eq!(after[0], CELL_BLANK, "the abandoned anchor stays blank");
        });
    }

    // ------------------------------------------------------------- resize

    /// Resize goes through `MapOp::Resize`, clamps out-of-range numbers,
    /// ignores garbage, and undoes in one step.
    #[gpui::test]
    async fn test_resize_applies_clamps_and_undoes(cx: &mut TestAppContext) {
        // The resize fields are real `Editor`s, which need the settings
        // store the rest of these panel tests can do without.
        cx.update(|cx| {
            AppState::test(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.ensure_resize_fields(window, cx))
        });

        let set = |text: (&str, &str), cx: &mut gpui::VisualTestContext| {
            let (w, h) = (text.0.to_string(), text.1.to_string());
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    let fields = ready(panel).resize.as_ref().expect("fields exist");
                    let (we, he) = (fields.w.clone(), fields.h.clone());
                    we.update(cx, |e, cx| e.set_text(w.clone(), window, cx));
                    he.update(cx, |e, cx| e.set_text(h.clone(), window, cx));
                })
            });
        };

        set(("6", "2"), cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.resize_impl(window, cx)));
        panel.update(cx, |panel, _| {
            let state = ready(panel).session.store.state();
            assert_eq!((state.w, state.h), (6, 2));
            assert_eq!(state.cells.len(), 12);
        });

        // Out of range clamps to the map-size limits.
        set(("9999", "0"), cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.resize_impl(window, cx)));
        panel.update(cx, |panel, _| {
            let state = ready(panel).session.store.state();
            assert_eq!((state.w, state.h), (geom::MAX_MAP_DIM, geom::MIN_MAP_DIM));
        });

        // Garbage is a no-op, not a resize to the minimum.
        set(("wide", "5"), cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.resize_impl(window, cx)));
        panel.update(cx, |panel, cx| {
            let state = ready(panel).session.store.state();
            assert_eq!((state.w, state.h), (geom::MAX_MAP_DIM, geom::MIN_MAP_DIM));

            panel.undo_impl(cx);
            let state = ready(panel).session.store.state();
            assert_eq!((state.w, state.h), (6, 2), "undo steps back one resize");
        });
    }

    // --------------------------------------------------------------- save

    /// Save writes the live document back to the file it was READ from,
    /// under the derived asset root -- and worldlib reads it back
    /// identically, sidecar rels included.
    #[gpui::test]
    async fn test_save_round_trips_through_open_map(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);

        panel.update(cx, |panel, cx| {
            panel.paint_at((1, 2), cx);
            assert!(ready(panel).session.store.dirty());
            panel.save_impl(cx);
            let open = ready(panel);
            assert!(open.session.save_error.is_none());
            assert!(!open.session.store.dirty(), "save clears dirty");
        });

        let on_disk = io::open_map(&assets, "maps/level.map").unwrap();
        assert_eq!((on_disk.w, on_disk.h), (FIXTURE_W, FIXTURE_H));
        assert_eq!(
            on_disk.cells[2 * FIXTURE_W as usize + 1],
            pack_cell(0, 0, false, false)
        );
        assert_eq!(
            on_disk.til_path, "tiles/world.til",
            "save must keep the sidecar rel assets-root-relative"
        );
        // And the panel's own state agrees with what landed.
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).session.store.state().cells, on_disk.cells);
        });
    }

    // ----------------------------------------------- unsaved-document guard

    fn dirty_the_map(panel: &Entity<MapPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            panel.paint_at((0, 0), cx);
            assert!(
                panel.dirty_map_name().is_some(),
                "paint should dirty the doc"
            );
        });
    }

    /// A clean panel is invisible to the close flow.
    #[gpui::test]
    async fn test_close_guard_lets_a_clean_panel_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        assert!(!cx.has_pending_prompt());
        assert!(close.await);
    }

    /// Cancel aborts the window close and leaves the document dirty and
    /// unwritten -- the data-loss guard proper.
    #[gpui::test]
    async fn test_close_guard_cancel_aborts_the_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some(
                "assets/maps/level.map contains unsaved edits. Do you want to save it?".to_string()
            ),
        );
        cx.simulate_prompt_answer("Cancel");
        assert!(!close.await, "Cancel must veto the close");
        panel.update(cx, |panel, _| assert!(panel.dirty_map_name().is_some()));
        assert_eq!(
            io::open_map(&assets, "maps/level.map").unwrap().cells,
            blank_cells(),
            "Cancel must not have written the file"
        );
    }

    /// Save writes through the panel's own save path and then allows the
    /// close.
    #[gpui::test]
    async fn test_close_guard_save_writes_then_allows_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        cx.simulate_prompt_answer("Save");
        assert!(close.await);
        panel.update(cx, |panel, _| assert!(panel.dirty_map_name().is_none()));
        assert_ne!(
            io::open_map(&assets, "maps/level.map").unwrap().cells,
            blank_cells(),
            "Save must have written the edit"
        );
    }

    /// Answering "Save" when the write FAILS must cancel the close --
    /// letting it proceed would discard the very edits the user just asked
    /// to keep.
    #[gpui::test]
    async fn test_close_guard_save_failure_cancels_the_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        // The save resolves against the OPEN document's captured root;
        // repointing that root at a regular file makes the write's
        // parent-dir creation fail deterministically.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.session.root = blocker;
        });

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        cx.simulate_prompt_answer("Save");
        assert!(!close.await, "a failed save must cancel the close");

        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert!(
                open.session.save_error.is_some(),
                "the failure must be surfaced"
            );
            assert!(
                open.session.store.dirty(),
                "the edits must survive the failed save"
            );
        });
    }

    /// "Don't Save" closes and deliberately drops the edits -- the file on
    /// disk keeps its loaded contents.
    #[gpui::test]
    async fn test_close_guard_discard_allows_close_without_writing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        cx.simulate_prompt_answer("Don't Save");
        assert!(close.await, "Don't Save must allow the close");
        assert_eq!(
            io::open_map(&assets, "maps/level.map").unwrap().cells,
            blank_cells(),
            "Don't Save must not write the file"
        );
    }

    /// A failed write must be VISIBLE (`save_error` set, which the panel
    /// banner renders) and must not `mark_saved` -- the document keeps its
    /// edits and stays dirty for the next attempt.
    #[gpui::test]
    async fn test_save_failure_sets_the_error_and_keeps_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);

        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        panel.update(cx, |panel, cx| {
            panel.paint_at((0, 0), cx);
            assert!(panel.dirty_map_name().is_some());
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.session.root = blocker;

            panel.save_impl(cx);
            let open = ready(panel);
            assert!(
                open.session.save_error.is_some(),
                "the failed write must surface as save_error"
            );
            assert!(
                open.session.store.dirty(),
                "a failed save must not mark the document saved"
            );
        });
        assert_eq!(
            io::open_map(&assets, "maps/level.map").unwrap().cells,
            blank_cells(),
            "the real file must be untouched by the failed write"
        );
    }

    /// The wiring test: a dirty panel docked in a REAL workspace makes
    /// `Workspace::prepare_to_close` prompt and, on Cancel, report `false`.
    #[gpui::test]
    async fn test_dirty_panel_vetoes_workspace_prepare_to_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _, cx) = routed_workspace(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/level.map", cx);
        });
        cx.run_until_parked();
        dirty_the_map(&panel, cx);

        // The map is a tab: closing it goes through the pane's own dirty
        // prompt, driven by the item's `is_dirty`.
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let close = pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
        });
        cx.run_until_parked();
        assert!(
            cx.has_pending_prompt(),
            "a dirty map tab prompts before closing"
        );
        cx.simulate_prompt_answer("Cancel");
        close.await.unwrap();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<MapEditorItem>(cx).count(),
                1,
                "still open"
            );
        });
        panel.read_with(cx, |panel, _| assert!(panel.dirty(), "and still dirty"));
    }

    /// "Don't Save" closes the tab and discards the edits. `Pane` reloads
    /// a singleton item from disk on that answer, so `Item::reload` MUST
    /// exist -- the default is `unimplemented!()`, which took the whole
    /// editor down when a dirty tab was closed that way.
    #[gpui::test]
    async fn test_discarding_closes_the_dirty_map_tab_without_writing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _, cx) = routed_workspace(cx, dir.path()).await;
        dirty_the_map(&panel, cx);

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let close = pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "a dirty map tab must prompt");
        cx.simulate_prompt_answer("Don't Save");
        close.await.unwrap();
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<MapEditorItem>(cx).count(),
                0,
                "the tab closed"
            );
        });
        assert_eq!(
            io::open_map(&dir.path().join(ASSETS_DIR), "maps/level.map")
                .unwrap()
                .cells,
            blank_cells(),
            "discard writes nothing"
        );
    }

    /// The data-loss guard on a DOCUMENT SWITCH: a file-tree click while
    /// the open map has unsaved edits must prompt, and Cancel must abort
    /// the open -- the previous document stays loaded, dirty and unwritten.
    #[gpui::test]
    async fn test_open_rel_path_cancel_keeps_the_dirty_document(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("assets/maps/other.map", window, cx)
            })
        });
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some(
                "assets/maps/level.map contains unsaved edits. Do you want to save it?".to_string()
            ),
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/maps/level.map");
            assert!(open.session.store.dirty());
        });
    }

    /// Discard on a switch loads the new document and drops the edits.
    #[gpui::test]
    async fn test_open_rel_path_discard_switches_documents(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("assets/maps/other.map", window, cx)
            })
        });
        cx.simulate_prompt_answer("Don't Save");
        cx.run_until_parked();

        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/maps/other.map");
            assert!(!open.session.store.dirty());
        });
        assert_eq!(
            io::open_map(&assets, "maps/level.map").unwrap().cells,
            blank_cells(),
            "Don't Save must not write the abandoned document"
        );
    }

    /// The "Save" branch of a dirty switch writes the edit, then loads the
    /// new document -- and once the panel is clean, switching back neither
    /// prompts nor blocks.
    #[gpui::test]
    async fn test_open_rel_path_save_branch_writes_then_switches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("assets/maps/other.map", window, cx)
            })
        });
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();

        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/maps/other.map", "Save then switch");
            assert!(!open.session.store.dirty(), "the new document starts clean");
        });
        assert_ne!(
            io::open_map(&assets, "maps/level.map").unwrap().cells,
            blank_cells(),
            "Save must have written the abandoned document's edit"
        );

        // A clean panel switches straight back, no prompt.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("assets/maps/level.map", window, cx)
            })
        });
        assert!(
            !cx.has_pending_prompt(),
            "a clean panel must switch without asking"
        );
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/maps/level.map");
        });
    }

    /// **Regression, fix round 1 BLOCKING 2.** "New Map…" must run the
    /// unsaved-edits guard BEFORE it writes anything.
    ///
    /// Creating first and prompting afterwards left a `map.map` orphaned
    /// on disk when the user cancelled -- with the old document still on
    /// screen, and the next attempt creating `map-2.map` beside the
    /// orphan.
    #[gpui::test]
    async fn test_new_map_cancel_creates_no_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.create_map_inline("assets/maps", "arena", window, cx)
            })
        });
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some(
                "assets/maps/level.map contains unsaved edits. Do you want to save it?".to_string()
            ),
            "creating a map while the open one is dirty must prompt FIRST"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert!(
            !assets.join("maps/arena.map").exists(),
            "Cancel must not leave a file behind"
        );
        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/maps/level.map");
            assert!(open.session.store.dirty(), "and the edits stay put");
        });

        // Going through with it now creates the named map.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.create_map_inline("assets/maps", "arena", window, cx)
            })
        });
        cx.simulate_prompt_answer("Don't Save");
        cx.run_until_parked();
        assert!(assets.join("maps/arena.map").is_file());
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/maps/arena.map");
        });
    }

    /// Clicking the file that is ALREADY open must be a pure focus/reveal:
    /// no prompt and no reload. The undo assertion is the load-bearing one
    /// -- a reload would rebuild `OpenMap` and leave nothing to undo -- and
    /// the zoom assertion pins that the view state survives too.
    #[gpui::test]
    async fn test_open_rel_path_on_the_open_map_does_not_reload(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_map(&panel, cx);
        panel.update(cx, |panel, cx| panel.step_zoom(3, cx));
        let generation = panel.read_with(cx, |panel, _| panel.load_generation);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("assets/maps/level.map", window, cx)
            })
        });
        assert!(
            !cx.has_pending_prompt(),
            "re-opening the open map must not prompt"
        );
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            assert_eq!(
                panel.load_generation, generation,
                "an already-open click must not start another load"
            );
            assert_eq!(
                ready(panel).zoom,
                geom::DEFAULT_ZOOM + 3,
                "the view state must survive an already-open click"
            );
            assert!(ready(panel).session.store.dirty());
            panel.undo_impl(cx);
            assert_eq!(
                cells(panel),
                blank_cells(),
                "the undo stack must have survived too"
            );
        });
    }

    // ------------------------------------------ explorer-driven routing

    /// A workspace with `init()` run -- so the REAL interceptor and
    /// contributor are in the registry -- over a worktree rooted at the
    /// REAL temp project. Unlike the world panel's fake `/proj` worktree,
    /// this one has to be the real path: "New Map…"'s predicate stats the
    /// clicked directory's ancestors for `emerald.toml`.
    async fn routed_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Workspace>,
        Entity<MapPanel>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        write_project(root);
        cx.update(|cx| {
            AppState::test(cx);
            project_panel::init(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            serde_json::json!({
                "emerald.toml": "",
                "notes.txt": "",
                "assets": { "maps": { "level.map": "", "other.map": "" }, "tiles": { "world.til": "" } },
            }),
        )
        .await;
        let project = Project::test(fs, [root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("one visible worktree")
                .read(cx)
                .id()
        });
        // The editor is a tab per map now: open the fixture map's tab and
        // hand its panel back, as the dock panel used to be handed back.
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            open_map_item(workspace, "assets/maps/level.map".to_string(), window, cx);
            workspace
                .items_of_type::<MapEditorItem>(cx)
                .next()
                .expect("open_map_item adds the item")
                .read(cx)
                .panel()
                .clone()
        });
        let root = root.to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));
        cx.run_until_parked();
        // The inline "New Map…" entry seeds the project panel's name
        // editor, so the tests need one docked -- production gets it from
        // `initialize_workspace`.
        workspace.update_in(cx, |workspace, window, cx| {
            let project_panel = project_panel::ProjectPanel::ggo_test_new(workspace, window, cx);
            workspace.add_panel(project_panel, window, cx);
        });
        (workspace, panel, worktree_id, cx)
    }

    fn project_path(worktree_id: WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    /// The registered `.map` predicate claims the path (so the project
    /// panel opens NO pane item for it), opens the dock, and loads the map.
    /// Anything else in the same worktree is declined.
    #[gpui::test]
    async fn test_map_click_routes_into_the_panel_and_is_claimed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, worktree_id, cx) = routed_workspace(cx, dir.path()).await;

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "assets/maps/level.map"),
                window,
                cx,
            )
        });
        assert!(claimed, "a .map must be claimed, suppressing the pane item");
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/maps/level.map");
        });
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "routing must open the panel's dock even if it was closed"
            );
        });

        for rel in ["notes.txt", "assets/tiles/world.til"] {
            let claimed = workspace.update_in(cx, |workspace, window, cx| {
                workspace.intercept_path_open(&project_path(worktree_id, rel), window, cx)
            });
            assert!(!claimed, "{rel} must open the normal way");
        }
    }

    // -------------------------------------------------- New Map… (menu)

    /// The entry is offered for directories inside the asset root and for
    /// nothing else -- not for a file (even a `.map`), not for the project
    /// root, not for a directory outside `assets/`.
    #[gpui::test]
    async fn test_context_menu_offers_new_map_only_for_assets_dirs(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scratch")).unwrap();
        let (workspace, _panel, worktree_id, cx) = routed_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };
        assert_eq!(contributed("assets", true, cx), 1, "the asset root itself");
        assert_eq!(contributed("assets/maps", true, cx), 1, "and below it");
        assert_eq!(
            contributed("", true, cx),
            0,
            "the project root is not assets"
        );
        assert_eq!(contributed("scratch", true, cx), 0, "outside assets/");
        assert_eq!(
            contributed("assets/maps/level.map", false, cx),
            0,
            "New Map… is a directory action"
        );
    }

    /// The entry's OWN handler seeds the project panel's inline name
    /// editor; committing a name creates a blank, UNBOUND map with that
    /// name in the clicked directory and opens it here -- and the same
    /// name again is refused in the editor, before anything is written.
    #[gpui::test]
    async fn test_new_map_names_inline_then_creates_and_opens(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, worktree_id, cx) = routed_workspace(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);

        let handler = new_map_handler(
            workspace.downgrade(),
            worktree_id,
            "assets/maps".to_string(),
            assets.join("maps"),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        let project_panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<project_panel::ProjectPanel>(cx)
                .expect("docked")
        });
        project_panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.ggo_test_inline_state(),
                (true, true),
                "the inline editor is open with the commit armed"
            );
        });
        project_panel.update_in(cx, |panel, window, cx| {
            panel
                .ggo_test_filename_editor()
                .clone()
                .update(cx, |editor, cx| editor.set_text("arena", window, cx));
            panel.ggo_test_confirm_edit(window, cx);
        });
        cx.run_until_parked();

        assert!(
            assets.join("maps/arena.map").is_file(),
            "the file must exist"
        );
        // The new map opens in its OWN tab, not the fixture's.
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<MapEditorItem>(cx)
                .find(|item| item.read(cx).rel() == "assets/maps/arena.map")
                .expect("a tab for the new map")
                .read(cx)
                .panel()
                .clone()
        });
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/maps/arena.map");
            assert_eq!(open.session.rel_path, "maps/arena.map");
            assert_eq!(open.session.root, assets);
            let state = open.session.store.state();
            assert_eq!((state.w, state.h), (geom::NEW_MAP_DIM, geom::NEW_MAP_DIM));
            assert!(state.cells.iter().all(|&c| c == CELL_BLANK));
            assert_eq!(state.til_path, "", "a new map starts UNBOUND by design");
            assert!(!state.dirty, "creating it is not an unsaved edit");
            assert!(open.session.tileset.is_none());
            assert_eq!(
                open.tilesets,
                vec!["tiles/wide.til".to_string(), "tiles/world.til".to_string()],
                "the bind picker must offer the project's tilesets"
            );
        });

        // The same name again is refused while typing -- nothing written.
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        project_panel.update_in(cx, |panel, window, cx| {
            panel
                .ggo_test_filename_editor()
                .clone()
                .update(cx, |editor, cx| editor.set_text("arena", window, cx));
            panel.ggo_test_confirm_edit(window, cx);
        });
        cx.run_until_parked();
        project_panel.read_with(cx, |panel, _| {
            assert_eq!(panel.ggo_test_inline_state(), (true, true));
            assert!(
                panel
                    .ggo_test_validation_error()
                    .is_some_and(|error| error.contains("already exists")),
                "a duplicate name is refused in the editor"
            );
        });
    }

    // ------------------------------------------------------ strip mapping

    /// `strip_local`/`strip_cell_at` map a WINDOW position through the
    /// stamped strip bounds onto a tile -- with the fixture's 4x1 sheet at
    /// [`geom::STRIP_ZOOM`] a cell is 32 px -- and the `tile_count` gate
    /// keeps a wide sheet's zero-filled partial-row padding unpickable.
    #[gpui::test]
    async fn test_strip_cell_at_maps_through_stamped_bounds_and_gates_padding(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                *open.strip_bounds.borrow_mut() =
                    Some(bounds(point(px(50.), px(400.)), size(px(256.), px(64.))));
                assert_eq!(
                    open.strip_local(point(px(82.), px(410.))),
                    Some([32.0, 10.0]),
                    "strip-local px are window px minus the strip origin"
                );
                assert_eq!(open.strip_cell_at(point(px(83.), px(401.))), Some((1, 0)));
                assert_eq!(
                    open.strip_cell_at(point(px(49.), px(400.))),
                    None,
                    "left of the strip is a miss, not cell 0"
                );
                assert_eq!(
                    open.strip_cell_at(point(px(50. + 4. * 32.), px(400.))),
                    None,
                    "past the 4-column sheet is a miss"
                );
                assert_eq!(
                    open.strip_cell_at(point(px(50.), px(432.))),
                    None,
                    "below the single row is a miss"
                );
            }

            // The wide sheet: 9 tiles across the 8-column fallback, so row
            // 1 holds one real tile and 7 padding cells.
            panel.bind_tileset("tiles/wide.til".to_string(), cx);
            let open = ready(panel);
            assert_eq!(
                open.strip_cell_at(point(px(51.), px(433.))),
                Some((0, 1)),
                "tile 8 -- the last real tile -- is pickable"
            );
            assert_eq!(
                open.strip_cell_at(point(px(83.), px(433.))),
                None,
                "the padding cell after it is not"
            );
        });
    }

    fn move_event(x: f32, y: f32, button: Option<MouseButton>) -> MouseMoveEvent {
        MouseMoveEvent {
            position: point(px(x), px(y)),
            pressed_button: button,
            modifiers: gpui::Modifiers::default(),
        }
    }

    /// The strip's mouse trio: down arms the drag and anchors both
    /// corners, a held move extends the far corner (misses don't smear
    /// it), up finishes keeping the selection -- and the picked rect plus
    /// the live palSub is exactly what the stamp folds in.
    #[gpui::test]
    async fn test_strip_mouse_trio_arms_extends_and_clears(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            *ready(panel).strip_bounds.borrow_mut() =
                Some(bounds(point(px(0.), px(0.)), size(px(128.), px(32.))));

            // A down on a miss (off the 4x1 sheet) arms nothing.
            panel.strip_primary_down(point(px(0.), px(200.)), cx);
            assert!(!ready(panel).pal_dragging);

            panel.strip_primary_down(point(px(33.), px(1.)), cx);
            {
                let open = ready(panel);
                assert!(open.pal_dragging, "down arms the drag");
                assert_eq!(open.session.pal_anchor, (1, 0));
                assert_eq!(
                    open.session.pal_far,
                    (1, 0),
                    "both corners start on the hit tile"
                );
            }

            panel.strip_drag_move(&move_event(65., 1., Some(MouseButton::Left)), cx);
            {
                let open = ready(panel);
                assert_eq!(open.session.pal_anchor, (1, 0), "the anchor holds");
                assert_eq!(
                    open.session.pal_far,
                    (2, 0),
                    "the held move extends the far corner"
                );
                assert!(open.pal_dragging);
            }

            panel.strip_drag_move(&move_event(300., 300., Some(MouseButton::Left)), cx);
            {
                let open = ready(panel);
                assert_eq!(
                    open.session.pal_far,
                    (2, 0),
                    "a miss while dragging leaves the selection put"
                );
                assert!(open.pal_dragging, "and the drag stays alive");
            }

            panel.strip_primary_up();
            {
                let open = ready(panel);
                assert!(!open.pal_dragging, "up finishes the drag");
                assert_eq!(
                    (open.session.pal_anchor, open.session.pal_far),
                    ((1, 0), (2, 0))
                );
            }

            // The selection + the live palSub is what the brush stamps.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.session.pal_sub = 5;
            }
            let stamp = ready(panel).session.current_stamp();
            assert_eq!(
                stamp.cells,
                vec![pack_cell(1, 5, false, false), pack_cell(2, 5, false, false)],
                "the picked tiles carry the palSub"
            );

            // A button that came up outside the strip cancels the next
            // drag on its first move.
            panel.strip_primary_down(point(px(1.), px(1.)), cx);
            panel.strip_drag_move(&move_event(65., 1., None), cx);
            let open = ready(panel);
            assert!(
                !open.pal_dragging,
                "a move without the button ends the drag"
            );
            assert_eq!(
                open.session.pal_far,
                (0, 0),
                "without extending the selection"
            );
        });
    }

    // ----------------------------------------------------- canvas gestures

    /// Middle-drag pan, mirroring `ggo_world_panel`'s: the move handler
    /// applies the cursor delta to the drag's starting pan, a release
    /// elsewhere cancels (while still claiming the event), and with no
    /// drag in flight the move is not this handler's to consume.
    #[gpui::test]
    async fn test_handle_pan_move_pans_by_the_cursor_delta_and_cancels_on_release(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.pan_drag = Some(PanDrag {
                    start_cursor: [10.0, 10.0],
                    start_pan: [5.0, 5.0],
                });
            }

            let held = move_event(30., 25., Some(MouseButton::Middle));
            assert!(
                panel.handle_pan_move(&held, cx),
                "an in-flight pan owns the move"
            );
            assert_eq!(ready(panel).pan, [25.0, 20.0]);

            assert!(
                panel.handle_pan_move(&move_event(40., 40., None), cx),
                "the cancelling move still belongs to the pan"
            );
            {
                let open = ready(panel);
                assert!(
                    open.pan_drag.is_none(),
                    "release elsewhere cancels the drag"
                );
                assert_eq!(open.pan, [25.0, 20.0], "without moving the pan again");
            }

            assert!(
                !panel.handle_pan_move(&held, cx),
                "with no drag in flight the move is not a pan event"
            );
        });
    }

    /// Wheel zoom anchored on the cursor over the integer ladder: the pan
    /// keeps the map pixel under the cursor fixed ([`geom::zoom_at`]), and
    /// the ladder ends are no-ops for both zoom and pan.
    #[gpui::test]
    async fn test_zoom_at_cursor_steps_the_ladder_and_no_ops_at_the_ends(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.pan = [8.0, 4.0];
            }
            panel.zoom_at_cursor(1, [40.0, 20.0], cx);
            {
                let open = ready(panel);
                assert_eq!(open.zoom, geom::DEFAULT_ZOOM + 1);
                assert_eq!(
                    open.pan,
                    geom::zoom_at(
                        [8.0, 4.0],
                        geom::DEFAULT_ZOOM,
                        [40.0, 20.0],
                        geom::DEFAULT_ZOOM + 1
                    ),
                    "the pan is the cursor-anchored adjustment"
                );
            }

            if let ViewerState::Ready(open) = &mut panel.state {
                open.zoom = geom::MAX_ZOOM;
                open.pan = [8.0, 4.0];
            }
            panel.zoom_at_cursor(1, [40.0, 20.0], cx);
            {
                let open = ready(panel);
                assert_eq!(open.zoom, geom::MAX_ZOOM, "the top of the ladder holds");
                assert_eq!(open.pan, [8.0, 4.0], "a ladder end must not move the pan");
            }

            if let ViewerState::Ready(open) = &mut panel.state {
                open.zoom = geom::MIN_ZOOM;
            }
            panel.zoom_at_cursor(-1, [40.0, 20.0], cx);
            let open = ready(panel);
            assert_eq!(open.zoom, geom::MIN_ZOOM, "the bottom of the ladder holds");
            assert_eq!(open.pan, [8.0, 4.0]);
        });
    }

    // ------------------------------------------------------- tool buttons

    /// Switching tools discards a pending rect-fill preview -- the rule
    /// the toolbar buttons' click closures route through `set_tool` for: a
    /// half-dragged rect must not survive into another tool's gesture.
    #[gpui::test]
    async fn test_switching_tools_clears_the_pending_rect(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.set_tool(MapTool::RectFill, cx);
            panel.paint_at((1, 1), cx);
            assert_eq!(ready(panel).session.rect_pending, Some((1, 1, 1, 1)));

            panel.set_tool(MapTool::Eraser, cx);
            let open = ready(panel);
            assert_eq!(open.session.tool, MapTool::Eraser);
            assert!(
                open.session.rect_pending.is_none(),
                "the pending rect must not survive the tool switch"
            );
        });
    }

    // ----------------------------------------------- keystroke dispatch

    /// Load the fixture map into a panel that is the root view of a real
    /// test window -- what keystroke dispatch needs -- modeled on
    /// `ggo_world_panel`'s `ready_panel_in_window`.
    async fn ready_panel_in_window<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
    ) -> (Entity<MapPanel>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        write_project(root);
        let root = root.to_path_buf();
        let (panel, cx) = cx.add_window_view(|_window, cx| {
            let mut panel = MapPanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        // Focus only routes keystrokes while the window is ACTIVE (the
        // same note as the world panel's helper).
        cx.update(|window, _| window.activate_window());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/maps/level.map", cx);
        });
        cx.run_until_parked();
        (panel, cx)
    }

    /// The panel-scoped bindings only fire while the panel's own focus
    /// handle has focus -- the focus `canvas_primary_down` takes in
    /// production.
    fn focus_the_panel(panel: &Entity<MapPanel>, cx: &mut gpui::VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();
    }

    /// `ctrl-z`/`ctrl-shift-z` reach `undo_impl`/`redo_impl` through the
    /// keymap over a real paint (the history semantics have their own
    /// method-level tests; this pins the keystroke wiring).
    #[gpui::test]
    async fn test_undo_redo_keystrokes_step_a_real_paint(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        panel.update(cx, |panel, cx| panel.paint_at((1, 1), cx));
        focus_the_panel(&panel, cx);

        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(cells(panel), blank_cells(), "ctrl-z must undo the paint");
            assert!(!ready(panel).session.store.dirty());
        });

        cx.simulate_keystrokes("ctrl-shift-z");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                cells(panel)[FIXTURE_W as usize + 1],
                pack_cell(0, 0, false, false),
                "ctrl-shift-z must redo it"
            );
        });
    }

    #[gpui::test]
    async fn test_save_keystroke_writes_the_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        panel.update(cx, |panel, cx| panel.paint_at((1, 2), cx));
        focus_the_panel(&panel, cx);

        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();

        let on_disk = io::open_map(&dir.path().join(ASSETS_DIR), "maps/level.map").unwrap();
        assert_eq!(
            on_disk.cells[2 * FIXTURE_W as usize + 1],
            pack_cell(0, 0, false, false),
            "ctrl-s must write the painted cell to disk"
        );
        panel.read_with(cx, |panel, _| {
            assert!(!ready(panel).session.store.dirty(), "save clears dirty");
        });
    }

    /// Enter inside a focused size field resolves through the
    /// `GgoMapPanel > Editor` binding to `ApplyResize`: the typed size
    /// lands in the document, and the field shows the applied value.
    #[gpui::test]
    async fn test_enter_in_a_focused_size_field_applies_the_resize(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        let w_editor = panel.read_with(cx, |panel, _| {
            ready(panel)
                .resize
                .as_ref()
                .expect("the windowed render created the resize fields")
                .w
                .clone()
        });

        w_editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        w_editor.update_in(cx, |editor, window, cx| editor.set_text("6", window, cx));

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let state = ready(panel).session.store.state();
            assert_eq!(
                (state.w, state.h),
                (6, FIXTURE_H),
                "enter must apply the typed width"
            );
            assert_eq!(state.cells.len(), 6 * FIXTURE_H as usize);
        });
        assert_eq!(
            w_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "6",
            "the field shows the applied value"
        );
    }

    // ------------------------------------------- sidecar cols + terrains

    #[gpui::test]
    async fn test_the_tileset_sidecar_drives_strip_cols_and_terrains(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        ggo_worldlib::sprites::tileset_meta::save_tileset_meta(
            dir.path(),
            "assets/tiles/wide.til",
            &ggo_worldlib::sprites::tileset_meta::TilesetMeta {
                cols: Some(3),
                terrains: vec![Terrain {
                    name: "grass".into(),
                    tiles: vec![],
                }],
                ..Default::default()
            },
        )
        .unwrap();
        panel.update(cx, |panel, cx| {
            panel.bind_tileset("tiles/wide.til".to_string(), cx);
            let open = ready(panel);
            assert_eq!(
                open.session.tileset.as_ref().unwrap().cols,
                3,
                "cols come from the sidecar"
            );
            assert_eq!(open.session.terrains[0].name, "grass");
            assert_eq!(
                open.session.til_meta_rel.as_deref(),
                Some("assets/tiles/wide.til")
            );
            // Undoing the bind returns to world.til, which has no sidecar.
            panel.undo_impl(cx);
            let open = ready(panel);
            assert_eq!(
                open.session.tileset.as_ref().unwrap().cols,
                FIXTURE_TILES,
                "back to the default layout"
            );
            assert!(open.session.terrains.is_empty());
        });
    }

    // ------------------------------------------ fill, strokes, selection

    #[gpui::test]
    async fn test_fill_floods_only_the_connected_region(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            for y in 0..FIXTURE_H as i32 {
                panel.paint_at((2, y), cx);
            }
            panel.set_tool(MapTool::Fill, cx);
            panel.paint_at((0, 0), cx);
            let filled = cells(panel);
            let tile0 = pack_cell(0, 0, false, false);
            for y in 0..FIXTURE_H as usize {
                let row = &filled[y * FIXTURE_W as usize..][..FIXTURE_W as usize];
                assert_eq!(row, &[tile0, tile0, tile0, CELL_BLANK], "row {y}");
            }
            panel.undo_impl(cx);
            assert_eq!(cells(panel)[0], CELL_BLANK, "fill is its own undo step");
        });
    }

    #[gpui::test]
    async fn test_a_brush_drag_undoes_as_one_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.session.store.begin_stroke();
            }
            panel.paint_at((0, 0), cx);
            panel.paint_at((1, 0), cx);
            panel.end_paint(cx);
            panel.undo_impl(cx);
            assert_eq!(
                cells(panel),
                blank_cells(),
                "one undo reverts the whole drag"
            );
            panel.undo_impl(cx);
            assert_eq!(
                cells(panel),
                blank_cells(),
                "a second undo has nothing to revert"
            );
        });
    }

    #[gpui::test]
    async fn test_select_copy_paste_and_delete(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let tile0 = pack_cell(0, 0, false, false);
        let at = |x: i32, y: i32| y as usize * FIXTURE_W as usize + x as usize;
        panel.update(cx, |panel, cx| {
            panel.paint_at((1, 1), cx);
            panel.set_tool(MapTool::Select, cx);
            panel.paint_at((2, 2), cx);
            panel.paint_at((1, 1), cx);
            panel.end_paint(cx);
            assert_eq!(
                ready(panel).session.selection,
                Some((1, 1, 2, 2)),
                "normalized on release"
            );

            panel.copy_impl(cx);
            let stamp = panel.clipboard.as_ref().unwrap();
            assert_eq!((stamp.w, stamp.h), (2, 2));
            assert_eq!(stamp.cells, vec![tile0, CELL_BLANK, CELL_BLANK, CELL_BLANK]);

            // Paste lands under the cursor when it is over the canvas...
            if let ViewerState::Ready(open) = &mut panel.state {
                open.hover_cell = Some((2, 0));
            }
            panel.paste_impl(cx);
            let cells_now = cells(panel);
            assert_eq!(cells_now[at(2, 0)], tile0);
            assert_eq!(cells_now[at(3, 1)], CELL_BLANK);
            assert_eq!(
                ready(panel).session.selection,
                Some((2, 0, 3, 1)),
                "selection follows the paste"
            );

            panel.delete_selection_impl(cx);
            assert_eq!(cells(panel)[at(2, 0)], CELL_BLANK);

            // ...and on the selection's corner otherwise.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.hover_cell = None;
                open.session.selection = Some((0, 0, 0, 0));
            }
            panel.paste_impl(cx);
            assert_eq!(cells(panel)[at(0, 0)], tile0);

            panel.clear_selection_impl(cx);
            assert_eq!(ready(panel).session.selection, None);
        });
    }

    // --------------------------------------------------------- terrains

    #[gpui::test]
    async fn test_terrains_persist_to_the_sidecar_and_paint_by_neighbour_mask(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let tile = |c: u16| unpack_cell(c).tile;
        let at = |x: i32, y: i32| y as usize * FIXTURE_W as usize + x as usize;
        let sidecar = |root: &Path| {
            ggo_worldlib::sprites::tileset_meta::load_tileset_meta(root, "assets/tiles/world.til")
                .terrains
        };
        panel.update(cx, |panel, cx| {
            panel.set_tool(MapTool::Terrain, cx);
            panel.add_terrain("ground".to_string(), cx);
            assert_eq!(
                ready(panel).session.terrain,
                Some(0),
                "a new terrain is selected"
            );
            assert_eq!(sidecar(dir.path())[0].name, "ground");

            // Tile 0 (the default stamp) is the isolated tile; 1 and 2
            // are the east- and west-neighbour tiles.
            panel.assign_anchor_tile(cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.session.terrains[0].assign(1, terrain::EAST);
                open.session.terrains[0].assign(2, terrain::WEST);
            }
            panel.save_terrains(cx);
            assert_eq!(sidecar(dir.path())[0].tiles.len(), 3);

            panel.paint_at((0, 0), cx);
            assert_eq!(tile(cells(panel)[at(0, 0)]), 0, "alone: the isolated tile");
            panel.paint_at((1, 0), cx);
            let now = cells(panel);
            assert_eq!(tile(now[at(0, 0)]), 1, "gained an east neighbour");
            assert_eq!(tile(now[at(1, 0)]), 2, "west neighbour");

            if let ViewerState::Ready(open) = &mut panel.state {
                open.session.paint_erase = true;
            }
            panel.paint_at((1, 0), cx);
            let now = cells(panel);
            assert_eq!(now[at(1, 0)], CELL_BLANK, "shift-paint erases");
            assert_eq!(tile(now[at(0, 0)]), 0, "and re-resolves the neighbour");

            panel.unassign_tile(2, cx);
            panel.rename_terrain("dirt".to_string(), cx);
            let saved = sidecar(dir.path());
            assert_eq!(saved[0].name, "dirt");
            assert_eq!(saved[0].tiles.len(), 2);
            panel.remove_terrain(cx);
            assert!(sidecar(dir.path()).is_empty());
            assert_eq!(ready(panel).session.terrain, None);
        });
    }
}
