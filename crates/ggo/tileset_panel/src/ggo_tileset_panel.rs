//! GGO Tileset editor: a center-pane tab per `.til` (mirroring
//! `ggo_sprite_panel`'s architecture) with the composed tile sheet as the
//! editing canvas, the transient tools -- pencil/eraser, zoom,
//! undo/redo/save -- in a toolbar across the top, and the document
//! sections (file info, the 16-slot palette the sheet is drawn through)
//! in a column on the panel's right side.
//!
//! Editing goes through worldlib's op surface end to end:
//! `tileset_doc::TilesetDocStore` + `TilesetOp::Paint`, saved by
//! `io::save_tileset` (which writes the `.til` and its companion `.pal`
//! in one call). The eraser is `Paint` with color 0 -- palette slot 0 IS
//! the transparent index (PPU contract §1), so there is no separate
//! erase op to invent. Dirty state reaches the workspace through
//! [`tileset_item::TilesetEditorItem`]'s `Item::is_dirty`/`save`, which
//! is what puts the dot on the tab and routes Ctrl-S/close prompts.
//!
//! Which tileset is open is driven ENTIRELY by the file explorer:
//! clicking a `.til` there routes here through [`intercept_tileset_open`],
//! one tab per file ([`open_tileset_item`]); the panel has no picker of
//! its own.
//!
//! Structural mirror of `ggo_sprite_panel`: the panel entity keeps ALL
//! document logic and `tileset_item` only adapts it to the workspace's
//! tab machinery; `loader` owns everything off the UI thread (the `.til`
//! open, the grid compose) plus the pure grid geometry.

mod loader;
mod palette_widget;
mod tileset_item;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, RenderImage, ScrollHandle,
    Styled, Task, WeakEntity, Window, actions, div, img, point, px,
};
use project::ProjectPath;
use ui::prelude::*;
use workspace::Workspace;

use ggo_worldlib::sprites::io::save_tileset;
use ggo_worldlib::sprites::palette565::PAL_SLOTS;
use ggo_worldlib::sprites::pixel_tools;
use ggo_worldlib::sprites::tileset_doc::{TILE_PX, TilesetDocStore};
use ggo_worldlib::sprites::tileset_meta::load_tileset_meta;

use loader::LoadedTileset;
pub use palette_widget::SmallPaletteEditor;
pub use tileset_item::TilesetEditorItem;

actions!(
    ggo_tileset,
    [
        /// Undoes the last tileset edit.
        Undo,
        /// Redoes the last undone tileset edit.
        Redo,
        /// Saves the tileset (.til + .pal).
        Save,
        /// Scrolls the tileset view left.
        ScrollLeft,
        /// Scrolls the tileset view right.
        ScrollRight,
        /// Scrolls the tileset view up.
        ScrollUp,
        /// Scrolls the tileset view down.
        ScrollDown,
        /// Copies the selected pixel region.
        CopySelection,
        /// Pastes the copied pixel region at the selection.
        PasteSelection,
        /// Zooms the tileset view in one step.
        ZoomIn,
        /// Zooms the tileset view out one step.
        ZoomOut,
        /// Selects the whole sheet with the Select tool.
        SelectWholeSheet,
        /// Shrinks the brush by one pixel.
        BrushSmaller,
        /// Grows the brush by one pixel.
        BrushLarger,
        /// Flips the selected region horizontally.
        FlipHorizontal,
        /// Flips the selected region vertically.
        FlipVertical,
        /// Leaves tile focus, or clears the selection.
        Cancel,
        /// Focuses the tile under the selection (or tile 0) for magnified editing.
        FocusTile,
    ]
);

/// The zoom focus mode uses when the sheet's own zoom is smaller.
const FOCUS_MIN_ZOOM: usize = 8;

/// Brush size bounds (px, square).
const MIN_BRUSH: usize = 1;
const MAX_BRUSH: usize = 4;

/// The panel's key-dispatch context identifier -- undo/redo/save bind
/// here, scoped so they only fire while the editor has focus.
const KEY_CONTEXT: &str = "GgoTilesetPanel";

/// Integer zoom bounds for the sheet canvas. Integer only: the sheet is
/// pixel art, and a non-integer scale would resample 16x16 tiles into
/// blur. 1x is unreadably small on a HiDPI display; pixel editing wants
/// room, so the default is 4x.
const MIN_ZOOM: usize = 1;
const MAX_ZOOM: usize = 16;
const DEFAULT_ZOOM: usize = 4;

/// The tooling column's width.
const TOOLS_COL_PX: f32 = 280.0;

/// The edge "+" bars' thickness.
const EDGE_BAR_PX: f32 = 18.0;

/// The tileset extension this editor claims from the file explorer.
const TILESET_EXT: &str = "til";

pub fn init(cx: &mut App) {
    // Explorer-driven routing: clicking a `.til` in the project panel opens
    // the tileset editor tab instead of a (binary, unreadable) text buffer.
    workspace::register_path_open_interceptor(cx, intercept_tileset_open);
}

/// `workspace::PathOpenInterceptor` for `*.til`: claim the path and open
/// (or focus) its center-pane editor tab. Declines (so the normal open
/// path runs) for any other file and for a path outside the primary
/// worktree.
///
/// Note the extension split with `ggo_sprite_panel`: a sprite's `.til`
/// is its tile POOL and is opened through the `.spr` that names it, so
/// clicking the `.til` itself lands here (the sheet editor) rather than
/// in the sprite editor. Both interceptors key off disjoint extensions,
/// so registration order between them doesn't matter.
fn intercept_tileset_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !path
        .path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(TILESET_EXT))
    {
        return false;
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    open_tileset_item(workspace, rel, window, cx);
    true
}

/// Open (or focus) the center-pane tileset tab for worktree-relative
/// `rel` -- one item per file, activate on re-open. Public: the import
/// panel's post-import hand-off lands here.
pub fn open_tileset_item(
    workspace: &mut Workspace,
    rel: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<TilesetEditorItem>(cx)
        .find(|item| item.read(cx).rel() == rel);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let item = cx.new(|cx| TilesetEditorItem::new(rel, weak, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

// ------------------------------------------------------------- view state

/// The editing tools. The eraser is not its own op: it paints palette
/// index 0, the transparent slot. The picker samples the palette slot
/// under the click; Select drags a marquee for copy/paste.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Pencil,
    Eraser,
    Picker,
    Select,
    /// Flood the contiguous same-colour region of the composed sheet.
    Fill,
    Line,
    Rect,
    Ellipse,
}

impl Tool {
    /// Drag-to-shape tools: preview while dragging, commit on release.
    fn is_shape(self) -> bool {
        matches!(self, Tool::Line | Tool::Rect | Tool::Ellipse)
    }
}

/// The open tileset: worldlib's doc store plus the view state that must
/// survive a re-click on the already-open file.
/// The import-record banner: the recorded source changed since the
/// import (or is gone).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ImportAlert {
    source: String,
    missing: bool,
}

/// Compare the `.til`'s recorded source against disk (checked on open,
/// never watched).
fn import_alert_for(root: &Path, rel: &str) -> Option<ImportAlert> {
    let record = load_tileset_meta(root, rel).import?;
    match record.source_changed(root) {
        Some(true) => Some(ImportAlert {
            source: record.source,
            missing: false,
        }),
        None => Some(ImportAlert {
            source: record.source,
            missing: true,
        }),
        Some(false) => None,
    }
}

struct OpenTileset {
    rel_path: String,
    import_alert: Option<ImportAlert>,
    pal_path: String,
    missing_pal: bool,
    cols: usize,
    store: TilesetDocStore,
    /// The composed sheet, rebuilt after every doc op
    /// ([`TilesetPanel::recompose_grid`]).
    grid: Arc<RenderImage>,
    grid_size: (u32, u32),
    zoom: usize,
    tool: Tool,
    /// The selected palette slot the pencil paints with.
    slot: usize,
    save_error: Option<String>,
    /// The sheet image's on-screen bounds, recorded at prepaint by the
    /// canvas overlay -- the same idiom as `ggo_sprite_panel`'s
    /// `preview_bounds` (mouse positions are window-absolute, so pixel
    /// hit-testing needs them).
    sheet_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// True between mouse-down on the sheet and mouse-up anywhere --
    /// drag-painting is live.
    painting: bool,
    /// The sheet scroll container's handle -- arrow-key camera panning
    /// goes through it.
    scroll: ScrollHandle,
    /// The Select tool's marquee, in SHEET pixel coordinates: (anchor,
    /// head), both inclusive, unnormalized (head may sit any side of the
    /// anchor).
    selection: Option<((usize, usize), (usize, usize))>,
    /// The copy buffer: `(w, h, indices)` row-major, sheet-space.
    clipboard: Option<(usize, usize, Vec<u8>)>,
    /// Whether the tile-boundary lines draw over the sheet.
    show_lines: bool,
    /// Brush size (px, square) for the pencil, eraser and shape tools.
    brush: usize,
    /// Mirror every paint about the painted tile's centre lines.
    mirror_h: bool,
    mirror_v: bool,
    /// An in-progress Line/Rect/Ellipse drag, sheet-space (anchor, head).
    shape_drag: Option<((usize, usize), (usize, usize))>,
    /// A Select-tool drag that started INSIDE the marquee: (start, head)
    /// in sheet pixels; release moves the marquee's pixels by the offset.
    move_drag: Option<((usize, usize), (usize, usize))>,
    /// Focus mode: the sheet shows only this tile, magnified.
    focus: Option<usize>,
    /// The sheet zoom to restore when focus ends.
    zoom_before_focus: Option<usize>,
}

impl OpenTileset {
    fn new(rel_path: String, loaded: LoadedTileset) -> Self {
        Self {
            rel_path,
            import_alert: None,
            pal_path: loaded.pal_path,
            missing_pal: loaded.missing_pal,
            cols: loaded.cols,
            store: TilesetDocStore::new(loaded.indices, loaded.tile_count, loaded.palette),
            grid: loaded.grid,
            grid_size: loaded.grid_size,
            zoom: loaded
                .zoom
                .unwrap_or(DEFAULT_ZOOM)
                .clamp(MIN_ZOOM, MAX_ZOOM),
            tool: Tool::Pencil,
            slot: 1,
            save_error: None,
            sheet_bounds: Rc::new(RefCell::new(None)),
            painting: false,
            scroll: ScrollHandle::new(),
            selection: None,
            clipboard: None,
            show_lines: loaded.lines.unwrap_or(true),
            brush: MIN_BRUSH,
            mirror_h: false,
            mirror_v: false,
            shape_drag: None,
            move_drag: None,
            focus: None,
            zoom_before_focus: None,
        }
    }

    /// The move drag's offset so far.
    fn move_offset(&self) -> (i32, i32) {
        match self.move_drag {
            Some(((sx, sy), (hx, hy))) => (hx as i32 - sx as i32, hy as i32 - sy as i32),
            None => (0, 0),
        }
    }

    /// Brush expansion then per-tile mirroring -- what every paint goes
    /// through before it reaches the document.
    fn expand_points(&self, points: &[(i32, i32)]) -> Vec<(i32, i32)> {
        let brushed = pixel_tools::brush_expand(points, self.brush);
        pixel_tools::mirror_in_tile(&brushed, TILE_PX, self.mirror_h, self.mirror_v)
    }

    /// The points a shape drag would paint, expanded.
    fn shape_points(&self, filled: bool) -> Vec<(i32, i32)> {
        let Some(((ax, ay), (hx, hy))) = self.shape_drag else {
            return Vec::new();
        };
        let (a, b) = ((ax as i32, ay as i32), (hx as i32, hy as i32));
        let raw = match self.tool {
            Tool::Line => pixel_tools::line(a, b),
            Tool::Rect => pixel_tools::rect(a, b, filled),
            Tool::Ellipse => pixel_tools::ellipse(a, b, filled),
            _ => Vec::new(),
        };
        self.expand_points(&raw)
    }

    /// The sheet's on-screen size at the current zoom.
    fn zoomed_size(&self) -> (f32, f32) {
        let (w, h) = self.grid_size;
        let z = self.zoom as f32;
        (w as f32 * z, h as f32 * z)
    }

    /// The color the current tool paints with. Only meaningful for the
    /// painting tools -- Picker/Select never reach `paint_at`.
    fn paint_color(&self) -> u8 {
        match self.tool {
            Tool::Eraser => 0,
            _ => self.slot as u8,
        }
    }
}

pub(crate) enum ViewerState {
    /// Nothing opened yet.
    Empty,
    Loading {
        rel_path: String,
    },
    Ready(Box<OpenTileset>),
    Error(String),
}

pub struct TilesetPanel {
    focus_handle: FocusHandle,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    pub(crate) root_override: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl TilesetPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            root_override: None,
            project_root: None,
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Whether the open document holds unsaved edits -- the item's
    /// `is_dirty` source.
    pub(crate) fn dirty(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.store.dirty())
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree). MUST NOT run while the workspace itself is mid-update
    /// (it reads the workspace entity) -- see the deferral in
    /// [`Self::open_rel_path`].
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        cx.notify();
    }

    /// Load the project-relative `.til` path `rel`. This is the entry
    /// point from the item wrapper; there is no in-panel picker.
    ///
    /// The load runs on a spawned task, deliberately: the item is
    /// constructed from INSIDE the workspace's own update, and
    /// [`Self::refresh_root`] has to read that same workspace entity.
    pub fn open_rel_path(&mut self, rel: &str, _window: &mut Window, cx: &mut Context<Self>) {
        // Clicking the file that is ALREADY open focuses the existing tab
        // upstream of this call; reloading here would drop the view state
        // (zoom, tool, undo stack) for no reason.
        if let ViewerState::Ready(open) = &self.state
            && open.rel_path == rel
        {
            return;
        }
        let rel = rel.to_string();
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.refresh_root(cx);
                this.load_rel_path(&rel, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Kick off the off-thread load of `rel`. A stale result (superseded by
    /// a later open) is dropped by generation check.
    fn load_rel_path(&mut self, rel: &str, cx: &mut Context<Self>) {
        let rel = rel.to_string();
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            rel_path: rel.clone(),
        };
        cx.notify();

        let load = {
            let rel = rel.clone();
            let root = root.clone();
            cx.background_spawn(async move { loader::load_tileset(&root, &rel) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => {
                        let mut open = OpenTileset::new(rel.clone(), loaded);
                        open.import_alert = import_alert_for(&root, &rel);
                        ViewerState::Ready(Box::new(open))
                    }
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// The `.til` currently open, as the worktree-relative path it was
    /// opened WITH -- `None` unless the editor is Ready. Public as the
    /// observation point for the import panel's hand-off assertions.
    pub fn open_rel_path_now(&self) -> Option<&str> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.rel_path.as_str()),
            _ => None,
        }
    }

    /// Step the zoom by `delta` steps, clamped to [`MIN_ZOOM`]..=[`MAX_ZOOM`].
    fn zoom_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(zoom) = self.ready_zoom() else {
            return;
        };
        self.set_zoom((zoom as isize + delta).max(MIN_ZOOM as isize) as usize, cx);
    }

    fn ready_zoom(&self) -> Option<usize> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.zoom),
            _ => None,
        }
    }

    /// Set the zoom outright (the slider), clamped; persists like a step.
    fn set_zoom(&mut self, zoom: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let next = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if next != open.zoom {
            open.zoom = next;
            cx.notify();
            self.write_view_meta();
        }
    }

    /// Set the grid's column count, clamped to `1..=tile_count` --
    /// recomposes the sheet at the new width and persists the choice.
    fn set_cols(&mut self, cols: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let tile_count = open.store.state().tile_count;
        let next = cols.clamp(1, tile_count.max(1));
        if next == open.cols {
            return;
        }
        open.cols = next;
        open.grid_size = loader::grid_pixel_size(tile_count, next);
        self.recompose_grid(cx);
        self.write_view_meta();
    }

    /// Pan the sheet's scroll container by (`dx`, `dy`) tiles -- the
    /// arrow-key camera. Offsets grow NEGATIVE as the view scrolls
    /// right/down (gpui's convention); the paint pass clamps to the
    /// content, so only the zero edge needs guarding here.
    /// Enter focus on `tile`: the sheet becomes that one tile, magnified.
    fn enter_focus(&mut self, tile: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if tile >= open.store.state().tile_count {
            return;
        }
        if open.focus.is_none() {
            open.zoom_before_focus = Some(open.zoom);
            open.zoom = open.zoom.clamp(FOCUS_MIN_ZOOM, MAX_ZOOM);
        }
        open.focus = Some(tile);
        open.selection = None;
        open.shape_drag = None;
        open.move_drag = None;
        self.recompose_grid(cx);
    }

    fn leave_focus(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open.focus.take().is_none() {
            return;
        }
        if let Some(zoom) = open.zoom_before_focus.take() {
            open.zoom = zoom;
        }
        open.selection = None;
        self.recompose_grid(cx);
    }

    /// Focus the tile under the marquee (its anchor), else tile 0.
    fn focus_tile_impl(&mut self, cx: &mut Context<Self>) {
        if matches!(&self.state, ViewerState::Ready(open) if open.focus.is_some()) {
            return;
        }
        let tile = self
            .selection_rect()
            .and_then(|(x0, y0, ..)| self.doc_pixel(x0, y0))
            .map_or(0, |(tile, ..)| tile);
        self.enter_focus(tile, cx);
    }

    /// Escape: leave focus if in it, else drop the selection.
    fn cancel_impl(&mut self, cx: &mut Context<Self>) {
        let focused = matches!(&self.state, ViewerState::Ready(open) if open.focus.is_some());
        if focused {
            self.leave_focus(cx);
        } else if let ViewerState::Ready(open) = &mut self.state {
            open.selection = None;
            cx.notify();
        }
    }

    fn step_focus(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let (Some(focus), count) = (open.focus, open.store.state().tile_count) else {
            return;
        };
        let next = (focus as isize + delta).clamp(0, count.max(1) as isize - 1) as usize;
        self.enter_focus(next, cx);
    }

    fn scroll_by(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        if matches!(&self.state, ViewerState::Ready(open) if open.focus.is_some()) && dx != 0.0 {
            self.step_focus(if dx > 0.0 { 1 } else { -1 }, cx);
            return;
        }
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let step = (TILE_PX * open.zoom) as f32;
        let offset = open.scroll.offset();
        open.scroll.set_offset(point(
            (offset.x - px(dx * step)).min(px(0.)),
            (offset.y - px(dy * step)).min(px(0.)),
        ));
        cx.notify();
    }

    /// Persist the per-tileset view settings (zoom, cols) to the
    /// `.ggo-ide` sidecar. Best-effort: a failed write is logged, never
    /// surfaced -- view settings are not document data.
    fn write_view_meta(&self) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(root) = &self.project_root else {
            return;
        };
        // Load-modify-save: the sidecar also carries the map editor's
        // terrains, which this panel must not wipe.
        let mut meta = loader::load_view_meta(root, &open.rel_path);
        meta.zoom = Some(open.zoom_before_focus.unwrap_or(open.zoom));
        meta.cols = Some(open.cols);
        meta.lines = Some(open.show_lines);
        if let Err(e) = loader::save_view_meta(root, &open.rel_path, &meta) {
            log::error!(
                "GGO: failed to write view sidecar for {}: {e}",
                open.rel_path
            );
        }
    }

    // ------------------------------------------------------------ editing

    /// Map a window-absolute mouse position to SHEET pixel coordinates
    /// (over the whole composed grid, trailing pad cells included).
    /// `None` outside the sheet.
    fn sheet_px_at(&self, pos: Point<Pixels>) -> Option<(usize, usize)> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let bounds = (*open.sheet_bounds.borrow())?;
        if !bounds.contains(&pos) {
            return None;
        }
        let z = open.zoom as f32;
        let lx = f32::from(pos.x - bounds.origin.x) / z;
        let ly = f32::from(pos.y - bounds.origin.y) / z;
        let (grid_w, grid_h) = open.grid_size;
        if lx < 0.0 || ly < 0.0 || lx >= grid_w as f32 || ly >= grid_h as f32 {
            return None;
        }
        Some((lx as usize, ly as usize))
    }

    /// The doc pixel `(tile, x, y)` at sheet coordinates `(sx, sy)`, or
    /// `None` over the composed grid's trailing pad cells past
    /// `tile_count` -- the store's paint ops index the buffer raw, so the
    /// guard lives here.
    fn doc_pixel(&self, sx: usize, sy: usize) -> Option<(usize, usize, usize)> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        sheet_to_doc(open.focus, open.cols, open.store.tile_count(), sx, sy)
    }

    /// [`Self::sheet_px_at`] + [`Self::doc_pixel`]: the doc pixel under a
    /// window-absolute mouse position.
    fn pixel_at(&self, pos: Point<Pixels>) -> Option<(usize, usize, usize)> {
        let (sx, sy) = self.sheet_px_at(pos)?;
        self.doc_pixel(sx, sy)
    }

    /// Paint the pixel under `pos` with the current tool, folding into
    /// the open stroke -- one whole drag is one undo step. Same-color
    /// paints are no-ops inside the store, so drag-painting over
    /// already-painted ground is free.
    fn paint_at(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((sx, sy)) = self.sheet_px_at(pos) else {
            return;
        };
        let (points, color) = match &self.state {
            ViewerState::Ready(open) => (
                open.expand_points(&[(sx as i32, sy as i32)]),
                open.paint_color(),
            ),
            _ => return,
        };
        self.write_in_stroke(&points, color, cx);
    }

    /// Paint sheet points inside the stroke the gesture already opened
    /// (points off the sheet or over pad cells are dropped).
    fn write_in_stroke(&mut self, points: &[(i32, i32)], color: u8, cx: &mut Context<Self>) {
        let writes: Vec<(usize, usize, usize)> = points
            .iter()
            .filter(|(x, y)| *x >= 0 && *y >= 0)
            .filter_map(|&(x, y)| self.doc_pixel(x as usize, y as usize))
            .collect();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if writes.is_empty() {
            return;
        }
        for (tile, x, y) in writes {
            open.store.apply_stroke_paint(tile, x, y, color);
        }
        self.recompose_grid(cx);
    }

    /// Paint sheet points as ONE undo step (a whole shape, a fill).
    fn write_points(&mut self, points: &[(i32, i32)], color: u8, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.store.begin_stroke();
        }
        self.write_in_stroke(points, color, cx);
        if let ViewerState::Ready(open) = &mut self.state {
            open.store.end_stroke();
        }
    }

    /// The Fill tool: flood the composed sheet from the clicked pixel.
    fn fill_at(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((sx, sy)) = self.sheet_px_at(pos) else {
            return;
        };
        let (state, cols, focus, color) = match &self.state {
            ViewerState::Ready(open) => (
                open.store.state(),
                open.cols,
                open.focus,
                open.paint_color(),
            ),
            _ => return,
        };
        let sample = |x: i32, y: i32| {
            if x < 0 || y < 0 {
                return None;
            }
            let (tile, px_x, px_y) =
                sheet_to_doc(focus, cols, state.tile_count, x as usize, y as usize)?;
            let index =
                tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS + px_y * TILE_PX + px_x;
            state.indices.get(index).copied()
        };
        let region = pixel_tools::flood(sample, (sx as i32, sy as i32));
        self.write_points(&region, color, cx);
    }

    /// Rebuild the composed sheet image from the store's current state --
    /// after every op, undo, and redo.
    fn recompose_grid(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        let composed = match open.focus {
            Some(tile) => {
                let pixels = state
                    .indices
                    .get(tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS..)
                    .and_then(|rest| rest.get(..ggo_worldlib::sprites::tileset_doc::TILE_PIXELS));
                match pixels {
                    Some(pixels) => (
                        loader::compose_grid(pixels, 1, 1, &state.palette),
                        (TILE_PX as u32, TILE_PX as u32),
                    ),
                    // The focused tile is gone (deleted, undone): fall back
                    // to the sheet.
                    None => {
                        open.focus = None;
                        if let Some(zoom) = open.zoom_before_focus.take() {
                            open.zoom = zoom;
                        }
                        (
                            loader::compose_grid(
                                &state.indices,
                                state.tile_count,
                                open.cols,
                                &state.palette,
                            ),
                            loader::grid_pixel_size(state.tile_count, open.cols),
                        )
                    }
                }
            }
            None => (
                loader::compose_grid(&state.indices, state.tile_count, open.cols, &state.palette),
                loader::grid_pixel_size(state.tile_count, open.cols),
            ),
        };
        if let Some(grid) = composed.0 {
            open.grid = grid;
        }
        open.grid_size = composed.1;
        cx.notify();
    }

    fn on_sheet_mouse_down(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let tool = match &mut self.state {
            ViewerState::Ready(open) => {
                // A release off the sheet may not have reached us: a new
                // press never continues an old drag.
                open.move_drag = None;
                open.shape_drag = None;
                open.tool
            }
            _ => return,
        };
        match tool {
            Tool::Pencil | Tool::Eraser => {
                if let ViewerState::Ready(open) = &mut self.state {
                    open.store.begin_stroke();
                }
                self.paint_at(pos, cx);
            }
            Tool::Picker => {
                self.pick_at(pos, cx);
                return; // a pick is instantaneous, no drag state
            }
            Tool::Fill => {
                self.fill_at(pos, cx);
                return;
            }
            Tool::Line | Tool::Rect | Tool::Ellipse => {
                let anchor = self.sheet_px_at(pos);
                if let ViewerState::Ready(open) = &mut self.state {
                    open.shape_drag = anchor.map(|p| (p, p));
                    cx.notify();
                }
            }
            Tool::Select => {
                let anchor = self.sheet_px_at(pos);
                let inside = match (anchor, self.selection_rect()) {
                    (Some((x, y)), Some((x0, y0, x1, y1))) => {
                        (x0..=x1).contains(&x) && (y0..=y1).contains(&y)
                    }
                    _ => false,
                };
                if let ViewerState::Ready(open) = &mut self.state {
                    if inside {
                        open.move_drag = anchor.map(|p| (p, p));
                    } else {
                        open.selection = anchor.map(|p| (p, p));
                    }
                    cx.notify();
                }
            }
        }
        if let ViewerState::Ready(open) = &mut self.state {
            open.painting = true;
        }
    }

    fn on_sheet_mouse_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let (tool, painting) = match &self.state {
            ViewerState::Ready(open) => (open.tool, open.painting),
            _ => return,
        };
        if !painting {
            return;
        }
        match tool {
            Tool::Pencil | Tool::Eraser => self.paint_at(pos, cx),
            Tool::Line | Tool::Rect | Tool::Ellipse => {
                let head = self.sheet_px_at(pos);
                if let (ViewerState::Ready(open), Some(head)) = (&mut self.state, head)
                    && let Some((anchor, _)) = open.shape_drag
                {
                    open.shape_drag = Some((anchor, head));
                    cx.notify();
                }
            }
            Tool::Select => {
                let head = self.sheet_px_at(pos);
                if let (ViewerState::Ready(open), Some(head)) = (&mut self.state, head) {
                    if let Some((start, _)) = open.move_drag {
                        open.move_drag = Some((start, head));
                    } else if let Some((anchor, _)) = open.selection {
                        open.selection = Some((anchor, head));
                    }
                    cx.notify();
                }
            }
            Tool::Picker | Tool::Fill => {}
        }
    }

    /// The Picker tool: sample the palette slot under the click, make it
    /// the pencil color, and switch back to the pencil.
    fn pick_at(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((tile, x, y)) = self.pixel_at(pos) else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        let idx = tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS + y * TILE_PX + x;
        if let Some(&slot) = state.indices.get(idx) {
            open.slot = slot as usize;
            open.tool = Tool::Pencil;
            cx.notify();
        }
    }

    /// The selection's normalized inclusive rect `(x0, y0, x1, y1)`.
    fn selection_rect(&self) -> Option<(usize, usize, usize, usize)> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let ((ax, ay), (hx, hy)) = open.selection?;
        Some((ax.min(hx), ay.min(hy), ax.max(hx), ay.max(hy)))
    }

    /// The pixels of a sheet-space rect, row-major (pad cells read as 0).
    fn region_pixels(
        &self,
        (x0, y0, x1, y1): (usize, usize, usize, usize),
    ) -> (usize, usize, Vec<u8>) {
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let mut data = vec![0u8; w * h];
        let ViewerState::Ready(open) = &self.state else {
            return (w, h, data);
        };
        let state = open.store.state();
        for dy in 0..h {
            for dx in 0..w {
                if let Some((tile, px_x, px_y)) = self.doc_pixel(x0 + dx, y0 + dy) {
                    let idx = tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS
                        + px_y * TILE_PX
                        + px_x;
                    if let Some(&v) = state.indices.get(idx) {
                        data[dy * w + dx] = v;
                    }
                }
            }
        }
        (w, h, data)
    }

    /// Copy the selected region into the internal clipboard, sampling the
    /// doc through sheet coordinates (pad cells copy as 0).
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(rect) = self.selection_rect() else {
            return;
        };
        let region = self.region_pixels(rect);
        if let ViewerState::Ready(open) = &mut self.state {
            open.clipboard = Some(region);
            cx.notify();
        }
    }

    /// Write per-point colours as ONE undo step; later cells win.
    fn write_cells(&mut self, cells: &[(i32, i32, u8)], cx: &mut Context<Self>) {
        let writes: Vec<(usize, usize, usize, u8)> = cells
            .iter()
            .filter(|(x, y, _)| *x >= 0 && *y >= 0)
            .filter_map(|&(x, y, c)| {
                self.doc_pixel(x as usize, y as usize)
                    .map(|(tile, px_x, px_y)| (tile, px_x, px_y, c))
            })
            .collect();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if writes.is_empty() {
            return;
        }
        open.store.begin_stroke();
        for (tile, x, y, color) in writes {
            open.store.apply_stroke_paint(tile, x, y, color);
        }
        open.store.end_stroke();
        self.recompose_grid(cx);
    }

    /// Move the marquee's pixels by `(dx, dy)`: the source is cleared to
    /// slot 0, the destination written over it, one undo step; the
    /// marquee follows.
    fn move_selection(&mut self, (dx, dy): (i32, i32), cx: &mut Context<Self>) {
        let Some((x0, y0, x1, y1)) = self.selection_rect() else {
            return;
        };
        if (dx, dy) == (0, 0) {
            return;
        }
        let (w, h, data) = self.region_pixels((x0, y0, x1, y1));
        let mut cells = Vec::with_capacity(w * h * 2);
        for row in 0..h {
            for col in 0..w {
                cells.push(((x0 + col) as i32, (y0 + row) as i32, 0));
            }
        }
        for row in 0..h {
            for col in 0..w {
                cells.push((
                    (x0 + col) as i32 + dx,
                    (y0 + row) as i32 + dy,
                    data[row * w + col],
                ));
            }
        }
        self.write_cells(&cells, cx);
        if let ViewerState::Ready(open) = &mut self.state {
            let (sheet_w, sheet_h) = (open.grid_size.0 as i32, open.grid_size.1 as i32);
            let landed = x1 as i32 + dx >= 0
                && y1 as i32 + dy >= 0
                && x0 as i32 + dx < sheet_w
                && y0 as i32 + dy < sheet_h;
            let clamp = |v: usize, d: i32, max: i32| (v as i32 + d).clamp(0, max - 1) as usize;
            open.selection = landed.then(|| {
                (
                    (clamp(x0, dx, sheet_w), clamp(y0, dy, sheet_h)),
                    (clamp(x1, dx, sheet_w), clamp(y1, dy, sheet_h)),
                )
            });
        }
    }

    /// Flip the marquee's pixels in place, one undo step.
    fn flip_selection(&mut self, horizontal: bool, cx: &mut Context<Self>) {
        let Some((x0, y0, x1, y1)) = self.selection_rect() else {
            return;
        };
        let (w, h, data) = self.region_pixels((x0, y0, x1, y1));
        let mut cells = Vec::with_capacity(w * h);
        for row in 0..h {
            for col in 0..w {
                let (sc, sr) = if horizontal {
                    (w - 1 - col, row)
                } else {
                    (col, h - 1 - row)
                };
                cells.push(((x0 + col) as i32, (y0 + row) as i32, data[sr * w + sc]));
            }
        }
        self.write_cells(&cells, cx);
    }

    /// Remove the top or bottom row (the edge "−" bars) -- one undo step.
    fn delete_row(&mut self, at_top: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let cols = open.cols;
        open.store
            .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::DeleteRow { cols, at_top });
        open.selection = None;
        self.recompose_grid(cx);
    }

    /// Remove the left or right column; the view narrows with it (the
    /// same doc/view split as `insert_column`: undo restores the strip but
    /// the view keeps the narrower cols, so tiles rewrap).
    fn delete_column(&mut self, at_left: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let cols = open.cols;
        if cols <= 1 {
            return;
        }
        open.store
            .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::DeleteColumn { cols, at_left });
        open.cols = cols - 1;
        open.selection = None;
        self.recompose_grid(cx);
        self.write_view_meta();
    }

    /// Paste the clipboard at the selection's top-left as ONE stroke (one
    /// undo step). Pixels landing on pad cells are skipped; the selection
    /// moves to the pasted rect.
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some((x0, y0, ..)) = self.selection_rect() else {
            return;
        };
        let clipboard = match &self.state {
            ViewerState::Ready(open) => open.clipboard.clone(),
            _ => return,
        };
        let Some((w, h, data)) = clipboard else {
            return;
        };
        let mut writes = Vec::new();
        for dy in 0..h {
            for dx in 0..w {
                if let Some((tile, px_x, px_y)) = self.doc_pixel(x0 + dx, y0 + dy) {
                    writes.push((tile, px_x, px_y, data[dy * w + dx]));
                }
            }
        }
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if writes.is_empty() {
            return;
        }
        open.store.begin_stroke();
        for (tile, px_x, px_y, color) in writes {
            open.store.apply_stroke_paint(tile, px_x, px_y, color);
        }
        open.store.end_stroke();
        open.selection = Some(((x0, y0), (x0 + w - 1, y0 + h - 1)));
        self.recompose_grid(cx);
    }

    /// Insert a blank row at the sheet's top or bottom (the edge "+"
    /// bars) -- one undo step; the store pads a partial last row first.
    fn insert_row(&mut self, at_top: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let cols = open.cols;
        open.store
            .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::InsertRow { cols, at_top });
        self.recompose_grid(cx);
    }

    /// Insert a blank column at the sheet's left or right (the edge "+"
    /// bars) -- one undo step in the doc; the view's column count grows
    /// with it (and persists). NOTE the asymmetry with undo: ctrl-z
    /// restores the strip but the view keeps the wider cols, so the last
    /// column's tiles rewrap -- doc state and view settings are separate
    /// by design.
    fn insert_column(&mut self, at_left: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let cols = open.cols;
        open.store
            .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::InsertColumn { cols, at_left });
        if let ViewerState::Ready(open) = &mut self.state {
            open.cols = cols + 1;
        }
        self.recompose_grid(cx);
        self.write_view_meta();
    }

    /// Toggle the tile-boundary lines (persisted per view).
    fn toggle_lines(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.show_lines = !open.show_lines;
            cx.notify();
            self.write_view_meta();
        }
    }

    /// Ctrl-A: equip the Select tool and select the whole sheet.
    fn select_whole_sheet(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let (w, h) = open.grid_size;
        if w == 0 || h == 0 {
            return;
        }
        open.tool = Tool::Select;
        open.selection = Some(((0, 0), (w as usize - 1, h as usize - 1)));
        cx.notify();
    }

    /// Release: a shape drag commits (shift = filled); any other gesture
    /// closes its stroke.
    fn on_sheet_mouse_up(&mut self, shift: bool, cx: &mut Context<Self>) {
        let (shape, moved) = match &mut self.state {
            ViewerState::Ready(open) if open.painting => {
                open.painting = false;
                let points = open.shape_points(shift);
                open.shape_drag = None;
                let moved = open.move_drag.is_some().then(|| open.move_offset());
                open.move_drag = None;
                if open.tool.is_shape() {
                    (Some((points, open.paint_color())), moved)
                } else {
                    open.store.end_stroke();
                    (None, moved)
                }
            }
            _ => return,
        };
        if let Some((points, color)) = shape {
            self.write_points(&points, color, cx);
        }
        if let Some(offset) = moved {
            self.move_selection(offset, cx);
        }
        cx.notify();
    }

    fn step_brush(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.brush = (open.brush as isize + delta).clamp(MIN_BRUSH as isize, MAX_BRUSH as isize)
                as usize;
            cx.notify();
        }
    }

    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.tool != tool
        {
            open.tool = tool;
            cx.notify();
        }
    }

    fn select_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if slot < PAL_SLOTS && open.slot != slot {
            open.slot = slot;
            // Picking a color is an intent to draw with it.
            open.tool = Tool::Pencil;
            cx.notify();
        }
    }

    /// Set palette slot `slot` to `rgb565` -- an undoable
    /// `TilesetOp::SetPalette`, recoloring the whole sheet. Slot 0 is the
    /// transparent index and is locked; out-of-range slots are rejected
    /// here because the store's `SetPalette` arm indexes the palette raw.
    fn set_palette_slot(&mut self, slot: usize, rgb565: u16, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if slot == 0 || slot >= PAL_SLOTS || open.store.state().palette[slot] == rgb565 {
            return;
        }
        open.store.apply_palette_coalesced(slot, rgb565);
        self.recompose_grid(cx);
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open.store.undo() {
            self.recompose_grid(cx);
        }
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open.store.redo() {
            self.recompose_grid(cx);
        }
    }

    /// Write the `.til` + `.pal` pair back through worldlib's atomic
    /// save. Synchronous by choice, same reasoning as the sprite panel's
    /// save. A failure keeps the document dirty and surfaces on the panel
    /// (and as the item's save Err).
    pub(crate) fn save_impl(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(root) = self.project_root.clone() else {
            return;
        };
        let state = open.store.state();
        match save_tileset(
            &root,
            &open.rel_path,
            &state.indices,
            state.tile_count,
            &state.palette,
        ) {
            Ok(()) => {
                open.store.mark_saved();
                open.save_error = None;
            }
            Err(e) => {
                open.save_error = Some(e.to_string());
            }
        }
        cx.notify();
    }

    /// Test-only: apply one Paint op directly (the item test drives dirty
    /// state without window-level mouse simulation).
    #[cfg(test)]
    pub(crate) fn apply_paint_for_test(
        &mut self,
        tile: usize,
        x: usize,
        y: usize,
        color: u8,
        cx: &mut Context<Self>,
    ) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.store
                .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::Paint { tile, x, y, color });
            self.recompose_grid(cx);
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
                ggo_common::CopyableText::new("ggo-tileset-load-error-copy", message)
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

    /// The editing canvas: the composed sheet at the integer zoom,
    /// scrollable both ways, with the bounds-recording overlay and the
    /// paint mouse handlers.
    fn render_sheet(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_sheet is only called in the Ready state");
        };
        let (w, h) = open.zoomed_size();
        let bounds_cell = open.sheet_bounds.clone();
        let (cols, rows) = (open.cols, open.grid_size.1 as usize / TILE_PX);
        let show_lines = open.show_lines;
        let accent = cx.theme().colors().border_focused;
        let zoom = open.zoom as f32;
        let (move_dx, move_dy) = open.move_offset();
        let selection = self.selection_rect().map(|(x0, y0, x1, y1)| {
            let shift = |v: usize, d: i32| (v as i32 + d).max(0) as usize;
            (
                shift(x0, move_dx),
                shift(y0, move_dy),
                shift(x1, move_dx),
                shift(y1, move_dy),
            )
        });
        // The shape preview: what a release without shift would paint.
        let preview = open.shape_points(false);
        let preview_color = cx.theme().colors().text_accent;
        // `.top_0().left_0()` matters: an absolute child with auto insets
        // sits at its STATIC position -- after the in-flow img sibling --
        // so the recorded bounds would be shifted by exactly the image
        // (the `ggo_sprite_panel` preview-overlay lesson).
        let overlay = gpui::canvas(
            move |bounds, _window, _cx| {
                *bounds_cell.borrow_mut() = Some(bounds);
            },
            move |bounds, (), window, _cx| {
                if show_lines {
                    paint_tile_borders(bounds, cols, rows, window);
                }
                if let Some((x0, y0, x1, y1)) = selection {
                    let rect = Bounds::new(
                        gpui::point(
                            bounds.origin.x + px(x0 as f32 * zoom),
                            bounds.origin.y + px(y0 as f32 * zoom),
                        ),
                        gpui::size(
                            px((x1 - x0 + 1) as f32 * zoom),
                            px((y1 - y0 + 1) as f32 * zoom),
                        ),
                    );
                    window.paint_quad(gpui::outline(rect, accent, gpui::BorderStyle::Solid));
                }
                for &(x, y) in &preview {
                    if x < 0 || y < 0 {
                        continue;
                    }
                    let cell = Bounds::new(
                        gpui::point(
                            bounds.origin.x + px(x as f32 * zoom),
                            bounds.origin.y + px(y as f32 * zoom),
                        ),
                        gpui::size(px(zoom), px(zoom)),
                    );
                    window.paint_quad(gpui::fill(cell, preview_color));
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .id("ggo-tileset-sheet")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&open.scroll)
            .child(
                div().p_2().child(
                    v_flex()
                        .gap_1()
                        .child(self.edge_bar("ggo-tileset-add-top", px(w), true, true, cx))
                        .child(
                            h_flex()
                                .gap_1()
                                .items_stretch()
                                .child(self.edge_bar(
                                    "ggo-tileset-add-left",
                                    px(h),
                                    false,
                                    true,
                                    cx,
                                ))
                                .child(
                                    div()
                                        .relative()
                                        .w(px(w))
                                        .h(px(h))
                                        .child(
                                            img(open.grid.clone()).nearest(true).w(px(w)).h(px(h)),
                                        )
                                        .child(overlay)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                |this, event: &MouseDownEvent, window, cx| {
                                                    // Take focus so undo/save bindings apply.
                                                    window.focus(&this.focus_handle, cx);
                                                    if event.click_count >= 2 {
                                                        if let Some((tile, ..)) =
                                                            this.pixel_at(event.position)
                                                        {
                                                            this.enter_focus(tile, cx);
                                                        }
                                                        return;
                                                    }
                                                    this.on_sheet_mouse_down(event.position, cx);
                                                },
                                            ),
                                        )
                                        .on_mouse_move(cx.listener(
                                            |this, event: &MouseMoveEvent, _, cx| {
                                                this.on_sheet_mouse_move(event.position, cx);
                                            },
                                        ))
                                        .on_mouse_up(
                                            MouseButton::Left,
                                            cx.listener(|this, event: &MouseUpEvent, _, cx| {
                                                this.on_sheet_mouse_up(event.modifiers.shift, cx);
                                            }),
                                        )
                                        .on_mouse_up_out(
                                            MouseButton::Left,
                                            cx.listener(|this, event: &MouseUpEvent, _, cx| {
                                                this.on_sheet_mouse_up(event.modifiers.shift, cx);
                                            }),
                                        ),
                                )
                                .child(self.edge_bar(
                                    "ggo-tileset-add-right",
                                    px(h),
                                    false,
                                    false,
                                    cx,
                                )),
                        )
                        .child(self.edge_bar("ggo-tileset-add-bottom", px(w), true, false, cx)),
                ),
            )
            .into_any_element()
    }

    /// One dashed "+" bar along a sheet edge: clicking it grows the grid
    /// on that side (a row for the horizontal bars, a column for the
    /// vertical ones).
    fn edge_bar(
        &self,
        id: &'static str,
        length: Pixels,
        horizontal: bool,
        at_start: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if matches!(&self.state, ViewerState::Ready(open) if open.focus.is_some()) {
            return div().into_any_element();
        }
        let (add_tip, remove_tip) = match (horizontal, at_start) {
            (true, true) => ("Add row above", "Delete top row"),
            (true, false) => ("Add row below", "Delete bottom row"),
            (false, true) => ("Add column left", "Delete left column"),
            (false, false) => ("Add column right", "Delete right column"),
        };
        let half = |glyph: &'static str, tip: &'static str, index: usize| {
            div()
                .id((id, index))
                .flex_1()
                .flex()
                .justify_center()
                .items_center()
                .cursor_pointer()
                .tooltip(ui::Tooltip::text(tip))
                .child(Label::new(glyph).size(LabelSize::Small).color(Color::Muted))
        };
        div()
            .id(id)
            .flex()
            .when(horizontal, |el| el.flex_row().w(length).h(px(EDGE_BAR_PX)))
            .when(!horizontal, |el| el.flex_col().w(px(EDGE_BAR_PX)).h(length))
            .border_1()
            .border_dashed()
            .rounded_sm()
            .border_color(cx.theme().colors().border_variant)
            .child(
                half("+", add_tip, 0).on_click(cx.listener(move |this, _, _, cx| {
                    if horizontal {
                        this.insert_row(at_start, cx);
                    } else {
                        this.insert_column(at_start, cx);
                    }
                })),
            )
            .child(
                half("−", remove_tip, 1).on_click(cx.listener(move |this, _, _, cx| {
                    if horizontal {
                        this.delete_row(at_start, cx);
                    } else {
                        this.delete_column(at_start, cx);
                    }
                })),
            )
            .into_any_element()
    }

    /// The tooling column's header: source rels and the sheet summary.
    fn render_info(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_info is only called in the Ready state");
        };
        let (w, h) = open.grid_size;
        let state = open.store.state();
        let summary = format!(
            "{} tiles · {}x{} px · {} cols",
            state.tile_count, w, h, open.cols
        );
        v_flex()
            .gap_0p5()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(open.rel_path.clone()).size(LabelSize::Small))
            .child(
                Label::new(open.pal_path.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(summary)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// The toolbar across the top of the center view: pencil/eraser
    /// toggle and zoom on the left, undo/redo/save on the right.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_toolbar is only called in the Ready state");
        };
        let zoom_label = format!("{}x", open.zoom);
        h_flex()
            .gap_1()
            .items_center()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("ggo-tileset-pencil", IconName::Pencil)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.tool == Tool::Pencil)
                    .tooltip(ui::Tooltip::text("Pencil"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Pencil, cx))),
            )
            .child(
                IconButton::new("ggo-tileset-eraser", IconName::Eraser)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.tool == Tool::Eraser)
                    .tooltip(ui::Tooltip::text("Eraser (paints transparent)"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Eraser, cx))),
            )
            .child(
                IconButton::new("ggo-tileset-picker", IconName::Crosshair)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.tool == Tool::Picker)
                    .tooltip(ui::Tooltip::text("Pick color from the sheet"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Picker, cx))),
            )
            .child(
                IconButton::new("ggo-tileset-select", IconName::SquareDot)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.tool == Tool::Select)
                    .tooltip(ui::Tooltip::text(
                        "Select region (ctrl-c copy, ctrl-v paste)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Select, cx))),
            )
            .children(
                [
                    (
                        Tool::Fill,
                        "ggo-tileset-fill",
                        IconName::Sparkle,
                        "Fill (floods across tiles)",
                    ),
                    (Tool::Line, "ggo-tileset-line", IconName::Dash, "Line"),
                    (
                        Tool::Rect,
                        "ggo-tileset-rect",
                        IconName::Maximize,
                        "Rectangle (shift = filled)",
                    ),
                    (
                        Tool::Ellipse,
                        "ggo-tileset-ellipse",
                        IconName::Circle,
                        "Ellipse (shift = filled)",
                    ),
                ]
                .map(|(tool, id, icon, tip)| {
                    IconButton::new(id, icon)
                        .icon_size(IconSize::Small)
                        .toggle_state(open.tool == tool)
                        .tooltip(ui::Tooltip::text(tip))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_tool(tool, cx)))
                }),
            )
            .child(div().w_2())
            .child(
                IconButton::new("ggo-tileset-brush-down", IconName::Dash)
                    .icon_size(IconSize::XSmall)
                    .disabled(open.brush <= MIN_BRUSH)
                    .tooltip(ui::Tooltip::text("Smaller brush ([)"))
                    .on_click(cx.listener(|this, _, _, cx| this.step_brush(-1, cx))),
            )
            .child(Label::new(format!("{}px", open.brush)).size(LabelSize::XSmall))
            .child(
                IconButton::new("ggo-tileset-brush-up", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .disabled(open.brush >= MAX_BRUSH)
                    .tooltip(ui::Tooltip::text("Larger brush (])"))
                    .on_click(cx.listener(|this, _, _, cx| this.step_brush(1, cx))),
            )
            .child(
                IconButton::new("ggo-tileset-mirror-h", IconName::ArrowRightLeft)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.mirror_h)
                    .tooltip(ui::Tooltip::text("Mirror horizontally within the tile"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &mut this.state {
                            open.mirror_h = !open.mirror_h;
                            cx.notify();
                        }
                    })),
            )
            .child(
                IconButton::new("ggo-tileset-mirror-v", IconName::ExpandVertical)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.mirror_v)
                    .tooltip(ui::Tooltip::text("Mirror vertically within the tile"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &mut this.state {
                            open.mirror_v = !open.mirror_v;
                            cx.notify();
                        }
                    })),
            )
            .child(div().w_2())
            .child(
                Button::new("ggo-tileset-flip-h", "Flip H")
                    .disabled(open.selection.is_none())
                    .tooltip(ui::Tooltip::text(
                        "Flip the selection horizontally (shift-h)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.flip_selection(true, cx))),
            )
            .child(
                Button::new("ggo-tileset-flip-v", "Flip V")
                    .disabled(open.selection.is_none())
                    .tooltip(ui::Tooltip::text("Flip the selection vertically (shift-v)"))
                    .on_click(cx.listener(|this, _, _, cx| this.flip_selection(false, cx))),
            )
            .child(div().w_2())
            .child(match open.focus {
                Some(tile) => Button::new("ggo-tileset-focus", format!("Tile {tile} — Back"))
                    .tooltip(ui::Tooltip::text("Leave focus (escape); ←/→ step tiles"))
                    .on_click(cx.listener(|this, _, _, cx| this.leave_focus(cx))),
                None => Button::new("ggo-tileset-focus", "Focus")
                    .tooltip(ui::Tooltip::text(
                        "Magnify one tile (f, or double-click a tile)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.focus_tile_impl(cx))),
            })
            .child(div().w_2())
            .child(Label::new(zoom_label).size(LabelSize::XSmall))
            .child(
                ui::Slider::new(
                    "ggo-tileset-zoom",
                    ui::slider_fraction(open.zoom, MIN_ZOOM, MAX_ZOOM),
                )
                .width(px(72.))
                .on_change({
                    let weak = cx.weak_entity();
                    move |value, _window, cx| {
                        let zoom = ui::slider_step(value, MIN_ZOOM, MAX_ZOOM);
                        weak.update(cx, |this, cx| this.set_zoom(zoom, cx)).ok();
                    }
                }),
            )
            .child(
                IconButton::new("ggo-tileset-lines", IconName::Hash)
                    .icon_size(IconSize::XSmall)
                    .toggle_state(open.show_lines)
                    .tooltip(ui::Tooltip::text("Toggle tile boundary lines"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_lines(cx))),
            )
            .child(div().w_2())
            .child(
                Label::new("Cols")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new("ggo-tileset-cols-down", IconName::Dash)
                    .icon_size(IconSize::XSmall)
                    .disabled(open.cols <= 1)
                    .tooltip(ui::Tooltip::text("Narrower grid"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &this.state {
                            let cols = open.cols;
                            this.set_cols(cols.saturating_sub(1), cx);
                        }
                    })),
            )
            .child(Label::new(format!("{}", open.cols)).size(LabelSize::XSmall))
            .child(
                IconButton::new("ggo-tileset-cols-up", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .disabled(open.cols >= open.store.state().tile_count.max(1))
                    .tooltip(ui::Tooltip::text("Wider grid"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &this.state {
                            let cols = open.cols;
                            this.set_cols(cols + 1, cx);
                        }
                    })),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("ggo-tileset-undo", IconName::Undo)
                    .icon_size(IconSize::XSmall)
                    .tooltip(ui::Tooltip::text("Undo"))
                    .on_click(cx.listener(|this, _, _, cx| this.undo_impl(cx))),
            )
            .child(
                IconButton::new("ggo-tileset-redo", IconName::RotateCw)
                    .icon_size(IconSize::XSmall)
                    .tooltip(ui::Tooltip::text("Redo"))
                    .on_click(cx.listener(|this, _, _, cx| this.redo_impl(cx))),
            )
            .child(
                Button::new("ggo-tileset-save", "Save")
                    .disabled(!open.store.dirty())
                    .tooltip(ui::Tooltip::text("Save (.til + .pal)"))
                    .on_click(cx.listener(|this, _, _, cx| this.save_impl(cx))),
            )
            .into_any_element()
    }

    /// The 16-slot palette section: [`SmallPaletteEditor`], the compact
    /// side-column widget -- swatch selection plus per-channel steppers
    /// for the selected slot, whose changes land as undoable
    /// `SetPalette` ops through [`Self::set_palette_slot`].
    fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_palette is only called in the Ready state");
        };
        let select_target = cx.weak_entity();
        let change_target = cx.weak_entity();
        let mut editor = SmallPaletteEditor::new(open.store.state().palette, open.slot);
        if open.missing_pal {
            editor = editor.note("no .pal found — 16-gray fallback");
        }
        editor
            .on_select(move |slot, _, cx| {
                select_target
                    .update(cx, |this, cx| this.select_slot(slot, cx))
                    .ok();
            })
            .on_change(move |slot, rgb565, _, cx| {
                change_target
                    .update(cx, |this, cx| this.set_palette_slot(slot, rgb565, cx))
                    .ok();
            })
            .into_any_element()
    }

    /// The document column on the editor's right side: file info, the
    /// palette editor, and any save error.
    fn render_tooling(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_tooling is only called in the Ready state");
        };
        let save_error = open.save_error.clone();
        v_flex()
            .w(px(TOOLS_COL_PX))
            .h_full()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .child(self.render_info(cx))
            .child(self.render_palette(cx))
            .when_some(save_error, |this, e| {
                this.child(div().p_1().child(ggo_common::CopyableText::new(
                    "ggo-tileset-save-error-copy",
                    format!("Save failed: {e}"),
                )))
            })
            .into_any_element()
    }

    /// "Source changed — Re-import…": hands the `.til` to the import panel
    /// through `ggo_common::ReimportTileset` (no crate cycle).
    fn render_import_alert(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let alert = open.import_alert.clone()?;
        let til_rel = open.rel_path.clone();
        let text = if alert.missing {
            format!("Import source {} is missing", alert.source)
        } else {
            format!("Import source {} changed", alert.source)
        };
        Some(
            h_flex()
                .gap_1()
                .px_1()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new(text)
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                )
                .when(!alert.missing, |el| {
                    el.child(Button::new("ggo-tileset-reimport", "Re-import…").on_click(
                        move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(ggo_common::ReimportTileset {
                                    til_rel: til_rel.clone(),
                                }),
                                cx,
                            );
                        },
                    ))
                })
                .child(
                    Button::new("ggo-tileset-reimport-dismiss", "Dismiss").on_click(cx.listener(
                        |this, _, _, cx| {
                            if let ViewerState::Ready(open) = &mut this.state {
                                open.import_alert = None;
                                cx.notify();
                            }
                        },
                    )),
                )
                .into_any_element(),
        )
    }

    fn render_ready(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .child(self.render_toolbar(cx))
            .children(self.render_import_alert(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_sheet(cx))
                    .child(self.render_tooling(cx)),
            )
            .into_any_element()
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.state {
            ViewerState::Empty => {
                self.render_message("Open a .til file from the project panel".to_string(), cx)
            }
            ViewerState::Loading { rel_path } => {
                self.render_message(format!("Loading {rel_path}…"), cx)
            }
            ViewerState::Error(e) => self.render_load_error(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(cx),
        }
    }
}

impl Render for TilesetPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.render_body(cx);
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &Undo, _window, cx| this.undo_impl(cx)))
            .on_action(cx.listener(|this, _: &Redo, _window, cx| this.redo_impl(cx)))
            .on_action(cx.listener(|this, _: &Save, _window, cx| this.save_impl(cx)))
            .on_action(
                cx.listener(|this, _: &ScrollLeft, _window, cx| this.scroll_by(-1.0, 0.0, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ScrollRight, _window, cx| this.scroll_by(1.0, 0.0, cx)),
            )
            .on_action(cx.listener(|this, _: &ScrollUp, _window, cx| this.scroll_by(0.0, -1.0, cx)))
            .on_action(
                cx.listener(|this, _: &ScrollDown, _window, cx| this.scroll_by(0.0, 1.0, cx)),
            )
            .on_action(cx.listener(|this, _: &CopySelection, _window, cx| this.copy_selection(cx)))
            .on_action(
                cx.listener(|this, _: &PasteSelection, _window, cx| this.paste_clipboard(cx)),
            )
            .on_action(cx.listener(|this, _: &ZoomIn, _window, cx| this.zoom_by(1, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _window, cx| this.zoom_by(-1, cx)))
            .on_action(cx.listener(|this, _: &BrushSmaller, _window, cx| this.step_brush(-1, cx)))
            .on_action(cx.listener(|this, _: &BrushLarger, _window, cx| this.step_brush(1, cx)))
            .on_action(
                cx.listener(|this, _: &FlipHorizontal, _window, cx| this.flip_selection(true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FlipVertical, _window, cx| this.flip_selection(false, cx)),
            )
            .on_action(cx.listener(|this, _: &Cancel, _window, cx| this.cancel_impl(cx)))
            .on_action(cx.listener(|this, _: &FocusTile, _window, cx| this.focus_tile_impl(cx)))
            .on_action(
                cx.listener(|this, _: &SelectWholeSheet, _window, cx| this.select_whole_sheet(cx)),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for TilesetPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Kept for the item wrapper's observe plumbing (the panel emits nothing
/// itself; `cx.observe` drives tab updates).
pub enum TilesetPanelEvent {}

impl EventEmitter<TilesetPanelEvent> for TilesetPanel {}

/// Paint tile-boundary lines over the zoomed sheet: a 1px white line
/// centered in a 3px black one, so the boundary reads against any tile
/// art (`ggo_sprite_panel::paint_tile_grid`'s idiom, thickened).
fn paint_tile_borders(bounds: Bounds<Pixels>, cols: usize, rows: usize, window: &mut Window) {
    if cols == 0 || rows == 0 {
        return;
    }
    let thick = px(3.);
    let thin = px(1.);
    for i in 0..=cols {
        let x = (bounds.origin.x + bounds.size.width * (i as f32 / cols as f32))
            .min(bounds.origin.x + bounds.size.width - thin);
        window.paint_quad(gpui::fill(
            Bounds::new(
                gpui::point((x - thin).max(bounds.origin.x), bounds.origin.y),
                gpui::size(thick, bounds.size.height),
            ),
            gpui::black(),
        ));
        window.paint_quad(gpui::fill(
            Bounds::new(
                gpui::point(x, bounds.origin.y),
                gpui::size(thin, bounds.size.height),
            ),
            gpui::white(),
        ));
    }
    for i in 0..=rows {
        let y = (bounds.origin.y + bounds.size.height * (i as f32 / rows as f32))
            .min(bounds.origin.y + bounds.size.height - thin);
        window.paint_quad(gpui::fill(
            Bounds::new(
                gpui::point(bounds.origin.x, (y - thin).max(bounds.origin.y)),
                gpui::size(bounds.size.width, thick),
            ),
            gpui::black(),
        ));
        window.paint_quad(gpui::fill(
            Bounds::new(
                gpui::point(bounds.origin.x, y),
                gpui::size(bounds.size.width, thin),
            ),
            gpui::white(),
        ));
    }
}

/// The doc pixel `(tile, x, y)` for sheet pixel `(sx, sy)`: in focus mode
/// the sheet IS one tile; otherwise it is the `cols`-wide grid, and pad
/// cells past `tile_count` are `None`.
fn sheet_to_doc(
    focus: Option<usize>,
    cols: usize,
    tile_count: usize,
    sx: usize,
    sy: usize,
) -> Option<(usize, usize, usize)> {
    match focus {
        Some(tile) => (sx < TILE_PX && sy < TILE_PX && tile < tile_count).then_some((tile, sx, sy)),
        None => {
            if cols == 0 || sx >= cols * TILE_PX {
                return None;
            }
            let tile = (sy / TILE_PX) * cols + sx / TILE_PX;
            (tile < tile_count).then_some((tile, sx % TILE_PX, sy % TILE_PX))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::io::{open_tileset, save_tileset};
    use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
    use gpui::{Entity, TestAppContext, point, size};
    use project::{FakeFs, Project, WorktreeId};
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// The fixture tile count: 3 tiles is deliberately NOT a multiple of
    /// the 8-column fallback, so `grid_cols`'s short-sheet clamp is
    /// exercised end to end.
    const FIXTURE_TILES: usize = 3;

    /// Author a real `.til`/`.pal` pair via worldlib's own `save_tileset`
    /// (one call writes both). Tile 0 stays all index 0 (transparent),
    /// tiles 1 and 2 are solid palette index 1 (red) -- so the composed
    /// grid is provably non-blank and the transparent/opaque split is
    /// visible in the pixels.
    fn write_tileset_fixture(root: &std::path::Path, stem: &str) {
        let mut indices = vec![0u8; FIXTURE_TILES * TILE_PIXELS];
        for b in &mut indices[TILE_PIXELS..] {
            *b = 1;
        }
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800; // pure 565 red
        save_tileset(
            root,
            &format!("tiles/{stem}.til"),
            &indices,
            FIXTURE_TILES,
            &palette,
        )
        .unwrap();
    }

    /// Load the fixture tileset into a fresh panel and return it Ready.
    async fn ready_panel(cx: &mut TestAppContext, root: &std::path::Path) -> Entity<TilesetPanel> {
        write_tileset_fixture(root, "world");
        let root = root.to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
        });
        cx.executor().run_until_parked();
        panel
    }

    fn ready(panel: &TilesetPanel) -> &OpenTileset {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
    }

    /// Record sheet bounds as if the sheet were painted at the window
    /// origin at the CURRENT zoom -- what the canvas overlay does in a
    /// real window, minus the window.
    fn place_sheet_at_origin(panel: &Entity<TilesetPanel>, cx: &mut TestAppContext) {
        panel.update(cx, |panel, _| {
            let open = ready(panel);
            let (w, h) = open.zoomed_size();
            *open.sheet_bounds.borrow_mut() =
                Some(gpui::bounds(point(px(0.), px(0.)), size(px(w), px(h))));
        });
    }

    /// The window position of doc pixel `(tile, x, y)`'s center, for a
    /// sheet placed at the origin.
    fn pixel_pos(open: &OpenTileset, tile: usize, x: usize, y: usize) -> Point<Pixels> {
        let z = open.zoom as f32;
        let sx = ((tile % open.cols) * TILE_PX + x) as f32;
        let sy = ((tile / open.cols) * TILE_PX + y) as f32;
        point(px((sx + 0.5) * z), px((sy + 0.5) * z))
    }

    /// End-to-end load against a real-fs temp project: opening the
    /// fixture `.til` by rel path runs the off-thread loader and the panel
    /// reaches Ready with the expected doc state, the clamped column
    /// count, the fixture's own palette (not the grayscale fallback), the
    /// derived `.pal` rel, and a non-empty composed grid whose pixels
    /// prove tile 0 transparent / tile 1 red through the BGRA bridge.
    #[gpui::test]
    async fn test_open_til_reaches_ready_with_a_composed_grid(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            let state = open.store.state();
            assert_eq!(open.rel_path, "tiles/world.til");
            assert_eq!(open.pal_path, "tiles/world.pal");
            assert_eq!(state.tile_count, FIXTURE_TILES);
            assert!(!open.missing_pal, "the fixture wrote a .pal");
            assert_eq!(state.palette[1], 0xF800);
            assert!(!state.dirty, "freshly opened is clean");
            assert_eq!(
                open.cols, FIXTURE_TILES,
                "a sheet shorter than one 8-col row lays out at its own width"
            );
            assert_eq!(
                open.grid_size,
                ((FIXTURE_TILES * TILE_PX) as u32, TILE_PX as u32)
            );
            assert_eq!(open.zoom, DEFAULT_ZOOM);
            assert_eq!(open.tool, Tool::Pencil);
            assert_eq!(open.slot, 1);

            let bytes = open.grid.as_bytes(0).unwrap();
            assert_eq!(
                bytes.len(),
                FIXTURE_TILES * TILE_PIXELS * 4,
                "the grid image must not be empty"
            );
            // Row 0: tile 0's 16 px transparent, then tiles 1-2 opaque red
            // (BGRA, so red is [0, 0, 255, 255]).
            assert!(
                bytes[..TILE_PX * 4].chunks_exact(4).all(|p| p[3] == 0),
                "tile 0 (all index 0) must be transparent"
            );
            assert!(
                bytes[TILE_PX * 4..FIXTURE_TILES * TILE_PX * 4]
                    .chunks_exact(4)
                    .all(|p| p == [0, 0, 255, 255]),
                "tiles 1-2 (palette index 1) must be opaque red in BGRA"
            );
        });
    }

    /// A `.til` with no companion `.pal` still loads -- worldlib swaps in
    /// its 16-gray fallback and flags it, which the tooling column
    /// surfaces.
    #[gpui::test]
    async fn test_missing_pal_still_loads_with_the_fallback_palette(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "world");
        std::fs::remove_file(dir.path().join("tiles/world.pal")).unwrap();
        let root = dir.path().to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            assert!(open.missing_pal);
            assert_eq!(open.store.state().tile_count, FIXTURE_TILES);
        });
    }

    /// A bad `.til` lands in Error, not a panic -- and the panel stays
    /// usable.
    #[gpui::test]
    async fn test_a_malformed_til_reports_an_error(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tiles")).unwrap();
        // One byte short of a whole tile.
        std::fs::write(dir.path().join("tiles/bad.til"), vec![0u8; 127]).unwrap();
        let root = dir.path().to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/bad.til", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(&panel.state, ViewerState::Error(_)),
                "a malformed .til must surface as Error"
            );
        });
    }

    /// Zoom is integer and clamped at both ends.
    #[gpui::test]
    async fn test_zoom_steps_and_clamps(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.zoom_by(1, cx);
            assert_eq!(ready(panel).zoom, DEFAULT_ZOOM + 1);
            for _ in 0..40 {
                panel.zoom_by(1, cx);
            }
            assert_eq!(ready(panel).zoom, MAX_ZOOM);
            let (w, _) = ready(panel).zoomed_size();
            assert_eq!(w, (FIXTURE_TILES * TILE_PX * MAX_ZOOM) as f32);
            for _ in 0..40 {
                panel.zoom_by(-1, cx);
            }
            assert_eq!(ready(panel).zoom, MIN_ZOOM);
        });
    }

    // --------------------------------------------------------- painting

    /// The pencil paints the selected palette slot at the pixel under the
    /// mouse, through the doc store (dirty flips, undo works) and into
    /// the recomposed sheet image. The zoom scales the hit-test.
    #[gpui::test]
    async fn test_pencil_paints_the_selected_slot_under_the_mouse(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);

        panel.update(cx, |panel, cx| {
            // Paint tile 0's pixel (3, 2) with slot 1 (red).
            let pos = pixel_pos(ready(panel), 0, 3, 2);
            panel.on_sheet_mouse_down(pos, cx);
            panel.on_sheet_mouse_up(false, cx);

            let open = ready(panel);
            let state = open.store.state();
            assert_eq!(state.indices[2 * TILE_PX + 3], 1, "the doc pixel landed");
            assert!(state.dirty, "painting dirties the document");
            let bytes = open.grid.as_bytes(0).unwrap();
            let px_off = (2 * (FIXTURE_TILES * TILE_PX) + 3) * 4;
            assert_eq!(
                &bytes[px_off..px_off + 4],
                &[0, 0, 255, 255],
                "the recomposed sheet shows the painted pixel"
            );

            panel.undo_impl(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[2 * TILE_PX + 3], 0, "undo reverts the paint");
            assert!(!state.dirty, "undo back to saved is clean");
            panel.redo_impl(cx);
            assert_eq!(ready(panel).store.state().indices[2 * TILE_PX + 3], 1);
        });
    }

    /// Drag-painting: pixels visited while the button is held are painted;
    /// after mouse-up, moves paint nothing.
    #[gpui::test]
    async fn test_drag_paints_and_release_stops(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);

        panel.update(cx, |panel, cx| {
            let down = pixel_pos(ready(panel), 0, 0, 0);
            let drag = pixel_pos(ready(panel), 0, 1, 0);
            let after = pixel_pos(ready(panel), 0, 2, 0);
            panel.on_sheet_mouse_down(down, cx);
            panel.on_sheet_mouse_move(drag, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.on_sheet_mouse_move(after, cx);

            let state = ready(panel).store.state();
            assert_eq!(state.indices[0], 1, "the mouse-down pixel painted");
            assert_eq!(state.indices[1], 1, "the dragged-over pixel painted");
            assert_eq!(state.indices[2], 0, "a move after release paints nothing");

            // The whole drag is ONE undo step.
            panel.undo_impl(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[0], 0, "one undo reverts the whole stroke");
            assert_eq!(state.indices[1], 0);
            assert!(!state.dirty);

            // Two separate clicks are two steps.
            panel.on_sheet_mouse_down(down, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.on_sheet_mouse_down(drag, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.undo_impl(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[0], 1, "the first click survives");
            assert_eq!(state.indices[1], 0, "the second click undone alone");
        });
    }

    /// The eraser paints index 0 (transparent) whatever slot is selected,
    /// and clicking a swatch selects that slot AND switches back to the
    /// pencil.
    #[gpui::test]
    async fn test_eraser_paints_transparent_and_swatch_click_selects(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);

        panel.update(cx, |panel, cx| {
            // Tile 1 is solid index 1 in the fixture; erase one pixel.
            panel.set_tool(Tool::Eraser, cx);
            let pos = pixel_pos(ready(panel), 1, 5, 5);
            panel.on_sheet_mouse_down(pos, cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(
                state.indices[TILE_PIXELS + 5 * TILE_PX + 5],
                0,
                "the eraser writes index 0"
            );

            panel.select_slot(3, cx);
            let open = ready(panel);
            assert_eq!(open.slot, 3);
            assert_eq!(
                open.tool,
                Tool::Pencil,
                "picking a color switches back to the pencil"
            );
            // Out-of-range slot is a no-op.
            panel.select_slot(PAL_SLOTS + 5, cx);
            assert_eq!(ready(panel).slot, 3);
        });
    }

    /// Positions outside the sheet -- past its edge, or over the composed
    /// grid's trailing pad cells beyond `tile_count` -- paint nothing and
    /// never panic (`TilesetOp::Paint` indexes the buffer raw, so the
    /// guard lives in `pixel_at`).
    #[gpui::test]
    async fn test_positions_off_the_sheet_paint_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);

        panel.update(cx, |panel, cx| {
            let (w, h) = ready(panel).zoomed_size();
            panel.on_sheet_mouse_down(point(px(w + 10.), px(h + 10.)), cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert!(!state.dirty, "off-sheet clicks must not paint");
        });
    }

    /// Palette editing: a `SetPalette` op recolors the sheet (undoably),
    /// slot 0 is locked, and out-of-range slots are rejected instead of
    /// panicking (the store indexes the palette raw).
    #[gpui::test]
    async fn test_palette_edits_recolor_the_sheet_and_slot_zero_is_locked(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Tiles 1-2 are solid index 1 (red). Turn slot 1 green.
            panel.set_palette_slot(1, 0x07E0, cx);
            let open = ready(panel);
            let state = open.store.state();
            assert_eq!(state.palette[1], 0x07E0);
            assert!(state.dirty, "a palette edit dirties the document");
            let bytes = open.grid.as_bytes(0).unwrap();
            let px_off = TILE_PX * 4; // tile 1's first pixel, row 0
            assert_eq!(
                &bytes[px_off..px_off + 4],
                &[0, 255, 0, 255],
                "the sheet recomposed through the edited palette (BGRA green)"
            );

            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().palette[1],
                0xF800,
                "the edit is one undo step"
            );

            // A run of edits to the SAME slot (slider drag, +/- mashing)
            // coalesces: one undo spans the whole transition.
            panel.set_palette_slot(1, 0x0100, cx);
            panel.set_palette_slot(1, 0x0200, cx);
            panel.set_palette_slot(1, 0x07E0, cx);
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().palette[1],
                0xF800,
                "one undo spans the whole slider run"
            );

            panel.set_palette_slot(0, 0xFFFF, cx);
            assert_eq!(ready(panel).store.state().palette[0], 0, "slot 0 is locked");
            panel.set_palette_slot(PAL_SLOTS + 3, 0xFFFF, cx);
            assert!(
                !ready(panel).store.state().dirty,
                "no stray dirt from no-ops"
            );
        });
    }

    /// Save writes the `.til` + `.pal` pair through worldlib and clears
    /// the dirty flag; a reopen sees the painted pixel. A failed save
    /// keeps the document dirty and surfaces the error.
    #[gpui::test]
    async fn test_save_round_trips_and_failures_surface(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);

        panel.update(cx, |panel, cx| {
            let pos = pixel_pos(ready(panel), 0, 0, 0);
            panel.on_sheet_mouse_down(pos, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.save_impl(cx);
            let open = ready(panel);
            assert!(open.save_error.is_none(), "the fixture root is writable");
            assert!(!open.store.dirty(), "a landed save clears dirty");
        });
        let reopened = open_tileset(dir.path(), "tiles/world.til").unwrap();
        assert_eq!(reopened.indices[0], 1, "the painted pixel reached disk");

        // Break the save target: a root pointing at a regular FILE makes
        // the atomic write's create-parent-dirs step fail deterministically.
        panel.update(cx, |panel, cx| {
            let pos = pixel_pos(ready(panel), 0, 1, 0);
            panel.on_sheet_mouse_down(pos, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.project_root = Some(dir.path().join("tiles/world.til"));
            panel.save_impl(cx);
            let open = ready(panel);
            assert!(open.save_error.is_some(), "a failed write must surface");
            assert!(open.store.dirty(), "and the document must stay dirty");
        });
    }

    /// The grid-width setting: recomposes the sheet at the new column
    /// count, clamps to `1..=tile_count`, and persists -- a fresh panel
    /// on the same rel comes back with the stored cols AND zoom.
    #[gpui::test]
    async fn test_cols_setting_recomposes_clamps_and_persists(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.set_cols(1, cx);
            let open = ready(panel);
            assert_eq!(open.cols, 1);
            assert_eq!(
                open.grid_size,
                (TILE_PX as u32, (FIXTURE_TILES * TILE_PX) as u32),
                "one column stacks the three tiles vertically"
            );
            let bytes = open.grid.as_bytes(0).unwrap();
            assert_eq!(bytes.len(), FIXTURE_TILES * TILE_PIXELS * 4);
            // Row 0 is now tile 0 alone: fully transparent.
            assert!(
                bytes[..TILE_PX * 4].chunks_exact(4).all(|p| p[3] == 0),
                "the recomposed layout puts only tile 0 on row 0"
            );

            panel.set_cols(99, cx);
            assert_eq!(ready(panel).cols, FIXTURE_TILES, "clamped to tile_count");
            panel.set_cols(0, cx);
            assert_eq!(ready(panel).cols, 1, "clamped to 1");

            panel.set_cols(2, cx);
            panel.zoom_by(3, cx);
        });
        // The map panel keeps its terrains in the same sidecar; a later
        // view write must not wipe them.
        let mut meta = loader::load_view_meta(dir.path(), "tiles/world.til");
        meta.terrains = vec![ggo_worldlib::sprites::terrain::Terrain {
            name: "grass".into(),
            tiles: vec![],
        }];
        loader::save_view_meta(dir.path(), "tiles/world.til", &meta).unwrap();
        panel.update(cx, |panel, cx| panel.zoom_by(-1, cx));
        assert_eq!(
            loader::load_view_meta(dir.path(), "tiles/world.til").terrains[0].name,
            "grass"
        );

        // A fresh panel on the same rel restores the stored view settings.
        let root = dir.path().to_path_buf();
        let second = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        second.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
        });
        cx.executor().run_until_parked();
        second.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.cols, 2, "cols came back from the sidecar");
            assert_eq!(
                open.zoom,
                DEFAULT_ZOOM + 2,
                "zoom came back from the sidecar (3 up, then the 1 down above)"
            );
        });
    }

    /// Arrow-key camera panning: each step moves the scroll offset one
    /// tile at the current zoom, more negative as the view moves
    /// right/down, and clamps at the zero edge.
    #[gpui::test]
    async fn test_arrow_scroll_steps_one_tile_and_clamps_at_origin(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            let step = (TILE_PX * ready(panel).zoom) as f32;
            panel.scroll_by(1.0, 0.0, cx);
            panel.scroll_by(0.0, 2.0, cx);
            let offset = ready(panel).scroll.offset();
            assert_eq!(f32::from(offset.x), -step, "right = one tile negative x");
            assert_eq!(f32::from(offset.y), -2.0 * step, "down twice = two tiles");

            for _ in 0..10 {
                panel.scroll_by(-1.0, -1.0, cx);
            }
            let offset = ready(panel).scroll.offset();
            assert_eq!(f32::from(offset.x), 0.0, "clamped at the left edge");
            assert_eq!(f32::from(offset.y), 0.0, "clamped at the top edge");
        });
    }

    /// The Picker tool: clicking a pixel makes its palette slot the
    /// pencil color and switches back to the pencil.
    #[gpui::test]
    async fn test_picker_samples_the_slot_under_the_click(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);

        panel.update(cx, |panel, cx| {
            panel.select_slot(3, cx);
            panel.set_tool(Tool::Picker, cx);
            // Tile 1 is solid index 1 in the fixture.
            let pos = pixel_pos(ready(panel), 1, 4, 4);
            panel.on_sheet_mouse_down(pos, cx);
            let open = ready(panel);
            assert_eq!(open.slot, 1, "picked the slot under the click");
            assert_eq!(open.tool, Tool::Pencil, "picking returns to the pencil");
        });
    }

    /// Select tool marquee + copy/paste: drag selects a sheet-space rect,
    /// ctrl-c samples it, ctrl-v blits it at the selection anchor as ONE
    /// undo step, and the pasted pixels land in the doc.
    #[gpui::test]
    async fn test_marquee_copy_paste_is_one_undo_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);

        panel.update(cx, |panel, cx| {
            // Select tile 1's top-left 2x2 (solid index 1).
            panel.set_tool(Tool::Select, cx);
            let from = pixel_pos(ready(panel), 1, 0, 0);
            let to = pixel_pos(ready(panel), 1, 1, 1);
            panel.on_sheet_mouse_down(from, cx);
            panel.on_sheet_mouse_move(to, cx);
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(
                panel.selection_rect(),
                Some((TILE_PX, 0, TILE_PX + 1, 1)),
                "the marquee is a sheet-space rect over tile 1"
            );

            panel.copy_selection(cx);
            assert_eq!(
                ready(panel).clipboard,
                Some((2, 2, vec![1, 1, 1, 1])),
                "copy sampled the region"
            );

            // Move the selection onto tile 0 (all transparent) and paste.
            let target_from = pixel_pos(ready(panel), 0, 2, 2);
            panel.on_sheet_mouse_down(target_from, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.paste_clipboard(cx);
            {
                let state = ready(panel).store.state();
                assert_eq!(state.indices[2 * TILE_PX + 2], 1);
                assert_eq!(state.indices[2 * TILE_PX + 3], 1);
                assert_eq!(state.indices[3 * TILE_PX + 2], 1);
                assert_eq!(state.indices[3 * TILE_PX + 3], 1);
            }
            assert_eq!(
                panel.selection_rect(),
                Some((2, 2, 3, 3)),
                "the selection follows the pasted rect"
            );

            panel.undo_impl(cx);
            let state = ready(panel).store.state();
            assert_eq!(
                state.indices[2 * TILE_PX + 2],
                0,
                "one undo reverts the paste"
            );
            assert_eq!(state.indices[3 * TILE_PX + 3], 0);
            assert!(!state.dirty);

            // Paste with no clipboard or no selection: no-ops.
            panel.paste_clipboard(cx);
            panel.undo_impl(cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = None;
                open.clipboard = Some((1, 1, vec![5]));
            }
            panel.paste_clipboard(cx);
            assert!(!ready(panel).store.state().dirty);
        });
    }

    /// The edge "+" bars: rows grow the sheet top/bottom, columns grow
    /// it left/right (bumping the view's cols), each as one undo step.
    #[gpui::test]
    async fn test_edge_bars_grow_the_grid_on_each_side(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Fixture: 3 tiles at 3 cols. Bottom row first.
            panel.insert_row(false, cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().tile_count, 6);
                assert_eq!(
                    open.grid_size,
                    ((3 * TILE_PX) as u32, (2 * TILE_PX) as u32),
                    "a second row appeared"
                );
            }
            panel.undo_impl(cx);
            assert_eq!(ready(panel).store.state().tile_count, FIXTURE_TILES);

            // Top row: tile 0's red neighbors shift down one row.
            panel.insert_row(true, cx);
            {
                let state = ready(panel).store.state();
                assert_eq!(state.tile_count, 6);
                assert_eq!(state.indices[3 * TILE_PIXELS], 0, "row 0 is blank");
                assert_eq!(
                    state.indices[4 * TILE_PIXELS],
                    1,
                    "old tile 1 now sits in row 1"
                );
            }
            panel.undo_impl(cx);

            // Right column: view cols follows the wider grid.
            panel.insert_column(false, cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(open.cols, 4, "the view widened with the sheet");
                assert_eq!(state.tile_count, 4);
                assert_eq!(state.indices[3 * TILE_PIXELS], 0, "the new column is blank");
                assert_eq!(open.grid_size, ((4 * TILE_PX) as u32, TILE_PX as u32));
            }

            // Left column on the widened grid.
            panel.insert_column(true, cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(open.cols, 5);
                assert_eq!(state.tile_count, 5);
                assert_eq!(state.indices[0], 0, "slot 0 is the new blank column");
                assert_eq!(state.indices[TILE_PIXELS], 0, "old tile 0 was blank too");
                assert_eq!(
                    state.indices[2 * TILE_PIXELS],
                    1,
                    "old tile 1 shifted right"
                );
            }
        });
    }

    /// Ctrl-A equips the Select tool and selects the whole sheet; the
    /// lines toggle flips and persists to the view sidecar.
    #[gpui::test]
    async fn test_select_all_and_lines_toggle(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.select_whole_sheet(cx);
            let open = ready(panel);
            assert_eq!(open.tool, Tool::Select);
            assert_eq!(
                panel.selection_rect(),
                Some((0, 0, FIXTURE_TILES * TILE_PX - 1, TILE_PX - 1)),
                "the whole sheet is selected"
            );

            assert!(ready(panel).show_lines, "lines default on");
            panel.toggle_lines(cx);
            assert!(!ready(panel).show_lines);
        });

        // A fresh panel restores the stored toggle.
        let root = dir.path().to_path_buf();
        let second = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        second.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
        });
        cx.executor().run_until_parked();
        second.update(cx, |panel, _| {
            assert!(
                !ready(panel).show_lines,
                "the lines toggle came back from the sidecar"
            );
        });
    }

    // ------------------------------------------ explorer-driven routing

    /// A fake-fs project with one visible worktree holding the same file
    /// names the real-fs `root` fixture does: the interceptor only needs a
    /// worktree id and a rel path, while the item's panel loads the actual
    /// tileset bytes through `std::fs` from `root` (`root_override`).
    async fn routed_project(
        cx: &mut TestAppContext,
        root: &std::path::Path,
        run_init: bool,
    ) -> Entity<Project> {
        write_tileset_fixture(root, "world");
        write_tileset_fixture(root, "other");
        cx.update(|cx| {
            AppState::test(cx);
            if run_init {
                init(cx);
            }
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({ "tiles": { "world.til": "", "other.til": "" }, "notes.txt": "" }),
        )
        .await;
        Project::test(fs, ["/proj".as_ref()], cx).await
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

    fn project_path(worktree_id: WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    /// The registered `.til` predicate claims the path (so the project
    /// panel opens NO text buffer for it) and adds ONE center-pane editor
    /// tab; a re-click activates the existing tab instead of adding a
    /// second. A non-`.til` in the same worktree is declined.
    #[gpui::test]
    async fn test_til_click_opens_a_center_editor_tab(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "tiles/world.til"), window, cx)
        });
        assert!(claimed, "a .til must be claimed, suppressing the text item");
        cx.run_until_parked();

        let items: Vec<_> = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<TilesetEditorItem>(cx)
                .map(|item| item.read(cx).rel().to_string())
                .collect()
        });
        assert_eq!(items, vec!["tiles/world.til"], "one editor tab opened");

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "tiles/world.til"), window, cx)
        });
        assert!(claimed);
        cx.run_until_parked();
        let count = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<TilesetEditorItem>(cx).count()
        });
        assert_eq!(count, 1, "a re-click activates, never duplicates");

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "notes.txt"), window, cx)
        });
        assert!(!claimed, "everything but .til opens the normal way");
    }

    /// Clicking the file that is ALREADY open in the panel must be a pure
    /// focus/reveal: no reload, so the view state survives. The zoom
    /// assertion is the load-bearing one -- a reload would rebuild
    /// `OpenTileset` and snap the zoom back to its default.
    #[gpui::test]
    async fn test_open_rel_path_on_the_open_tileset_does_not_reload(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        panel.update(cx, |panel, cx| panel.zoom_by(3, cx));
        let generation = panel.read_with(cx, |panel, _| panel.load_generation);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("tiles/world.til", window, cx)
            })
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.load_generation, generation,
                "an already-open click must not start another load"
            );
            assert_eq!(
                ready(panel).zoom,
                DEFAULT_ZOOM + 3,
                "the view state must survive an already-open click"
            );
        });
    }

    /// Two loads issued back to back: whatever order the executor completes
    /// them in (hence the iterations), the SECOND is the one that must land
    /// -- a completed stale first load can never overwrite it.
    #[gpui::test(iterations = 10)]
    async fn test_a_stale_load_is_dropped_by_the_generation_guard(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "world");
        write_tileset_fixture(dir.path(), "other");
        let root = dir.path().to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });

        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
            panel.load_rel_path("tiles/other.til", cx);
            assert!(
                matches!(&panel.state, ViewerState::Loading { rel_path } if rel_path == "tiles/other.til"),
                "the second load owns the Loading state immediately"
            );
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                ready(panel).rel_path,
                "tiles/other.til",
                "the superseded first load must never win, whichever finishes last"
            );
        });
    }

    /// Sanity: the fixture really is on disk in the format worldlib reads
    /// back (guards the test fixture itself, mirroring worldlib's own
    /// `save_tileset` round-trip).
    #[test]
    fn fixture_round_trips_through_worldlib() {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "world");
        let r = open_tileset(dir.path(), "tiles/world.til").unwrap();
        assert_eq!(r.tile_count, FIXTURE_TILES);
        assert_eq!(r.indices[0], 0);
        assert_eq!(r.indices[TILE_PIXELS], 1);
        assert!(!r.missing_pal);
    }

    // ------------------------------------------------ import alert (task 6)

    #[gpui::test]
    async fn test_a_changed_or_missing_import_source_raises_the_alert(cx: &mut TestAppContext) {
        use ggo_worldlib::sprites::tileset_meta::{
            ImportRecord, TilesetMeta, save_tileset_meta, source_mtime,
        };
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("art/hero.png");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"png").unwrap();
        let record = |mtime: u64| TilesetMeta {
            import: Some(ImportRecord {
                source: "art/hero.png".into(),
                mtime,
                ..Default::default()
            }),
            ..Default::default()
        };
        let current = source_mtime(&source).unwrap();

        save_tileset_meta(dir.path(), "tiles/world.til", &record(current)).unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.read_with(cx, |panel, _| assert_eq!(ready(panel).import_alert, None));

        save_tileset_meta(dir.path(), "tiles/world.til", &record(current - 5)).unwrap();
        panel.update(cx, |panel, cx| panel.load_rel_path("tiles/world.til", cx));
        cx.executor().run_until_parked();
        panel.read_with(cx, |panel, _| {
            let alert = ready(panel)
                .import_alert
                .clone()
                .expect("stale mtime alerts");
            assert!(!alert.missing);
            assert_eq!(alert.source, "art/hero.png");
        });

        std::fs::remove_file(&source).unwrap();
        panel.update(cx, |panel, cx| panel.load_rel_path("tiles/world.til", cx));
        cx.executor().run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                ready(panel)
                    .import_alert
                    .as_ref()
                    .is_some_and(|a| a.missing)
            );
        });
    }

    // ------------------------------------------------- pixel tools (task 7)

    fn idx(tile: usize, x: usize, y: usize) -> usize {
        tile * TILE_PIXELS + y * TILE_PX + x
    }

    #[gpui::test]
    async fn test_line_drag_paints_its_endpoints_as_one_undo_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Line, cx);
            let a = pixel_pos(ready(panel), 0, 0, 0);
            let b = pixel_pos(ready(panel), 0, 5, 3);
            panel.on_sheet_mouse_down(a, cx);
            panel.on_sheet_mouse_move(b, cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 0, 0)],
                0,
                "nothing until release"
            );
            assert!(
                !ready(panel).shape_points(false).is_empty(),
                "preview points exist"
            );
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 0, 0)], 1);
            assert_eq!(state.indices[idx(0, 5, 3)], 1);
            assert!(ready(panel).shape_drag.is_none());
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 5, 3)],
                0,
                "one undo clears the line"
            );
        });
    }

    #[gpui::test]
    async fn test_rect_is_an_outline_unless_shift_fills(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Rect, cx);
            let a = pixel_pos(ready(panel), 0, 1, 1);
            let b = pixel_pos(ready(panel), 0, 4, 4);
            panel.on_sheet_mouse_down(a, cx);
            panel.on_sheet_mouse_move(b, cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 1, 1)], 1);
            assert_eq!(state.indices[idx(0, 2, 2)], 0, "outline leaves the middle");
            panel.on_sheet_mouse_down(a, cx);
            panel.on_sheet_mouse_move(b, cx);
            panel.on_sheet_mouse_up(true, cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 2, 2)],
                1,
                "shift fills"
            );
        });
    }

    #[gpui::test]
    async fn test_fill_floods_across_the_tile_border(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            // Tiles 1 and 2 are solid slot 1 and adjacent on the sheet; tile
            // 0 is transparent. Filling tile 1 with slot 2 crosses into 2.
            panel.select_slot(2, cx);
            panel.set_tool(Tool::Fill, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 1, 3, 3), cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 0, 0)], 2);
            assert_eq!(state.indices[idx(2, 7, 7)], 2, "flooded across the border");
            assert_eq!(
                state.indices[idx(0, 0, 0)],
                0,
                "tile 0 was a different colour"
            );
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(2, 7, 7)],
                1,
                "one undo step"
            );
        });
    }

    #[gpui::test]
    async fn test_brush_size_and_mirror_expand_a_pencil_dot(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.step_brush(1, cx);
            assert_eq!(ready(panel).brush, 2);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.mirror_h = true;
            }
            let pos = pixel_pos(ready(panel), 0, 1, 2);
            panel.on_sheet_mouse_down(pos, cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            for (x, y) in [(1, 2), (2, 2), (1, 3), (2, 3)] {
                assert_eq!(state.indices[idx(0, x, y)], 1, "brush 2x2 at ({x},{y})");
            }
            let m = TILE_PX - 1;
            for (x, y) in [(m - 1, 2), (m - 2, 2), (m - 1, 3), (m - 2, 3)] {
                assert_eq!(state.indices[idx(0, x, y)], 1, "mirrored at ({x},{y})");
            }
            assert_eq!(state.indices[idx(0, 3, 2)], 0, "the middle is untouched");
            panel.step_brush(-5, cx);
            assert_eq!(ready(panel).brush, MIN_BRUSH, "clamped");
        });
    }

    #[gpui::test]
    async fn test_dragging_inside_the_marquee_moves_its_pixels(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            let dot = pixel_pos(ready(panel), 0, 1, 1);
            panel.on_sheet_mouse_down(dot, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.set_tool(Tool::Select, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 0, 0), cx);
            panel.on_sheet_mouse_move(pixel_pos(ready(panel), 0, 2, 2), cx);
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(ready(panel).selection, Some(((0, 0), (2, 2))));

            // Drag from inside the marquee by (+3, 0).
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 1, 1), cx);
            panel.on_sheet_mouse_move(pixel_pos(ready(panel), 0, 4, 1), cx);
            assert_eq!(ready(panel).move_offset(), (3, 0));
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 1, 1)], 0, "source cleared");
            assert_eq!(state.indices[idx(0, 4, 1)], 1, "moved");
            assert_eq!(
                ready(panel).selection,
                Some(((3, 0), (5, 2))),
                "marquee followed"
            );
            panel.undo_impl(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 1, 1)], 1, "one undo restores the move");
            assert_eq!(state.indices[idx(0, 4, 1)], 0);
        });
    }

    #[gpui::test]
    async fn test_flip_reverses_the_selection_and_delete_row_shrinks(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 0, 0), cx);
            panel.on_sheet_mouse_up(false, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (3, 0)));
            }
            panel.flip_selection(true, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 0, 0)], 0);
            assert_eq!(
                state.indices[idx(0, 3, 0)],
                1,
                "the dot moved to the far end"
            );
            panel.flip_selection(false, cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 3, 0)],
                1,
                "1-row flip V is identity"
            );

            let count = ready(panel).store.state().tile_count;
            let cols = ready(panel).cols;
            panel.insert_row(false, cx);
            assert_eq!(ready(panel).store.state().tile_count, count + cols);
            panel.delete_row(false, cx);
            assert_eq!(ready(panel).store.state().tile_count, count);
            panel.delete_row(false, cx);
            assert_eq!(
                ready(panel).store.state().tile_count,
                count,
                "the only row stays"
            );
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().tile_count,
                count + cols,
                "undo re-grows"
            );

            panel.insert_column(false, cx);
            assert_eq!(ready(panel).cols, cols + 1);
            panel.delete_column(false, cx);
            assert_eq!(ready(panel).cols, cols, "the view narrows with the column");
        });
    }

    #[gpui::test]
    async fn test_focus_mode_edits_one_tile_and_steps_with_arrows(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.enter_focus(1, cx);
            let open = ready(panel);
            assert_eq!(open.focus, Some(1));
            assert_eq!(open.grid_size, (TILE_PX as u32, TILE_PX as u32));
            assert!(open.zoom >= FOCUS_MIN_ZOOM);
        });
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            // Sheet pixel (2, 3) is tile 1's own (2, 3) in focus.
            let z = ready(panel).zoom as f32;
            panel.select_slot(2, cx);
            panel.on_sheet_mouse_down(point(px(2.5 * z), px(3.5 * z)), cx);
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(ready(panel).store.state().indices[idx(1, 2, 3)], 2);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 2, 3)],
                0,
                "tile 0 untouched"
            );

            panel.scroll_by(1.0, 0.0, cx);
            assert_eq!(ready(panel).focus, Some(2), "right steps to the next tile");
            panel.scroll_by(1.0, 0.0, cx);
            assert_eq!(ready(panel).focus, Some(2), "clamped at the last tile");
            panel.cancel_impl(cx);
            let open = ready(panel);
            assert_eq!(open.focus, None);
            assert_eq!(open.zoom, DEFAULT_ZOOM, "zoom restored");
            assert_eq!(
                open.grid_size.0,
                (FIXTURE_TILES * TILE_PX) as u32,
                "whole sheet again"
            );
        });
    }

    #[gpui::test]
    async fn test_a_new_press_never_continues_a_stale_drag(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Select, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (3, 3)));
                // A drag whose release happened off the sheet.
                open.move_drag = Some(((1, 1), (5, 1)));
                open.painting = false;
            }
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 1, 2, 2), cx);
            assert!(
                ready(panel).move_drag.is_none(),
                "the stale move drag is gone"
            );
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 0, 0)],
                1,
                "nothing moved"
            );
        });
    }
}
