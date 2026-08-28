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
    ScrollWheelEvent, Styled, Task, WeakEntity, Window, actions, div, img, point, px,
};
use project::ProjectPath;
use ui::prelude::*;
use workspace::Workspace;

use ggo_worldlib::sprites::io::save_tileset;
use ggo_worldlib::sprites::palette565::{PAL_SLOTS, Pal};
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
        /// Selects the Pencil tool.
        UsePencil,
        /// Selects the Eraser tool.
        UseEraser,
        /// Selects the colour Picker tool.
        UsePicker,
        /// Selects the marquee Select tool.
        UseSelect,
        /// Selects the Fill tool.
        UseFill,
        /// Selects the Line tool.
        UseLine,
        /// Selects the Rect tool.
        UseRect,
        /// Selects the Ellipse tool.
        UseEllipse,
        /// Clears the selected region to the transparent slot.
        DeleteSelection,
        /// Copies the selected region, then clears it.
        CutSelection,
        /// Appends a copy of the focused (or selected) tile to the sheet.
        DuplicateFocusedTile,
        /// Appends one new blank tile to the sheet.
        AppendBlankTile,
        /// Selects the next palette slot.
        NextSlot,
        /// Selects the previous palette slot.
        PrevSlot,
        /// Returns the sheet to its default zoom.
        ResetZoom,
        /// Steps to the next tile in focus mode.
        NextTile,
        /// Steps to the previous tile in focus mode.
        PrevTile,
        /// Toggles the tile-boundary lines.
        ToggleLines,
        /// Toggles horizontal mirrored painting.
        ToggleMirrorHorizontal,
        /// Toggles vertical mirrored painting.
        ToggleMirrorVertical,
        /// Toggles snapping the selection to whole tiles.
        ToggleSnapTiles,
        /// Toggles whether paste writes transparent pixels.
        TogglePasteOpaque,
    ]
);

/// The zoom focus mode uses when the sheet's own zoom is smaller.
const FOCUS_MIN_ZOOM: usize = 8;

/// Upper bound on transparency-checker quads per frame.
const CHECKER_MAX_CELLS: f32 = 8192.0;

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

/// The single key that selects `tool`, shown in its toolbar tooltip and
/// bound in the default keymaps. `f` is deliberately absent -- it is
/// already `FocusTile` -- so Fill takes `g`, the bucket key.
fn tool_shortcut(tool: Tool) -> &'static str {
    match tool {
        Tool::Pencil => "b",
        Tool::Eraser => "e",
        Tool::Picker => "i",
        Tool::Select => "m",
        Tool::Fill => "g",
        Tool::Line => "l",
        Tool::Rect => "r",
        Tool::Ellipse => "o",
    }
}

/// A tool button's tooltip: its description with its key appended, the
/// same `"... (key)"` shape the brush and flip buttons already use.
fn tool_tip(description: &str, tool: Tool) -> String {
    format!("{description} ({})", tool_shortcut(tool))
}

impl Tool {
    /// Drag-to-shape tools: preview while dragging, commit on release.
    fn is_shape(self) -> bool {
        matches!(self, Tool::Line | Tool::Rect | Tool::Ellipse)
    }
}

/// A copied region, shared by every open tileset.
///
/// The buffer used to live on `OpenTileset`, and `tileset_item` builds a
/// fresh panel entity per tab, so copying in one tileset and pasting into
/// another silently did nothing at all.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TilesetClip {
    w: usize,
    h: usize,
    pixels: Vec<u8>,
    /// The palette these indices were written against. Pixels are palette
    /// INDICES, so pasting them under a different palette reads the same
    /// numbers as different colours -- worth saying rather than silently
    /// recolouring someone's art.
    palette: Pal,
}

#[derive(Default)]
struct SharedClipboard(Option<TilesetClip>);

impl gpui::Global for SharedClipboard {}

/// A region lifted out of the sheet and floating above it.
///
/// Moving a marquee used to rewrite the document on every step: blank the
/// source, stamp the destination, one undo entry each. Nudging ten pixels
/// was ten mutations, anything pushed past the sheet edge was filtered out
/// by `doc_pixel` and gone for good, and each step composited over whatever
/// art it landed on. Lifting once and moving a buffer fixes all three.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Float {
    w: usize,
    h: usize,
    pixels: Vec<u8>,
    /// Top-left in sheet coords. SIGNED: a float may hang off the sheet and
    /// come back intact, which is the whole point.
    at: (i32, i32),
    /// Where it was lifted from, so a cancel can put it back.
    from: (i32, i32),
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
    /// Set when the last paste came from a tileset with a different
    /// palette, so the status line can say the colours will not match.
    clipboard_note: Option<&'static str>,
    /// Whether the tile-boundary lines draw over the sheet.
    show_lines: bool,
    /// Expand the marquee to whole tiles. Off by default: this is a pixel
    /// editor first, and snapping every selection would make sub-tile
    /// selection impossible without a modifier. Per-session, like
    /// `paste_opaque`.
    snap_tiles: bool,
    /// Whether shift was held during the in-flight shape drag, so the
    /// preview draws what a release would actually paint.
    shape_filled: bool,
    /// Whether ctrl was held during the in-flight shape drag: squares,
    /// circles and 45-degree lines.
    ///
    /// Ctrl and not shift, deliberately. Shift already means "filled" on
    /// these exact tools, and it is spoken for twice more on this sheet --
    /// whole-sheet fill, and the shape commit itself.
    shape_constrained: bool,
    /// The in-flight stroke is a right-drag, which paints the transparent
    /// slot. Aseprite's secondary colour; in a 16-slot indexed palette
    /// where slot 0 IS transparency, erase is what a secondary is for.
    secondary_paint: bool,
    /// The sheet pixel under the cursor, for the status readout. `None`
    /// once the pointer leaves the sheet.
    hover: Option<(usize, usize)>,
    /// Space is held: the sheet pans instead of painting.
    space_held: bool,
    /// An in-flight space-pan: (cursor at press, scroll offset at press).
    pan_drag: Option<(Point<Pixels>, Point<Pixels>)>,
    /// Column-count changes pinned to the document revision that caused
    /// them: undo depth AFTER the op -> (cols before, cols after).
    ///
    /// `cols` is a VIEW property, so the store cannot restore it, but
    /// InsertColumn/DeleteColumn change it. Without this, undoing a column
    /// op put the tile strip back and left the sheet wrapped at the wrong
    /// width, silently rearranging every tile.
    cols_history: std::collections::HashMap<usize, (usize, usize)>,
    /// The lifted region, if the marquee has been moved.
    float: Option<Float>,
    /// Paste writes the clipboard's transparent pixels too, punching holes
    /// in the destination. Off = composite over. Per-session ink mode, so
    /// unlike `show_lines` it is not persisted.
    paste_opaque: bool,
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
            clipboard_note: None,
            show_lines: loaded.lines.unwrap_or(true),
            snap_tiles: false,
            shape_filled: false,
            shape_constrained: false,
            secondary_paint: false,
            hover: None,
            space_held: false,
            pan_drag: None,
            cols_history: std::collections::HashMap::new(),
            float: None,
            paste_opaque: false,
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
    /// Expand `points` by the brush, then mirror.
    ///
    /// worldlib's `brush_expand` grows only down-right from each point, so a
    /// 4px brush put the cursor on the footprint's top-left CORNER and every
    /// stroke landed low and right of where it was aimed. Recentre here
    /// rather than in worldlib: that crate is a separate repository and its
    /// own tests pin the down-right expansion.
    fn expand_points(&self, points: &[(i32, i32)]) -> Vec<(i32, i32)> {
        let offset = (self.brush.max(1) as i32 - 1) / 2;
        let centred: Vec<(i32, i32)> = points
            .iter()
            .map(|&(x, y)| (x - offset, y - offset))
            .collect();
        let brushed = pixel_tools::brush_expand(&centred, self.brush);
        pixel_tools::mirror_in_tile(&brushed, TILE_PX, self.mirror_h, self.mirror_v)
    }

    /// The points a shape drag would paint, expanded.
    fn shape_points(&self, filled: bool) -> Vec<(i32, i32)> {
        let Some(((ax, ay), (hx, hy))) = self.shape_drag else {
            return Vec::new();
        };
        let a = (ax as i32, ay as i32);
        let b = if self.shape_constrained {
            constrain_head(a, (hx as i32, hy as i32), self.tool)
        } else {
            (hx as i32, hy as i32)
        };
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
        if self.secondary_paint {
            return 0;
        }
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
    /// Re-read `rel` from disk, DISCARDING any unsaved edits -- the
    /// "Don't Save" answer to the tab's close prompt. Unlike
    /// `open_rel_path` this asks nothing: the user already answered.
    pub(crate) fn reload_from_disk(&mut self, rel: &str, cx: &mut Context<Self>) {
        self.refresh_root(cx);
        self.load_rel_path(rel, cx);
    }

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
        let tile_count = open.store.tile_count();
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
        if tile >= open.store.tile_count() {
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

    /// The tile a tile-level command acts on: the focused one, else the
    /// one under the marquee's top-left. `None` -- rather than
    /// `focus_tile_impl`'s fallback to tile 0 -- so a stray keypress with
    /// nothing targeted does nothing instead of silently acting on tile 0.
    fn target_tile(&self) -> Option<usize> {
        if let ViewerState::Ready(open) = &self.state
            && let Some(tile) = open.focus
        {
            return Some(tile);
        }
        self.selection_rect()
            .and_then(|(x0, y0, ..)| self.doc_pixel(x0, y0))
            .map(|(tile, ..)| tile)
    }

    /// Append a copy of [`Self::target_tile`] to the end of the sheet.
    ///
    /// The range guard is deliberately HERE rather than in worldlib:
    /// `TilesetOp::DuplicateTile` slices `indices[start..start +
    /// TILE_PIXELS]` unchecked and would panic on an out-of-range tile, and
    /// worldlib is a separate repository.
    fn duplicate_tile(&mut self, cx: &mut Context<Self>) {
        // Any other document op puts a float down first.
        self.commit_float(cx);
        let Some(tile) = self.target_tile() else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if tile >= open.store.tile_count() {
            return;
        }
        open.store
            .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::DuplicateTile { tile });
        // In focus mode the sheet IS one tile, so follow the copy -- landing
        // on a magnified view of the tile you just left would look like the
        // duplicate silently failed.
        if open.focus.is_some() {
            open.focus = Some(open.store.tile_count() - 1);
        }
        self.recompose_grid(cx);
    }

    /// Append one blank tile to the end of the sheet.
    fn append_tile(&mut self, cx: &mut Context<Self>) {
        // Any other document op puts a float down first.
        self.commit_float(cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        open.store
            .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::AppendTile);
        self.recompose_grid(cx);
    }

    /// Escape: leave focus if in it, else drop the selection.
    fn cancel_impl(&mut self, cx: &mut Context<Self>) {
        // A float outranks the other two meanings: it is the only one
        // holding unsaved work, and dropping it is free.
        if self.cancel_float(cx) {
            return;
        }
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
        let (Some(focus), count) = (open.focus, open.store.tile_count()) else {
            return;
        };
        let next = (focus as isize + delta).clamp(0, count.max(1) as isize - 1) as usize;
        self.enter_focus(next, cx);
    }

    /// A left press on the sheet. `click_count` is deliberately unused: a
    /// double click is just two clicks.
    ///
    /// It used to enter single-tile focus, which fires by accident
    /// whenever you paint quickly and replaces the whole sheet with one
    /// magnified tile. Focus is still reachable on purpose -- `f`
    /// ([`FocusTile`]) or the toolbar button.
    fn on_sheet_click(
        &mut self,
        position: Point<Pixels>,
        _click_count: usize,
        modifiers: gpui::Modifiers,
        secondary: bool,
        cx: &mut Context<Self>,
    ) {
        // Alt samples the colour under the cursor whatever tool is active,
        // and leaves that tool alone.
        if modifiers.alt {
            self.pick_at(position, true, cx);
            return;
        }
        if matches!(&self.state, ViewerState::Ready(open) if open.space_held) {
            self.begin_pan(position, cx);
            return;
        }
        // A press outside a float puts it down before anything else acts.
        if let Some(rect) = self.float_rect()
            && let Some((sx, sy)) = self.sheet_px_at(position)
            && (sx < rect.0 || sx > rect.2 || sy < rect.1 || sy > rect.3)
        {
            self.commit_float(cx);
        }
        if let ViewerState::Ready(open) = &mut self.state {
            open.secondary_paint = secondary;
        }
        self.on_sheet_mouse_down(position, modifiers.shift, cx);
    }

    /// An arrow-key step: shift the marquee if there is one, else step
    /// tiles in focus mode, else scroll the sheet.
    ///
    /// The marquee wins because a visible selection is what the arrows most
    /// obviously address -- and it is the same move dragging inside the
    /// marquee performs, so the two gestures agree. Escape drops the
    /// selection and hands the arrows back to scrolling.
    /// A press that landed outside the panel entirely drops the marquee.
    ///
    /// Scoped to the panel ROOT, not the sheet: the toolbar's Copy, Paste
    /// and Flip buttons all operate on the selection, so a press inside the
    /// panel must never clear it.
    fn clear_selection_on_click_out(&mut self, cx: &mut Context<Self>) {
        // A press outside the panel ends the gesture, so put any float down
        // first. Clearing the marquee and leaving it live would strand it:
        // the art shows moved, nothing says it is uncommitted, and a later
        // escape would silently take it back.
        self.commit_float(cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open.selection.is_none() && open.move_drag.is_none() {
            return;
        }
        open.selection = None;
        open.move_drag = None;
        cx.notify();
    }

    /// Zoom one step, keeping the sheet pixel `(sx, sy)` under the cursor.
    ///
    /// The sheet is drawn at `image * zoom` inside a scroll container, so a
    /// pixel `s` sits at `origin + offset + s*zoom`. Holding it still across
    /// a zoom change is exactly `offset' = offset + s*(zoom - zoom')`.
    /// Without this the view lurches toward the origin on every step and you
    /// lose whatever you were working on.
    fn zoom_at_sheet_px(&mut self, delta: isize, (sx, sy): (usize, usize), cx: &mut Context<Self>) {
        let Some(before) = self.ready_zoom() else {
            return;
        };
        self.zoom_by(delta, cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let after = open.zoom;
        if after == before {
            return;
        }
        let shift = before as f32 - after as f32;
        let offset = open.scroll.offset();
        // Offsets are negative-or-zero: 0 is the top/left of the sheet.
        open.scroll.set_offset(point(
            (offset.x + px(sx as f32 * shift)).min(px(0.)),
            (offset.y + px(sy as f32 * shift)).min(px(0.)),
        ));
        cx.notify();
    }

    /// Begin a space-pan from `pos`, remembering where the sheet started.
    fn begin_pan(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.pan_drag = Some((pos, open.scroll.offset()));
            cx.notify();
        }
    }

    /// Continue a space-pan: the sheet follows the cursor one-to-one.
    fn pan_to(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some((start, base)) = open.pan_drag else {
            return;
        };
        open.scroll.set_offset(point(
            (base.x + (pos.x - start.x)).min(px(0.)),
            (base.y + (pos.y - start.y)).min(px(0.)),
        ));
        cx.notify();
    }

    fn end_pan(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.pan_drag.take().is_some()
        {
            cx.notify();
        }
    }

    /// Space down/up. Releasing space also ends any pan in flight, so a
    /// release off the sheet cannot strand the drag.
    fn set_space_held(&mut self, held: bool, cx: &mut Context<Self>) {
        let changed = match &mut self.state {
            ViewerState::Ready(open) if open.space_held != held => {
                open.space_held = held;
                true
            }
            _ => false,
        };
        if !held {
            self.end_pan(cx);
        }
        if changed {
            cx.notify();
        }
    }

    /// Ctrl-wheel zooms; a plain wheel is left to the scroll container.
    fn on_sheet_scroll(
        &mut self,
        dy: f32,
        zoom_modifier: bool,
        at: Option<(usize, usize)>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !zoom_modifier || dy == 0.0 {
            return false;
        }
        let delta = if dy > 0.0 { 1 } else { -1 };
        match at {
            Some(sheet_px) => self.zoom_at_sheet_px(delta, sheet_px, cx),
            None => self.zoom_by(delta, cx),
        }
        true
    }

    fn scroll_by(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        if self.selection_rect().is_some() {
            let step = self.nudge_step();
            self.move_selection((dx as i32 * step, dy as i32 * step), cx);
            return;
        }
        // In focus mode the sheet IS one tile, so there is nothing to
        // scroll: left/right step one tile, up/down step a whole row. Up
        // and down used to be dead keys there.
        if let ViewerState::Ready(open) = &self.state
            && open.focus.is_some()
        {
            let cols = open.cols.max(1) as isize;
            let delta = if dx != 0.0 {
                if dx > 0.0 { 1 } else { -1 }
            } else if dy != 0.0 {
                if dy > 0.0 { cols } else { -cols }
            } else {
                return;
            };
            self.step_focus(delta, cx);
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
    /// Flood from `pos`, walled in by the clicked TILE unless `whole_sheet`.
    ///
    /// Slot 0 is transparent everywhere, so across a whole sheet every
    /// unpainted pixel of every tile is one contiguous region -- an
    /// unbounded bucket repaints the entire tileset on the first click.
    /// One tile is the tileset analogue of Aseprite's canvas, so that is
    /// the default; shift restores the sheet-wide reach. A live marquee
    /// walls the flood in as well, matching what a selection means in
    /// every other editor.
    fn fill_at(&mut self, pos: Point<Pixels>, whole_sheet: bool, cx: &mut Context<Self>) {
        let Some((sx, sy)) = self.sheet_px_at(pos) else {
            return;
        };
        let bound_rect = self.selection_rect();
        let (indices, tile_count, cols, focus, color) = match &self.state {
            ViewerState::Ready(open) => (
                open.store.indices(),
                open.store.tile_count(),
                open.cols,
                open.focus,
                open.paint_color(),
            ),
            _ => return,
        };
        // A pad cell backs no tile, so there is nothing to flood.
        let Some((clicked_tile, ..)) = sheet_to_doc(focus, cols, tile_count, sx, sy) else {
            return;
        };
        if bound_rect.is_some_and(|(x0, y0, x1, y1)| sx < x0 || sx > x1 || sy < y0 || sy > y1) {
            return;
        }
        let sample = |x: i32, y: i32| {
            if x < 0 || y < 0 {
                return None;
            }
            let (x, y) = (x as usize, y as usize);
            if let Some((x0, y0, x1, y1)) = bound_rect
                && (x < x0 || x > x1 || y < y0 || y > y1)
            {
                return None;
            }
            let (tile, px_x, px_y) = sheet_to_doc(focus, cols, tile_count, x, y)?;
            if !whole_sheet && tile != clicked_tile {
                return None;
            }
            let index =
                tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS + px_y * TILE_PX + px_x;
            indices.get(index).copied()
        };
        let region = pixel_tools::flood(sample, (sx as i32, sy as i32));
        self.write_points(&region, color, cx);
    }

    /// Rebuild the composed sheet image from the store's current state --
    /// after every op, undo, and redo.
    fn recompose_grid(&mut self, cx: &mut Context<Self>) {
        // A float is shown by composing it INTO the sheet image rather than
        // painting it in the overlay: a whole-sheet float would be hundreds
        // of thousands of quads, and the document must stay untouched until
        // commit. The copy is proportionate -- composing already allocates a
        // full RGBA buffer.
        let floated = self.float_composed_indices();
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        // Borrow the float's view when there is one, else the document's own
        // pixels. The palette is copied out (it is 32 bytes) so the store
        // borrow does not have to live across the focus-arm's mutations.
        let tile_count = open.store.tile_count();
        let palette = *open.store.palette();
        let indices: &[u8] = floated.as_deref().unwrap_or_else(|| open.store.indices());
        let composed = match open.focus {
            Some(tile) => {
                let pixels = indices
                    .get(tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS..)
                    .and_then(|rest| rest.get(..ggo_worldlib::sprites::tileset_doc::TILE_PIXELS));
                match pixels {
                    Some(pixels) => (
                        loader::compose_grid(pixels, 1, 1, &palette),
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
                            loader::compose_grid(indices, tile_count, open.cols, &palette),
                            loader::grid_pixel_size(tile_count, open.cols),
                        )
                    }
                }
            }
            None => (
                loader::compose_grid(indices, tile_count, open.cols, &palette),
                loader::grid_pixel_size(tile_count, open.cols),
            ),
        };
        if let Some(grid) = composed.0 {
            open.grid = grid;
        }
        open.grid_size = composed.1;
        cx.notify();
    }

    fn on_sheet_mouse_down(&mut self, pos: Point<Pixels>, shift: bool, cx: &mut Context<Self>) {
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
                self.pick_at(pos, false, cx);
                return; // a pick is instantaneous, no drag state
            }
            Tool::Fill => {
                self.fill_at(pos, shift, cx);
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

    fn on_sheet_mouse_move(
        &mut self,
        pos: Point<Pixels>,
        modifiers: gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        if matches!(&self.state, ViewerState::Ready(open) if open.pan_drag.is_some()) {
            self.pan_to(pos, cx);
            return;
        }
        let hover = self.sheet_px_at(pos);
        if let ViewerState::Ready(open) = &mut self.state {
            open.shape_filled = modifiers.shift;
            open.shape_constrained = modifiers.control;
            if open.hover != hover {
                open.hover = hover;
                cx.notify();
            }
        }
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
    fn pick_at(&mut self, pos: Point<Pixels>, keep_tool: bool, cx: &mut Context<Self>) {
        let Some((tile, x, y)) = self.pixel_at(pos) else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let idx = tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS + y * TILE_PX + x;
        if let Some(slot) = open.store.indices().get(idx).copied() {
            open.slot = slot as usize;
            if keep_tool {
                cx.notify();
                return;
            }
            // Picking with the Picker TOOL is an intent to draw with the
            // colour, so it hands you the pencil. Alt-picking mid-stroke is
            // not -- it samples and gives you your tool straight back.
            open.tool = Tool::Pencil;
            cx.notify();
        }
    }

    /// Drop the hover readout when the pointer leaves the sheet, so a
    /// stale tile index does not sit in the status line.
    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.hover.take().is_some()
        {
            cx.notify();
        }
    }

    /// The status-line note for a live float.
    ///
    /// Nothing else on screen distinguishes "floating" from "committed",
    /// and escape means two different things depending on which you are in,
    /// so the state has to be said out loud.
    fn float_status(&self) -> Option<&'static str> {
        match &self.state {
            ViewerState::Ready(open) if open.float.is_some() => {
                Some("floating — escape cancels, click away to place")
            }
            _ => None,
        }
    }

    /// The tile index and in-tile pixel under the cursor, for the status
    /// line. The tile index appeared nowhere in the UI except the Focus
    /// button's label, so there was no way to tell tile 12 from tile 13.
    fn hover_status(&self) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let (sx, sy) = open.hover?;
        match self.doc_pixel(sx, sy) {
            Some((tile, x, y)) => Some(format!("tile {tile} · px {x},{y}")),
            // A pad cell backs no tile; say so rather than showing nothing.
            None => Some("pad cell".to_string()),
        }
    }

    /// The selection's normalized inclusive rect `(x0, y0, x1, y1)`.
    fn selection_rect(&self) -> Option<(usize, usize, usize, usize)> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let ((ax, ay), (hx, hy)) = open.selection?;
        let (x0, y0, x1, y1) = (ax.min(hx), ay.min(hy), ax.max(hx), ay.max(hy));
        if !open.snap_tiles {
            return Some((x0, y0, x1, y1));
        }
        // Expand the NORMALIZED rect outward. Snapping the raw anchor down
        // and the raw head up is wrong for an up-left drag: the head would
        // land below the anchor and normalization would then eat both outer
        // halves. `open.selection` itself stays raw, so toggling snap off is
        // lossless.
        let (sheet_w, sheet_h) = (open.grid_size.0 as usize, open.grid_size.1 as usize);
        let up =
            |v: usize, max: usize| ((v / TILE_PX + 1) * TILE_PX - 1).min(max.saturating_sub(1));
        Some((
            x0 / TILE_PX * TILE_PX,
            y0 / TILE_PX * TILE_PX,
            up(x1, sheet_w),
            up(y1, sheet_h),
        ))
    }

    /// One arrow press moves the marquee by a whole tile when snapping is
    /// on, otherwise by one pixel.
    fn nudge_step(&self) -> i32 {
        match &self.state {
            ViewerState::Ready(open) if open.snap_tiles => TILE_PX as i32,
            _ => 1,
        }
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
        let indices = open.store.indices();
        for dy in 0..h {
            for dx in 0..w {
                if let Some((tile, px_x, px_y)) = self.doc_pixel(x0 + dx, y0 + dy) {
                    let idx = tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS
                        + px_y * TILE_PX
                        + px_x;
                    if let Some(&v) = indices.get(idx) {
                        data[dy * w + dx] = v;
                    }
                }
            }
        }
        (w, h, data)
    }

    /// The shared clip, if anything has been copied in any tileset.
    fn clip(cx: &App) -> Option<TilesetClip> {
        cx.try_global::<SharedClipboard>().and_then(|c| c.0.clone())
    }

    /// Put `pixels` on the shared clipboard, tagged with the palette they
    /// were indexed against.
    fn set_clip(w: usize, h: usize, pixels: Vec<u8>, palette: Pal, cx: &mut App) {
        cx.set_global(SharedClipboard(Some(TilesetClip {
            w,
            h,
            pixels,
            palette,
        })));
    }

    /// Copy the selected region into the internal clipboard, sampling the
    /// doc through sheet coordinates (pad cells copy as 0).
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        // A float IS the selection's pixels; reading the sheet under it
        // would copy whatever it is hovering over instead.
        let copied = match &self.state {
            ViewerState::Ready(open) => match &open.float {
                Some(float) => Some((
                    float.w,
                    float.h,
                    float.pixels.clone(),
                    *open.store.palette(),
                )),
                None => None,
            },
            _ => return,
        };
        let (w, h, pixels, palette) = match copied {
            Some(copied) => copied,
            None => {
                let Some(rect) = self.selection_rect() else {
                    return;
                };
                let (w, h, pixels) = self.region_pixels(rect);
                let ViewerState::Ready(open) = &self.state else {
                    return;
                };
                (w, h, pixels, *open.store.palette())
            }
        };
        Self::set_clip(w, h, pixels, palette, cx);
        cx.notify();
    }

    /// Blank the selected region to slot 0 as one undo step.
    ///
    /// Distinct from [`Cancel`], which only drops the marquee. Without this
    /// the only way to clear a region was to switch to the eraser and scrub
    /// it with a brush capped at 4px.
    fn erase_selection(&mut self, cx: &mut Context<Self>) {
        // Deleting a float throws away the carried pixels AND blanks where
        // they came from, in one step.
        if let Some(float) = match &self.state {
            ViewerState::Ready(open) => open.float.clone(),
            _ => None,
        } {
            let cells: Vec<(i32, i32, u8)> = (0..float.h)
                .flat_map(|row| {
                    (0..float.w)
                        .map(move |col| (float.from.0 + col as i32, float.from.1 + row as i32, 0u8))
                })
                .collect();
            if let ViewerState::Ready(open) = &mut self.state {
                open.float = None;
                open.selection = None;
            }
            self.write_cells(&cells, cx);
            return;
        }
        let Some((x0, y0, x1, y1)) = self.selection_rect() else {
            return;
        };
        let cells: Vec<(i32, i32, u8)> = (y0..=y1)
            .flat_map(|y| (x0..=x1).map(move |x| (x as i32, y as i32, 0u8)))
            .collect();
        self.write_cells(&cells, cx);
    }

    /// Copy the selection, then blank it.
    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        self.copy_selection(cx);
        self.erase_selection(cx);
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
    /// Move the marquee, lifting it out of the sheet on the first step.
    ///
    /// Only the float's origin changes after that, so a ten-pixel nudge is
    /// one document mutation rather than ten, and pixels carried off the
    /// sheet edge come back when you move the other way.
    fn move_selection(&mut self, (dx, dy): (i32, i32), cx: &mut Context<Self>) {
        if (dx, dy) == (0, 0) {
            return;
        }
        if !self.lift_selection(cx) {
            return;
        }
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(float) = &mut open.float
        {
            float.at = (float.at.0 + dx, float.at.1 + dy);
        }
        // Keep the marquee showing where the float actually is.
        let rect = self.float_rect();
        if let ViewerState::Ready(open) = &mut self.state {
            open.selection = rect.map(|(x0, y0, x1, y1)| ((x0, y0), (x1, y1)));
        }
        self.recompose_grid(cx);
    }

    /// Lift the marquee into a float. A no-op if one already exists.
    ///
    /// COPIES rather than cuts: the document is not touched until
    /// [`Self::commit_float`], so the whole move -- blanking the source and
    /// stamping the destination -- lands as ONE undo step, and a cancel is
    /// free because there is nothing to roll back.
    fn lift_selection(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(&self.state, ViewerState::Ready(open) if open.float.is_some()) {
            return true;
        }
        let Some((x0, y0, x1, y1)) = self.selection_rect() else {
            return false;
        };
        let (w, h, pixels) = self.region_pixels((x0, y0, x1, y1));
        if let ViewerState::Ready(open) = &mut self.state {
            let at = (x0 as i32, y0 as i32);
            open.float = Some(Float {
                w,
                h,
                pixels,
                at,
                from: at,
            });
        }
        self.recompose_grid(cx);
        true
    }

    /// Stamp the float back into the sheet and drop it. One undo step.
    ///
    /// Composites like [`Self::paste_clipboard`] and honours the same
    /// `paste_opaque` toggle, so moving art over art behaves the way pasting
    /// it would rather than punching the float's transparent pixels through.
    /// Cells outside the sheet are dropped HERE and only here -- while
    /// floating they are still carried.
    fn commit_float(&mut self, cx: &mut Context<Self>) {
        let (float, skip_transparent) = match &self.state {
            ViewerState::Ready(open) => match &open.float {
                Some(float) => (float.clone(), !open.paste_opaque),
                None => return,
            },
            _ => return,
        };
        let mut cells = Vec::with_capacity(float.w * float.h * 2);
        if float.at != float.from {
            // Blank the source first; a later cell for the same pixel wins,
            // so an overlapping move still stamps correctly.
            for row in 0..float.h {
                for col in 0..float.w {
                    cells.push((float.from.0 + col as i32, float.from.1 + row as i32, 0u8));
                }
            }
        }
        for row in 0..float.h {
            for col in 0..float.w {
                let value = float.pixels[row * float.w + col];
                if value == 0 && skip_transparent {
                    continue;
                }
                cells.push((float.at.0 + col as i32, float.at.1 + row as i32, value));
            }
        }
        self.write_cells(&cells, cx);
        if let ViewerState::Ready(open) = &mut self.state {
            open.float = None;
            cx.notify();
        }
    }

    /// Drop the float. Free: the document was never touched.
    fn cancel_float(&mut self, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
            return false;
        };
        if open.float.take().is_none() {
            return false;
        }
        self.recompose_grid(cx);
        true
    }

    /// The document's pixels with the float applied: source blanked,
    /// float stamped. `None` when nothing is floating.
    ///
    /// This is a VIEW of the document, never written back -- committing is
    /// what writes, and it goes through `write_cells` for the undo entry.
    fn float_composed_indices(&self) -> Option<Vec<u8>> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let float = open.float.as_ref()?;
        // The one deliberate copy: this fn RETURNS an owned view.
        let mut indices = open.store.indices().to_vec();
        let mut put = |x: i32, y: i32, value: u8| {
            if x < 0 || y < 0 {
                return;
            }
            if let Some((tile, px_x, px_y)) = self.doc_pixel(x as usize, y as usize) {
                let at =
                    tile * ggo_worldlib::sprites::tileset_doc::TILE_PIXELS + px_y * TILE_PX + px_x;
                if let Some(slot) = indices.get_mut(at) {
                    *slot = value;
                }
            }
        };
        if float.at != float.from {
            for row in 0..float.h {
                for col in 0..float.w {
                    put(float.from.0 + col as i32, float.from.1 + row as i32, 0);
                }
            }
        }
        let skip_transparent = !open.paste_opaque;
        for row in 0..float.h {
            for col in 0..float.w {
                let value = float.pixels[row * float.w + col];
                if value == 0 && skip_transparent {
                    continue;
                }
                put(float.at.0 + col as i32, float.at.1 + row as i32, value);
            }
        }
        Some(indices)
    }

    /// The float's rect in sheet coords, clipped to the sheet.
    fn float_rect(&self) -> Option<(usize, usize, usize, usize)> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let float = open.float.as_ref()?;
        let (sheet_w, sheet_h) = (open.grid_size.0 as i32, open.grid_size.1 as i32);
        let (x0, y0) = float.at;
        let (x1, y1) = (x0 + float.w as i32 - 1, y0 + float.h as i32 - 1);
        if x1 < 0 || y1 < 0 || x0 >= sheet_w || y0 >= sheet_h {
            return None;
        }
        Some((
            x0.max(0) as usize,
            y0.max(0) as usize,
            x1.min(sheet_w - 1) as usize,
            y1.min(sheet_h - 1) as usize,
        ))
    }

    /// Flip the marquee's pixels in place, one undo step.
    fn flip_selection(&mut self, horizontal: bool, cx: &mut Context<Self>) {
        // Flipping a float rearranges the carried buffer, not the document:
        // it stays one pending change until commit.
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(float) = &mut open.float
        {
            let (w, h) = (float.w, float.h);
            let source = float.pixels.clone();
            for row in 0..h {
                for col in 0..w {
                    let (sc, sr) = if horizontal {
                        (w - 1 - col, row)
                    } else {
                        (col, h - 1 - row)
                    };
                    float.pixels[row * w + col] = source[sr * w + sc];
                }
            }
            self.recompose_grid(cx);
            return;
        }
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
        // Any other document op puts a float down first.
        self.commit_float(cx);
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
        // Any other document op puts a float down first.
        self.commit_float(cx);
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
        self.record_cols_change(cols, cols - 1);
        self.recompose_grid(cx);
        self.write_view_meta();
    }

    /// Paste the clipboard at the selection's top-left as ONE stroke (one
    /// undo step). Pixels landing on pad cells are skipped; the selection
    /// moves to the pasted rect.
    /// Where a paste with no marquee lands: the sheet origin in focus mode
    /// (the sheet IS one tile), else the top-left visible sheet pixel,
    /// floored to a tile boundary so a paste never lands half a tile off.
    fn paste_origin(&self) -> Option<(usize, usize)> {
        if let Some((x0, y0, ..)) = self.selection_rect() {
            return Some((x0, y0));
        }
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        if open.focus.is_some() {
            return Some((0, 0));
        }
        let zoom = open.zoom.max(1) as f32;
        let offset = open.scroll.offset();
        let floor = |v: f32| ((v.max(0.) as usize) / TILE_PX) * TILE_PX;
        Some((
            floor(f32::from(-offset.x) / zoom),
            floor(f32::from(-offset.y) / zoom),
        ))
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        // Any other document op puts a float down first.
        self.commit_float(cx);
        let Some((x0, y0)) = self.paste_origin() else {
            return;
        };
        let Some(clip) = Self::clip(cx) else {
            return;
        };
        let (w, h, data) = (clip.w, clip.h, clip.pixels);
        let skip_transparent = match &self.state {
            ViewerState::Ready(open) => !open.paste_opaque,
            _ => return,
        };
        // Pixels are palette INDICES. Under a different palette the same
        // numbers are different colours, so say so rather than silently
        // recolouring what was pasted.
        if let ViewerState::Ready(open) = &mut self.state {
            open.clipboard_note = (*open.store.palette() != clip.palette)
                .then_some("pasted from a tileset with a different palette");
        }
        let mut writes = Vec::new();
        for dy in 0..h {
            for dx in 0..w {
                let value = data[dy * w + dx];
                // Composite: a transparent source pixel leaves the
                // destination alone, so pasting art copied on transparent
                // ground does not punch that ground through.
                if value == 0 && skip_transparent {
                    continue;
                }
                if let Some((tile, px_x, px_y)) = self.doc_pixel(x0 + dx, y0 + dy) {
                    writes.push((tile, px_x, px_y, value));
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
        // Any other document op puts a float down first.
        self.commit_float(cx);
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
        // Any other document op puts a float down first.
        self.commit_float(cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let cols = open.cols;
        open.store
            .apply(ggo_worldlib::sprites::tileset_doc::TilesetOp::InsertColumn { cols, at_left });
        if let ViewerState::Ready(open) = &mut self.state {
            open.cols = cols + 1;
        }
        self.record_cols_change(cols, cols + 1);
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
        if matches!(&self.state, ViewerState::Ready(open) if open.pan_drag.is_some()) {
            self.end_pan(cx);
            return;
        }
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
        // Cleared only now: a shape tool reads paint_color at commit time,
        // above, so clearing any earlier would paint the primary slot.
        if let ViewerState::Ready(open) = &mut self.state {
            open.secondary_paint = false;
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
        if matches!(&self.state, ViewerState::Ready(open) if open.tool != tool) {
            // Leaving the Select tool with something floating would strand
            // it invisibly; put it down first.
            self.commit_float(cx);
        }
        if let ViewerState::Ready(open) = &mut self.state
            && open.tool != tool
        {
            open.tool = tool;
            cx.notify();
        }
    }

    /// Move the painting slot by `delta`, wrapping the 16-slot palette.
    /// Slot 0 is included on purpose -- it is the transparent slot, and
    /// painting with it is how you erase without changing tool.
    fn step_slot(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let next = (open.slot as isize + delta).rem_euclid(PAL_SLOTS as isize) as usize;
        self.set_slot(next, true, cx);
    }

    /// Back to the zoom a freshly opened tileset uses.
    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(DEFAULT_ZOOM, cx);
    }

    fn select_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.set_slot(slot, false, cx);
    }

    /// `keep_tool` leaves the active tool alone. CLICKING a swatch is an
    /// intent to draw with it, so it hands you the pencil; CYCLING the
    /// palette from the keyboard is not -- being yanked out of the Rect
    /// tool because you stepped a colour is just a lost tool.
    fn set_slot(&mut self, slot: usize, keep_tool: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if slot < PAL_SLOTS && open.slot != slot {
            open.slot = slot;
            if !keep_tool {
                open.tool = Tool::Pencil;
            }
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
        if slot == 0 || slot >= PAL_SLOTS || open.store.palette()[slot] == rgb565 {
            return;
        }
        open.store.apply_palette_coalesced(slot, rgb565);
        self.recompose_grid(cx);
    }

    /// Record that the op that just landed took `cols` from `before` to
    /// `after`, so undo and redo can put the view back.
    fn record_cols_change(&mut self, before: usize, after: usize) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let depth = open.store.undo_depth();
        // A new op clears the redo stack, so anything pinned above this
        // revision is unreachable and would only mislead a later undo.
        open.cols_history.retain(|&d, _| d < depth);
        open.cols_history.insert(depth, (before, after));
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        let restored = {
            let ViewerState::Ready(open) = &mut self.state else {
                return;
            };
            // The entry is keyed by the depth the op PRODUCED, which is the
            // depth we are standing on before undoing it.
            let depth = open.store.undo_depth();
            if !open.store.undo() {
                return;
            }
            match open.cols_history.get(&depth) {
                Some(&(before, _)) => {
                    open.cols = before;
                    true
                }
                None => false,
            }
        };
        self.recompose_grid(cx);
        if restored {
            self.write_view_meta();
        }
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        let restored = {
            let ViewerState::Ready(open) = &mut self.state else {
                return;
            };
            if !open.store.redo() {
                return;
            }
            let depth = open.store.undo_depth();
            match open.cols_history.get(&depth) {
                Some(&(_, after)) => {
                    open.cols = after;
                    true
                }
                None => false,
            }
        };
        self.recompose_grid(cx);
        if restored {
            self.write_view_meta();
        }
    }

    /// Write the `.til` + `.pal` pair back through worldlib's atomic
    /// save. Synchronous by choice, same reasoning as the sprite panel's
    /// save. A failure keeps the document dirty and surfaces on the panel
    /// (and as the item's save Err).
    pub(crate) fn save_impl(&mut self, cx: &mut Context<Self>) {
        // The .til must contain what is on screen.
        self.commit_float(cx);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(root) = self.project_root.clone() else {
            return;
        };
        match save_tileset(
            &root,
            &open.rel_path,
            open.store.indices(),
            open.store.tile_count(),
            open.store.palette(),
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
        let (cols, tile_count) = (open.cols, open.store.state().tile_count);
        let show_lines = open.show_lines;
        let accent = cx.theme().colors().border_focused;
        let floating = matches!(&self.state, ViewerState::Ready(open) if open.float.is_some());
        let float_accent = cx.theme().colors().text_accent;
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
        let preview = open.shape_points(open.shape_filled);
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
                    paint_tile_borders(bounds, tile_count, cols, window);
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
                    if floating {
                        // Dashed, in a different colour, with a halo one
                        // pixel out: a lifted region has to look unlike a
                        // committed marquee at a glance, because escape
                        // means something different in each.
                        window.paint_quad(gpui::outline(
                            rect,
                            float_accent,
                            gpui::BorderStyle::Dashed,
                        ));
                        let halo = Bounds::new(
                            gpui::point(rect.origin.x - px(1.), rect.origin.y - px(1.)),
                            gpui::size(rect.size.width + px(2.), rect.size.height + px(2.)),
                        );
                        window.paint_quad(gpui::outline(
                            halo,
                            float_accent,
                            gpui::BorderStyle::Dashed,
                        ));
                    } else {
                        window.paint_quad(gpui::outline(rect, accent, gpui::BorderStyle::Solid));
                    }
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
                                        .id("ggo-tileset-canvas")
                                        .relative()
                                        .w(px(w))
                                        .h(px(h))
                                        .child(
                                            gpui::canvas(
                                                |_, _, _| (),
                                                |bounds, (), window, _cx| {
                                                    paint_checkerboard(bounds, window);
                                                },
                                            )
                                            .absolute()
                                            .top_0()
                                            .left_0()
                                            .size_full(),
                                        )
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
                                                    this.on_sheet_click(
                                                        event.position,
                                                        event.click_count,
                                                        event.modifiers,
                                                        false,
                                                        cx,
                                                    );
                                                },
                                            ),
                                        )
                                        .on_scroll_wheel(cx.listener(
                                            |this, event: &ScrollWheelEvent, _, cx| {
                                                let dy =
                                                    f32::from(event.delta.pixel_delta(px(20.)).y);
                                                let at = this.sheet_px_at(event.position);
                                                if this.on_sheet_scroll(
                                                    dy,
                                                    event.modifiers.control,
                                                    at,
                                                    cx,
                                                ) {
                                                    cx.stop_propagation();
                                                }
                                            },
                                        ))
                                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                            if !*hovered {
                                                this.clear_hover(cx);
                                            }
                                        }))
                                        .on_mouse_move(cx.listener(
                                            |this, event: &MouseMoveEvent, _, cx| {
                                                this.on_sheet_mouse_move(
                                                    event.position,
                                                    event.modifiers,
                                                    cx,
                                                );
                                            },
                                        ))
                                        .on_mouse_down(
                                            MouseButton::Right,
                                            cx.listener(
                                                |this, event: &MouseDownEvent, window, cx| {
                                                    window.focus(&this.focus_handle, cx);
                                                    this.on_sheet_click(
                                                        event.position,
                                                        event.click_count,
                                                        event.modifiers,
                                                        true,
                                                        cx,
                                                    );
                                                },
                                            ),
                                        )
                                        .on_mouse_up(
                                            MouseButton::Right,
                                            cx.listener(|this, event: &MouseUpEvent, _, cx| {
                                                this.on_sheet_mouse_up(event.modifiers.shift, cx);
                                            }),
                                        )
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
            (true, true) => (
                "Add row above",
                "Delete top row — removes its art (ctrl-z undoes)",
            ),
            (true, false) => (
                "Add row below",
                "Delete bottom row — removes its art (ctrl-z undoes)",
            ),
            (false, true) => (
                "Add column left",
                "Delete left column — removes its art (ctrl-z undoes)",
            ),
            (false, false) => (
                "Add column right",
                "Delete right column — removes its art (ctrl-z undoes)",
            ),
        };
        // The destructive half is coloured, not just glyph-different: it
        // sits inside an 18px strip next to the additive one, and a misclick
        // takes a whole row or column of art with it.
        let half = |glyph: &'static str, tip: &'static str, index: usize, destructive: bool| {
            div()
                .id((id, index))
                .flex_1()
                .flex()
                .justify_center()
                .items_center()
                .cursor_pointer()
                .tooltip(ui::Tooltip::text(tip))
                .child(
                    Label::new(glyph)
                        .size(LabelSize::Small)
                        .color(if destructive {
                            Color::Error
                        } else {
                            Color::Muted
                        }),
                )
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
                half("+", add_tip, 0, false).on_click(cx.listener(move |this, _, _, cx| {
                    if horizontal {
                        this.insert_row(at_start, cx);
                    } else {
                        this.insert_column(at_start, cx);
                    }
                })),
            )
            .child(
                half("−", remove_tip, 1, true).on_click(cx.listener(move |this, _, _, cx| {
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
        let mut summary = format!(
            "{} tiles · {}x{} px · {} cols",
            open.store.tile_count(),
            w,
            h,
            open.cols
        );
        if let Some(hover) = self.hover_status() {
            summary.push_str(" · ");
            summary.push_str(&hover);
        }
        if let Some(note) = self.float_status() {
            summary.push_str(" · ");
            summary.push_str(note);
        }
        if let Some(note) = open.clipboard_note {
            summary.push_str(" · ");
            summary.push_str(note);
        }
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
                    .tooltip(ui::Tooltip::text(tool_tip("Pencil", Tool::Pencil)))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Pencil, cx))),
            )
            .child(
                IconButton::new("ggo-tileset-eraser", IconName::Eraser)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.tool == Tool::Eraser)
                    .tooltip(ui::Tooltip::text(tool_tip(
                        "Eraser, paints transparent",
                        Tool::Eraser,
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Eraser, cx))),
            )
            .child(
                IconButton::new("ggo-tileset-picker", IconName::Crosshair)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.tool == Tool::Picker)
                    .tooltip(ui::Tooltip::text(tool_tip(
                        "Pick color from the sheet",
                        Tool::Picker,
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Picker, cx))),
            )
            .child(
                IconButton::new("ggo-tileset-select", IconName::SquareDot)
                    .icon_size(IconSize::Small)
                    .toggle_state(open.tool == Tool::Select)
                    .tooltip(ui::Tooltip::text(tool_tip(
                        "Select region; ctrl-c copy, ctrl-v paste, arrows nudge",
                        Tool::Select,
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tool(Tool::Select, cx))),
            )
            .children(
                [
                    (
                        Tool::Fill,
                        "ggo-tileset-fill",
                        IconName::Sparkle,
                        "Fill this tile (shift: whole sheet)",
                    ),
                    (
                        Tool::Line,
                        "ggo-tileset-line",
                        IconName::Dash,
                        "Line (ctrl = 45 degrees)",
                    ),
                    (
                        Tool::Rect,
                        "ggo-tileset-rect",
                        IconName::Maximize,
                        "Rectangle (shift = filled, ctrl = square)",
                    ),
                    (
                        Tool::Ellipse,
                        "ggo-tileset-ellipse",
                        IconName::Circle,
                        "Ellipse (shift = filled, ctrl = circle)",
                    ),
                ]
                .map(|(tool, id, icon, tip)| {
                    IconButton::new(id, icon)
                        .icon_size(IconSize::Small)
                        .toggle_state(open.tool == tool)
                        .tooltip(ui::Tooltip::text(tool_tip(tip, tool)))
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
                    .tooltip(ui::Tooltip::text("Magnify one tile (f)"))
                    .on_click(cx.listener(|this, _, _, cx| this.focus_tile_impl(cx))),
            })
            .child(
                Button::new("ggo-tileset-duplicate", "Duplicate")
                    .disabled(self.target_tile().is_none())
                    .tooltip(ui::Tooltip::text(
                        "Append a copy of the focused or selected tile (ctrl-d)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.duplicate_tile(cx))),
            )
            .child(
                IconButton::new("ggo-tileset-append", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Append one blank tile"))
                    .on_click(cx.listener(|this, _, _, cx| this.append_tile(cx))),
            )
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
                IconButton::new("ggo-tileset-snap", IconName::Maximize)
                    .icon_size(IconSize::XSmall)
                    .toggle_state(open.snap_tiles)
                    .tooltip(ui::Tooltip::text(
                        "Snap the selection to whole tiles (arrows then nudge by a tile)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &mut this.state {
                            open.snap_tiles = !open.snap_tiles;
                            cx.notify();
                        }
                    })),
            )
            .child(
                IconButton::new("ggo-tileset-paste-opaque", IconName::Copy)
                    .icon_size(IconSize::XSmall)
                    .toggle_state(open.paste_opaque)
                    .tooltip(ui::Tooltip::text(
                        "Paste writes transparent pixels too (off: composite over)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let ViewerState::Ready(open) = &mut this.state {
                            open.paste_opaque = !open.paste_opaque;
                            cx.notify();
                        }
                    })),
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
            .on_action(cx.listener(|this, _: &CutSelection, _, cx| this.cut_selection(cx)))
            .on_action(cx.listener(|this, _: &DeleteSelection, _, cx| this.erase_selection(cx)))
            .on_action(cx.listener(|this, _: &DuplicateFocusedTile, _, cx| this.duplicate_tile(cx)))
            .on_action(cx.listener(|this, _: &AppendBlankTile, _, cx| this.append_tile(cx)))
            .on_action(cx.listener(|this, _: &NextSlot, _, cx| this.step_slot(1, cx)))
            .on_action(cx.listener(|this, _: &PrevSlot, _, cx| this.step_slot(-1, cx)))
            .on_action(cx.listener(|this, _: &ResetZoom, _, cx| this.reset_zoom(cx)))
            .on_action(cx.listener(|this, _: &NextTile, _, cx| this.step_focus(1, cx)))
            .on_action(cx.listener(|this, _: &PrevTile, _, cx| this.step_focus(-1, cx)))
            .on_action(cx.listener(|this, _: &ToggleLines, _, cx| this.toggle_lines(cx)))
            .on_action(cx.listener(|this, _: &ToggleMirrorHorizontal, _, cx| {
                if let ViewerState::Ready(open) = &mut this.state {
                    open.mirror_h = !open.mirror_h;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleMirrorVertical, _, cx| {
                if let ViewerState::Ready(open) = &mut this.state {
                    open.mirror_v = !open.mirror_v;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSnapTiles, _, cx| {
                if let ViewerState::Ready(open) = &mut this.state {
                    open.snap_tiles = !open.snap_tiles;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &TogglePasteOpaque, _, cx| {
                if let ViewerState::Ready(open) = &mut this.state {
                    open.paste_opaque = !open.paste_opaque;
                    cx.notify();
                }
            }))
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
            .on_action(cx.listener(|this, _: &UsePencil, _, cx| this.set_tool(Tool::Pencil, cx)))
            .on_action(cx.listener(|this, _: &UseEraser, _, cx| this.set_tool(Tool::Eraser, cx)))
            .on_action(cx.listener(|this, _: &UsePicker, _, cx| this.set_tool(Tool::Picker, cx)))
            .on_action(cx.listener(|this, _: &UseSelect, _, cx| this.set_tool(Tool::Select, cx)))
            .on_action(cx.listener(|this, _: &UseFill, _, cx| this.set_tool(Tool::Fill, cx)))
            .on_action(cx.listener(|this, _: &UseLine, _, cx| this.set_tool(Tool::Line, cx)))
            .on_action(cx.listener(|this, _: &UseRect, _, cx| this.set_tool(Tool::Rect, cx)))
            .on_action(cx.listener(|this, _: &UseEllipse, _, cx| this.set_tool(Tool::Ellipse, cx)))
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.clear_selection_on_click_out(cx);
                // A press elsewhere means we will never see the space-up.
                this.set_space_held(false, cx);
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "space" {
                    this.set_space_held(true, cx);
                }
            }))
            .on_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, _, cx| {
                if event.keystroke.key == "space" {
                    this.set_space_held(false, cx);
                }
            }))
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
/// The blocks of the sheet grid that back a REAL tile, as
/// `(row_start, rows, cols)` in tile units: the full rows, then the
/// partial last row on its own.
///
/// `compose_tile_grid` pads the sheet image up to a whole last row, so the
/// drawn rectangle is always `cols x div_ceil(tile_count, cols)`. Stroking
/// that whole rectangle draws a tile box over cells no tile backs -- 18
/// tiles at 7 cols shows 21. The pad pixels themselves are index 0, which
/// `slot_rgba` maps to alpha 0, so the borders are the entire illusion.
fn grid_regions(tile_count: usize, cols: usize) -> Vec<(usize, usize, usize)> {
    if tile_count == 0 || cols == 0 {
        return Vec::new();
    }
    let full_rows = tile_count / cols;
    let remainder = tile_count % cols;
    let mut regions = Vec::new();
    if full_rows > 0 {
        regions.push((0, full_rows, cols));
    }
    if remainder > 0 {
        regions.push((full_rows, 1, remainder));
    }
    regions
}

/// The checkerboard cell size in SCREEN px for a sheet drawn `w` x `h`.
///
/// 8px reads as a backdrop rather than as art, but a big sheet at a high
/// zoom would need tens of thousands of quads at that size, so the cell
/// doubles until the count is under [`CHECKER_MAX_CELLS`]. Bounded work per
/// frame, and a coarser check on a huge sheet is not worth a stutter.
/// Pull `head` onto the nearest constrained position from `anchor`: a
/// square for Rect and Ellipse, one of eight directions for Line.
///
/// A zero-length axis resolves positive so a drag that has not moved yet
/// still produces a well-formed shape rather than collapsing.
fn constrain_head(anchor: (i32, i32), head: (i32, i32), tool: Tool) -> (i32, i32) {
    let (ax, ay) = anchor;
    let (dx, dy) = (head.0 - ax, head.1 - ay);
    let sign = |v: i32| if v < 0 { -1 } else { 1 };
    match tool {
        Tool::Rect | Tool::Ellipse => {
            let side = dx.abs().max(dy.abs());
            (ax + side * sign(dx), ay + side * sign(dy))
        }
        Tool::Line => {
            let (adx, ady) = (dx.abs(), dy.abs());
            if adx > ady * 2 {
                (head.0, ay)
            } else if ady > adx * 2 {
                (ax, head.1)
            } else {
                let run = adx.max(ady);
                (ax + run * sign(dx), ay + run * sign(dy))
            }
        }
        _ => head,
    }
}

fn checker_cell_px(w: f32, h: f32) -> f32 {
    let mut cell = 8.0_f32;
    while cell < 4096.0 && (w / cell).ceil() * (h / cell).ceil() > CHECKER_MAX_CELLS {
        cell *= 2.0;
    }
    cell
}

/// Paint the transparency checkerboard across `bounds`.
///
/// Nothing rendered transparency AS transparency: slot 0 composes to alpha
/// 0, so on a dark theme it was indistinguishable from a near-black palette
/// entry and you could not tell a hole from a colour.
fn paint_checkerboard(bounds: Bounds<Pixels>, window: &mut Window) {
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let cell = checker_cell_px(w, h);
    let light = gpui::rgb(0x6b6b6b);
    let dark = gpui::rgb(0x4a4a4a);
    let mut row = 0usize;
    let mut y = 0.0_f32;
    while y < h {
        let mut col = 0usize;
        let mut x = 0.0_f32;
        while x < w {
            let color = if (row + col).is_multiple_of(2) {
                light
            } else {
                dark
            };
            window.paint_quad(gpui::fill(
                Bounds::new(
                    gpui::point(bounds.origin.x + px(x), bounds.origin.y + px(y)),
                    gpui::size(px(cell.min(w - x)), px(cell.min(h - y))),
                ),
                color,
            ));
            x += cell;
            col += 1;
        }
        y += cell;
        row += 1;
    }
}

fn paint_tile_borders(bounds: Bounds<Pixels>, tile_count: usize, cols: usize, window: &mut Window) {
    // `bounds` covers the PADDED sheet, so a cell is sized against the
    // padded row count; only the blocks that back a tile are stroked.
    let total_rows = if cols == 0 {
        0
    } else {
        tile_count.div_ceil(cols)
    };
    if total_rows == 0 {
        return;
    }
    let cell_w = bounds.size.width / cols as f32;
    let cell_h = bounds.size.height / total_rows as f32;
    for (row_start, rows, region_cols) in grid_regions(tile_count, cols) {
        paint_grid(
            Bounds::new(
                gpui::point(bounds.origin.x, bounds.origin.y + cell_h * row_start as f32),
                gpui::size(cell_w * region_cols as f32, cell_h * rows as f32),
            ),
            region_cols,
            rows,
            window,
        );
    }
}

/// Stroke a complete `cols x rows` lattice over `bounds`.
fn paint_grid(bounds: Bounds<Pixels>, cols: usize, rows: usize, window: &mut Window) {
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
    use super::grid_regions;

    /// Only cells that back a real tile get a border. The reported case is
    /// 18 tiles at 7 cols: the sheet image is padded to 3 full rows (21
    /// cells), so stroking the whole rectangle drew 3 tiles that do not
    /// exist.
    #[test]
    fn grid_regions_cover_exactly_the_real_tiles() {
        assert_eq!(grid_regions(18, 7), vec![(0, 2, 7), (2, 1, 4)]);
        assert_eq!(
            grid_regions(21, 7),
            vec![(0, 3, 7)],
            "an exact fit is one block, no partial row"
        );
        assert_eq!(grid_regions(0, 7), Vec::new());
        assert_eq!(
            grid_regions(5, 7),
            vec![(0, 1, 5)],
            "a sheet shorter than one row is just the partial row"
        );

        // The property that was violated: the blocks account for every
        // tile and never more.
        for tile_count in 0..40usize {
            for cols in 1..12usize {
                let cells: usize = grid_regions(tile_count, cols)
                    .into_iter()
                    .map(|(_, rows, c)| rows * c)
                    .sum();
                assert_eq!(cells, tile_count, "{tile_count} tiles at {cols} cols");
            }
        }
    }

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
            panel.on_sheet_mouse_down(pos, false, cx);
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
            panel.on_sheet_mouse_down(down, false, cx);
            panel.on_sheet_mouse_move(drag, gpui::Modifiers::default(), cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.on_sheet_mouse_move(after, gpui::Modifiers::default(), cx);

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
            panel.on_sheet_mouse_down(down, false, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.on_sheet_mouse_down(drag, false, cx);
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
            panel.on_sheet_mouse_down(pos, false, cx);
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
            panel.on_sheet_mouse_down(point(px(w + 10.), px(h + 10.)), false, cx);
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
            panel.on_sheet_mouse_down(pos, false, cx);
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
            panel.on_sheet_mouse_down(pos, false, cx);
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
            panel.on_sheet_mouse_down(pos, false, cx);
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
            panel.on_sheet_mouse_down(from, false, cx);
            panel.on_sheet_mouse_move(to, gpui::Modifiers::default(), cx);
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(
                panel.selection_rect(),
                Some((TILE_PX, 0, TILE_PX + 1, 1)),
                "the marquee is a sheet-space rect over tile 1"
            );

            panel.copy_selection(cx);
            let clip = TilesetPanel::clip(cx).expect("copy filled the clipboard");
            assert_eq!(
                (clip.w, clip.h, clip.pixels),
                (2, 2, vec![1, 1, 1, 1]),
                "copy sampled the region"
            );

            // Move the selection onto tile 0 (all transparent) and paste.
            let target_from = pixel_pos(ready(panel), 0, 2, 2);
            panel.on_sheet_mouse_down(target_from, false, cx);
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

            // Paste again, then undo back to a clean document.
            panel.paste_clipboard(cx);
            panel.undo_impl(cx);
            assert!(!ready(panel).store.state().dirty);

            // No marquee is NOT a no-op any more -- the paste lands at the
            // top-left visible tile instead of being silently dropped.
            TilesetPanel::set_clip(1, 1, vec![5], [0u16; PAL_SLOTS], cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = None;
            }
            panel.paste_clipboard(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 0, 0)],
                5,
                "it landed at the sheet origin"
            );
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
            panel.on_sheet_mouse_down(a, false, cx);
            panel.on_sheet_mouse_move(b, gpui::Modifiers::default(), cx);
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
            panel.on_sheet_mouse_down(a, false, cx);
            panel.on_sheet_mouse_move(b, gpui::Modifiers::default(), cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 1, 1)], 1);
            assert_eq!(state.indices[idx(0, 2, 2)], 0, "outline leaves the middle");
            panel.on_sheet_mouse_down(a, false, cx);
            panel.on_sheet_mouse_move(b, gpui::Modifiers::default(), cx);
            panel.on_sheet_mouse_up(true, cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 2, 2)],
                1,
                "shift fills"
            );
        });
    }

    #[gpui::test]
    async fn test_fill_stops_at_the_tile_border(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            // Tiles 1 and 2 are solid slot 1 and adjacent on the sheet.
            panel.select_slot(2, cx);
            panel.set_tool(Tool::Fill, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 1, 3, 3), false, cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 0, 0)], 2, "the clicked tile filled");
            assert_eq!(
                state.indices[idx(2, 7, 7)],
                1,
                "the neighbour is NOT touched"
            );
            panel.undo_impl(cx);
            assert_eq!(ready(panel).store.state().indices[idx(1, 0, 0)], 1);
        });
    }

    #[gpui::test]
    async fn test_shift_fill_still_floods_the_whole_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(2, cx);
            panel.set_tool(Tool::Fill, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 1, 3, 3), true, cx);
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(2, 7, 7)],
                2,
                "shift crosses the border"
            );
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(2, 7, 7)],
                1,
                "still one undo step"
            );
        });
    }

    #[gpui::test]
    async fn test_fill_is_bounded_by_the_marquee(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(2, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 7, TILE_PX - 1)));
            }
            panel.set_tool(Tool::Fill, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 1, 3, 3), false, cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 3, 3)], 2, "inside the marquee");
            assert_eq!(state.indices[idx(1, 15, 3)], 1, "the marquee walls it in");
        });
    }

    #[gpui::test]
    async fn test_fill_outside_the_marquee_paints_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(2, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 7, TILE_PX - 1)));
            }
            panel.set_tool(Tool::Fill, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 3, 3), false, cx);
            panel.on_sheet_mouse_up(false, cx);
            assert!(
                !ready(panel).store.state().dirty,
                "a click outside the marquee is a no-op"
            );
        });
    }

    /// A press on a pad cell -- `tile_count` not a multiple of `cols` -- has
    /// no tile to bound the flood to, so it must do nothing at all.
    #[gpui::test]
    async fn test_fill_on_a_pad_cell_is_a_no_op(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| panel.set_cols(2, cx));
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(2, cx);
            panel.set_tool(Tool::Fill, cx);
            // 3 tiles at 2 cols: the second row's right cell is padding.
            let pos = {
                let z = ready(panel).zoom as f32;
                point(
                    px((TILE_PX as f32 + 4.0) * z),
                    px((TILE_PX as f32 + 4.0) * z),
                )
            };
            panel.on_sheet_mouse_down(pos, false, cx);
            panel.on_sheet_mouse_up(false, cx);
            assert!(
                !ready(panel).store.state().dirty,
                "the pad cell backs no tile"
            );
        });
    }

    #[gpui::test]
    async fn test_fill_in_focus_mode_fills_the_focused_tile_only(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| panel.enter_focus(1, cx));
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(2, cx);
            panel.set_tool(Tool::Fill, cx);
            let z = ready(panel).zoom as f32;
            panel.on_sheet_mouse_down(point(px(3.5 * z), px(3.5 * z)), false, cx);
            panel.on_sheet_mouse_up(false, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 0, 0)], 2, "the focused tile filled");
            assert_eq!(state.indices[idx(2, 7, 7)], 1, "tile 2 untouched");
            assert_eq!(state.indices[idx(0, 0, 0)], 0, "tile 0 untouched");
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
            panel.on_sheet_mouse_down(pos, false, cx);
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
            panel.on_sheet_mouse_down(dot, false, cx);
            panel.on_sheet_mouse_up(false, cx);
            panel.set_tool(Tool::Select, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 0, 0), false, cx);
            panel.on_sheet_mouse_move(
                pixel_pos(ready(panel), 0, 2, 2),
                gpui::Modifiers::default(),
                cx,
            );
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(ready(panel).selection, Some(((0, 0), (2, 2))));

            // Drag from inside the marquee by (+3, 0).
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 1, 1), false, cx);
            panel.on_sheet_mouse_move(
                pixel_pos(ready(panel), 0, 4, 1),
                gpui::Modifiers::default(),
                cx,
            );
            assert_eq!(ready(panel).move_offset(), (3, 0));
            panel.on_sheet_mouse_up(false, cx);
            // The move is floating: the document is untouched until commit.
            assert!(ready(panel).float.is_some(), "the drag lifted a float");
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 1, 1)],
                1,
                "the source is still in the document while floating"
            );
            assert_eq!(
                ready(panel).selection,
                Some(((3, 0), (5, 2))),
                "marquee followed"
            );

            panel.commit_float(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 1, 1)], 0, "source cleared on commit");
            assert_eq!(state.indices[idx(0, 4, 1)], 1, "moved");

            panel.undo_impl(cx);
            let state = ready(panel).store.state();
            assert_eq!(
                state.indices[idx(0, 1, 1)],
                1,
                "still ONE undo for the whole move"
            );
            assert_eq!(state.indices[idx(0, 4, 1)], 0);
        });
    }

    #[gpui::test]
    async fn test_flip_reverses_the_selection_and_delete_row_shrinks(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 0, 0), false, cx);
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
            panel.on_sheet_mouse_down(point(px(2.5 * z), px(3.5 * z)), false, cx);
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

    /// A double click is just two clicks. It used to drop the sheet into
    /// single-tile focus mode, which fires constantly while painting fast
    /// and yanks the whole view out from under you. Focus stays reachable
    /// deliberately, via `f` or the toolbar.
    #[gpui::test]
    async fn test_double_clicking_a_tile_paints_instead_of_entering_focus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(1, cx);
            panel.on_sheet_click(
                pixel_pos(ready(panel), 0, 2, 3),
                2,
                gpui::Modifiers::default(),
                false,
                cx,
            );
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(ready(panel).focus, None, "no hard focus on a double click");
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 2, 3)],
                1,
                "the second click still paints"
            );
        });
    }

    /// With a marquee up, the arrow keys shift the selected pixels instead
    /// of scrolling -- same operation dragging inside the marquee performs.
    /// With no selection they still scroll, and focus mode still steps.
    #[gpui::test]
    async fn test_arrows_nudge_the_selection_and_scroll_without_one(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Select, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                // Tile 1 is solid index 1; select its top-left 2x2.
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 1)));
            }
            let before = ready(panel).scroll.offset();

            panel.scroll_by(1.0, 0.0, cx);

            assert_eq!(
                panel.selection_rect(),
                Some((TILE_PX + 1, 0, TILE_PX + 2, 1)),
                "the marquee travelled one pixel right"
            );
            assert_eq!(
                ready(panel).scroll.offset(),
                before,
                "and the view did not scroll"
            );
        });

        panel.update(cx, |panel, cx| {
            // Escape drops the float first; the marquee needs a second one.
            panel.cancel_impl(cx);
            assert!(ready(panel).float.is_none(), "the float is gone");
            panel.cancel_impl(cx);
            assert!(ready(panel).selection.is_none());
            panel.scroll_by(0.0, 1.0, cx);
            assert!(
                ready(panel).scroll.offset().y < px(0.),
                "with no selection the arrows scroll again"
            );
        });
    }

    /// Every tool has its own key, and none of them collide with a key the
    /// panel already binds -- `f` is `FocusTile`, which is why Fill is `g`.
    #[test]
    fn every_tool_has_a_distinct_shortcut_that_is_not_already_taken() {
        let tools = [
            Tool::Pencil,
            Tool::Eraser,
            Tool::Picker,
            Tool::Select,
            Tool::Fill,
            Tool::Line,
            Tool::Rect,
            Tool::Ellipse,
        ];
        let keys: Vec<&str> = tools.into_iter().map(tool_shortcut).collect();
        for (i, key) in keys.iter().enumerate() {
            assert!(!key.is_empty(), "{:?} has no shortcut", tools[i]);
            assert!(
                !["f", "[", "]", "escape"].contains(key),
                "{key} is already bound in this panel"
            );
        }
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "shortcuts collide: {keys:?}");
    }

    /// Pressing outside the panel drops the marquee; anything inside it,
    /// such as reaching for a toolbar button, leaves the selection alone.
    #[gpui::test]
    async fn test_pressing_outside_the_panel_clears_the_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Select, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (3, 3)));
                open.move_drag = Some(((1, 1), (2, 2)));
            }

            // A toolbar press stays inside the panel.
            panel.set_tool(Tool::Select, cx);
            assert!(
                panel.selection_rect().is_some(),
                "the toolbar must not clear what its buttons act on"
            );

            panel.clear_selection_on_click_out(cx);
            assert!(panel.selection_rect().is_none(), "the marquee is gone");
            assert!(
                ready(panel).move_drag.is_none(),
                "and so is any half-finished move"
            );
        });
    }

    #[gpui::test]
    async fn test_delete_selection_clears_to_slot_zero_in_one_undo_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (2 * TILE_PX - 1, TILE_PX - 1)));
            }
            panel.erase_selection(cx);
            let state = ready(panel).store.state();
            for y in 0..TILE_PX {
                for x in 0..TILE_PX {
                    assert_eq!(state.indices[idx(1, x, y)], 0, "({x},{y}) cleared");
                }
            }
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 7, 7)],
                1,
                "one undo restores the whole region"
            );
            panel.redo_impl(cx);
            assert_eq!(ready(panel).store.state().indices[idx(1, 7, 7)], 0);
        });
    }

    #[gpui::test]
    async fn test_delete_without_a_marquee_is_a_no_op(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.erase_selection(cx);
            let state = ready(panel).store.state();
            assert!(!state.dirty, "no marquee, nothing written");
            assert_eq!(state.indices[idx(1, 7, 7)], 1, "the art is untouched");
        });
    }

    /// Blanking an already-blank region must not push an undo entry --
    /// `apply_stroke_paint` drops a same-colour write.
    #[gpui::test]
    async fn test_deleting_an_already_blank_region_pushes_no_undo_entry(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            // Tile 0 is all zeros in the fixture.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (TILE_PX - 1, TILE_PX - 1)));
            }
            panel.erase_selection(cx);
            assert!(!ready(panel).store.state().dirty, "nothing changed");
        });
    }

    #[gpui::test]
    async fn test_cut_copies_then_clears_and_paste_restores(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (2 * TILE_PX - 1, TILE_PX - 1)));
            }
            panel.cut_selection(cx);
            let clip = TilesetPanel::clip(cx).expect("cut filled the clipboard");
            assert_eq!((clip.w, clip.h), (TILE_PX, TILE_PX));
            assert!(clip.pixels.iter().all(|&v| v == 1), "the copy took the art");
            let open = ready(panel);
            assert_eq!(
                open.store.state().indices[idx(1, 7, 7)],
                0,
                "and the region is blank"
            );

            panel.paste_clipboard(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 7, 7)],
                1,
                "paste puts it back"
            );
        });
    }

    #[gpui::test]
    async fn test_delete_over_pad_cells_does_not_panic(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(2, cx);
            panel.select_whole_sheet(cx);
            panel.erase_selection(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.tile_count, 3, "no tile was added or removed");
            for tile in 0..3 {
                assert_eq!(state.indices[idx(tile, 0, 0)], 0, "tile {tile} cleared");
            }
        });
    }

    #[gpui::test]
    async fn test_delete_in_focus_mode_clears_only_the_focused_tile(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.enter_focus(1, cx);
            panel.select_whole_sheet(cx);
            panel.erase_selection(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 7, 7)], 0, "the focused tile cleared");
            assert_eq!(state.indices[idx(2, 7, 7)], 1, "tile 2 untouched");
        });
    }

    /// Every action this panel defines is reachable from a key, or is
    /// explicitly allowlisted as toolbar-only. Catches an action that ships
    /// with a handler but no way to invoke it -- which is how ZoomIn,
    /// ZoomOut and SelectWholeSheet all sat unbound.
    #[test]
    fn every_ggo_tileset_action_is_bound_or_allowlisted() {
        // Reachable from the command palette; deliberately keyless,
        // because the panel's single-letter budget is spent on the tools.
        const UNBOUND_BY_DESIGN: &[&str] = &[
            "AppendBlankTile",
            "NextTile",
            "PrevTile",
            "ToggleLines",
            "ToggleMirrorHorizontal",
            "ToggleMirrorVertical",
            "ToggleSnapTiles",
            "TogglePasteOpaque",
        ];
        let actions = [
            "Undo",
            "Redo",
            "Save",
            "ScrollLeft",
            "ScrollRight",
            "ScrollUp",
            "ScrollDown",
            "CopySelection",
            "PasteSelection",
            "CutSelection",
            "DeleteSelection",
            "ZoomIn",
            "ZoomOut",
            "ResetZoom",
            "SelectWholeSheet",
            "BrushSmaller",
            "BrushLarger",
            "FlipHorizontal",
            "FlipVertical",
            "Cancel",
            "FocusTile",
            "UsePencil",
            "UseEraser",
            "UsePicker",
            "UseSelect",
            "UseFill",
            "UseLine",
            "UseRect",
            "UseEllipse",
            "DuplicateFocusedTile",
            "AppendBlankTile",
            "NextSlot",
            "PrevSlot",
            "NextTile",
            "PrevTile",
            "ToggleLines",
            "ToggleMirrorHorizontal",
            "ToggleMirrorVertical",
            "ToggleSnapTiles",
            "TogglePasteOpaque",
        ];
        for keymap in [
            "/../../../assets/keymaps/default-linux.json",
            "/../../../assets/keymaps/default-macos.json",
            "/../../../assets/keymaps/default-windows.json",
        ] {
            let path = format!("{}{keymap}", env!("CARGO_MANIFEST_DIR"));
            let text = std::fs::read_to_string(&path).expect("keymap readable");
            let start = text
                .find(r#""context": "GgoTilesetPanel""#)
                .unwrap_or_else(|| panic!("no GgoTilesetPanel block in {path}"));
            let block = &text[start..start + text[start..].find("\n  },").expect("block ends")];
            for action in actions {
                if UNBOUND_BY_DESIGN.contains(&action) {
                    continue;
                }
                assert!(
                    block.contains(&format!("ggo_tileset::{action}\"")),
                    "{action} has no binding in {path}"
                );
            }
        }
    }

    #[gpui::test]
    async fn test_duplicate_appends_a_copy_of_the_focused_tile(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.enter_focus(1, cx);
            panel.duplicate_tile(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.tile_count, 4);
            for y in 0..TILE_PX {
                for x in 0..TILE_PX {
                    assert_eq!(state.indices[idx(3, x, y)], state.indices[idx(1, x, y)]);
                }
            }
        });
    }

    #[gpui::test]
    async fn test_duplicate_targets_the_tile_under_the_marquee(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((2 * TILE_PX, 0), (2 * TILE_PX + 3, 3)));
            }
            panel.duplicate_tile(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.tile_count, 4);
            assert_eq!(state.indices[idx(3, 7, 7)], state.indices[idx(2, 7, 7)]);
            assert_eq!(ready(panel).focus, None, "duplicating did not enter focus");
        });
    }

    #[gpui::test]
    async fn test_duplicate_with_no_focus_and_no_marquee_is_a_no_op(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.duplicate_tile(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.tile_count, 3);
            assert!(!state.dirty);
        });
    }

    /// A marquee anchored on a pad cell resolves to no tile. worldlib's
    /// DuplicateTile slices `indices[start..start + TILE_PIXELS]` raw and
    /// would panic, so the guard has to live here.
    #[gpui::test]
    async fn test_duplicate_from_a_pad_cell_marquee_is_a_no_op(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(2, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                // 3 tiles at 2 cols: the second row's right cell is padding.
                open.selection = Some(((TILE_PX, TILE_PX), (TILE_PX + 3, TILE_PX + 3)));
            }
            panel.duplicate_tile(cx);
            assert_eq!(ready(panel).store.state().tile_count, 3);
        });
    }

    #[gpui::test]
    async fn test_undo_after_duplicate_restores_the_tile_count_and_grid_size(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let before = ready(panel).grid_size;
            panel.enter_focus(0, cx);
            panel.leave_focus(cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (3, 3)));
            }
            panel.duplicate_tile(cx);
            assert_eq!(ready(panel).store.state().tile_count, 4);
            assert_ne!(ready(panel).grid_size, before, "the sheet grew");
            panel.undo_impl(cx);
            assert_eq!(ready(panel).store.state().tile_count, 3);
            assert_eq!(ready(panel).grid_size, before, "and the view shrank back");
        });
    }

    #[gpui::test]
    async fn test_append_adds_one_blank_tile_and_undo_removes_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.append_tile(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.tile_count, 4);
            assert!(
                (0..TILE_PX).all(|y| (0..TILE_PX).all(|x| state.indices[idx(3, x, y)] == 0)),
                "the appended tile is blank"
            );
            panel.undo_impl(cx);
            assert_eq!(ready(panel).store.state().tile_count, 3);
        });
    }

    #[gpui::test]
    async fn test_duplicating_while_focused_moves_focus_to_the_copy(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.enter_focus(1, cx);
            panel.duplicate_tile(cx);
            assert_eq!(ready(panel).focus, Some(3), "the view follows the copy");
            assert_eq!(
                ready(panel).grid_size,
                (TILE_PX as u32, TILE_PX as u32),
                "still one magnified tile"
            );
        });
    }

    #[gpui::test]
    async fn test_duplicate_on_a_partial_last_row_fills_the_row(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(2, cx);
            assert_eq!(
                grid_regions(3, 2),
                vec![(0, 1, 2), (1, 1, 1)],
                "partial row"
            );
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (3, 3)));
            }
            panel.duplicate_tile(cx);
            assert_eq!(ready(panel).store.state().tile_count, 4);
            assert_eq!(
                grid_regions(4, 2),
                vec![(0, 2, 2)],
                "the row is full, so no partial block is stroked"
            );
        });
    }

    /// Paste composites: a transparent source pixel leaves the destination
    /// alone. Copying a tile with art on transparent ground and pasting it
    /// over another tile used to punch the whole ground through.
    #[gpui::test]
    async fn test_paste_skips_transparent_source_pixels(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let mut data = vec![0u8; TILE_PX * TILE_PX];
            for x in 0..TILE_PX {
                data[x] = 1; // row 0 only
            }
            TilesetPanel::set_clip(TILE_PX, TILE_PX, data, [0u16; PAL_SLOTS], cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                // Destination: tile 1, solid slot 1 in the fixture.
                open.selection = Some(((TILE_PX, 0), (2 * TILE_PX - 1, TILE_PX - 1)));
            }
            panel.paste_clipboard(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 0, 0)], 1, "row 0 written");
            assert_eq!(
                state.indices[idx(1, 0, 5)],
                1,
                "row 5 kept its own art, not a hole"
            );
        });
    }

    #[gpui::test]
    async fn test_opaque_paste_writes_the_zeros(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let data = vec![0u8; TILE_PX * TILE_PX];
            TilesetPanel::set_clip(TILE_PX, TILE_PX, data, [0u16; PAL_SLOTS], cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (2 * TILE_PX - 1, TILE_PX - 1)));
                open.paste_opaque = true;
            }
            panel.paste_clipboard(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 0, 5)],
                0,
                "opaque paste punches the hole through"
            );
        });
    }

    #[gpui::test]
    async fn test_paste_without_a_marquee_lands_on_a_tile_boundary(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            TilesetPanel::set_clip(
                TILE_PX,
                TILE_PX,
                vec![2u8; TILE_PX * TILE_PX],
                [0u16; PAL_SLOTS],
                cx,
            );
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = None;
            }
            panel.paste_clipboard(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(0, 0, 0)], 2, "landed at the sheet origin");
            assert_eq!(
                panel.selection_rect(),
                Some((0, 0, TILE_PX - 1, TILE_PX - 1)),
                "and the paste is selected so it can be nudged"
            );
        });
    }

    #[gpui::test]
    async fn test_paste_in_focus_mode_without_a_marquee_lands_at_the_tile_origin(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.enter_focus(2, cx);
            TilesetPanel::set_clip(
                TILE_PX,
                TILE_PX,
                vec![2u8; TILE_PX * TILE_PX],
                [0u16; PAL_SLOTS],
                cx,
            );
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = None;
            }
            panel.paste_clipboard(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(2, 0, 0)],
                2,
                "the focused tile received it at its origin"
            );
        });
    }

    #[gpui::test]
    async fn test_paste_with_an_empty_clipboard_is_a_no_op(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.paste_clipboard(cx);
            assert!(!ready(panel).store.state().dirty);
        });
    }

    #[gpui::test]
    async fn test_an_all_transparent_clipboard_paste_pushes_no_undo_entry(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            TilesetPanel::set_clip(
                TILE_PX,
                TILE_PX,
                vec![0u8; TILE_PX * TILE_PX],
                [0u16; PAL_SLOTS],
                cx,
            );
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (2 * TILE_PX - 1, TILE_PX - 1)));
            }
            panel.paste_clipboard(cx);
            assert!(
                !ready(panel).store.state().dirty,
                "every source pixel was skipped, so nothing was written"
            );
        });
    }

    #[gpui::test]
    async fn test_paste_clipped_by_the_sheet_edge_writes_only_in_sheet_pixels(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(2, cx);
            TilesetPanel::set_clip(
                TILE_PX,
                TILE_PX,
                vec![2u8; TILE_PX * TILE_PX],
                [0u16; PAL_SLOTS],
                cx,
            );
            if let ViewerState::Ready(open) = &mut panel.state {
                // The pad cell's origin on the second row.
                open.selection = Some(((TILE_PX, TILE_PX), (2 * TILE_PX - 1, 2 * TILE_PX - 1)));
            }
            panel.paste_clipboard(cx);
            assert_eq!(
                ready(panel).store.state().tile_count,
                3,
                "a paste never grows the sheet"
            );
        });
    }

    /// Snapping expands the NORMALIZED rect outward, which is why an
    /// up-left drag works: snapping the raw anchor down and the raw head up
    /// would put the head below the anchor and normalization would then eat
    /// both outer halves.
    #[gpui::test]
    async fn test_snap_expands_the_marquee_to_whole_tiles(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, _| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.snap_tiles = true;
                // A bare click inside tile 1.
                open.selection = Some(((TILE_PX + 4, 5), (TILE_PX + 4, 5)));
            }
            assert_eq!(
                panel.selection_rect(),
                Some((TILE_PX, 0, 2 * TILE_PX - 1, TILE_PX - 1)),
                "a click selects exactly one tile"
            );

            if let ViewerState::Ready(open) = &mut panel.state {
                // Dragged UP-LEFT: head is above/left of the anchor.
                open.selection = Some(((TILE_PX + 1, 3), (2, 2)));
            }
            assert_eq!(
                panel.selection_rect(),
                Some((0, 0, 2 * TILE_PX - 1, TILE_PX - 1)),
                "expanded outward in both directions"
            );
        });
    }

    #[gpui::test]
    async fn test_snap_clamps_to_the_sheet_edge(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(2, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.snap_tiles = true;
                open.selection = Some(((TILE_PX + 1, TILE_PX + 1), (TILE_PX + 2, TILE_PX + 2)));
            }
            let (w, h) = ready(panel).grid_size;
            let (_, _, x1, y1) = panel.selection_rect().expect("a rect");
            assert!(
                x1 < w as usize && y1 < h as usize,
                "clamped inside the sheet"
            );
            // Must not panic when the region is read back.
            panel.copy_selection(cx);
            assert!(TilesetPanel::clip(cx).is_some());
        });
    }

    #[gpui::test]
    async fn test_snap_off_keeps_pixel_precision(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, _| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.snap_tiles = false;
                open.selection = Some(((2, 2), (5, 4)));
            }
            assert_eq!(
                panel.selection_rect(),
                Some((2, 2, 5, 4)),
                "a pixel editor must still select pixels"
            );
        });
    }

    #[gpui::test]
    async fn test_arrows_nudge_by_a_whole_tile_when_snapping(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.snap_tiles = true;
                open.selection = Some(((TILE_PX + 2, 2), (TILE_PX + 2, 2)));
            }
            assert_eq!(panel.nudge_step(), TILE_PX as i32);
            panel.scroll_by(1.0, 0.0, cx);
            assert!(ready(panel).float.is_some(), "the nudge lifted a float");

            panel.commit_float(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(2, 7, 7)],
                1,
                "tile 1's art moved a whole tile right, into tile 2"
            );
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 7, 7)],
                0,
                "and vacated the tile it left"
            );
        });
    }

    #[gpui::test]
    async fn test_snap_in_focus_mode_selects_the_whole_focused_tile(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.enter_focus(1, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.snap_tiles = true;
                open.selection = Some(((3, 4), (3, 4)));
            }
            assert_eq!(
                panel.selection_rect(),
                Some((0, 0, TILE_PX - 1, TILE_PX - 1)),
                "the focus sheet IS one tile"
            );
        });
    }

    /// worldlib's brush_expand grows down-right from each point, which put
    /// the cursor on the footprint's top-left CORNER: every wide stroke
    /// landed low and right of where it was aimed.
    #[gpui::test]
    async fn test_a_wide_brush_is_centred_on_the_cursor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.step_brush(2, cx); // 1 -> 3
            assert_eq!(ready(panel).brush, 3);
            let points = ready(panel).expand_points(&[(5, 5)]);
            assert!(points.contains(&(4, 4)), "reaches up-left of the cursor");
            assert!(points.contains(&(6, 6)), "and down-right");
            assert!(
                !points.contains(&(7, 7)),
                "a 3px brush centred on (5,5) stops at 6"
            );
        });
    }

    /// The preview drew `shape_points(false)` unconditionally, so holding
    /// shift showed an outline and then painted a filled shape.
    #[gpui::test]
    async fn test_the_shape_preview_matches_what_a_release_would_paint(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Rect, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 2, 2), false, cx);
            panel.on_sheet_mouse_move(
                pixel_pos(ready(panel), 0, 8, 8),
                gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
                cx,
            );

            let open = ready(panel);
            assert!(open.shape_filled, "the drag recorded the modifier");
            let previewed = open.shape_points(open.shape_filled);
            assert_eq!(
                previewed,
                open.shape_points(true),
                "the preview is what a shift release paints"
            );
            assert!(
                previewed.contains(&(5, 5)),
                "a filled rect covers its interior"
            );

            // Releasing the modifier mid-drag must flip the preview back.
            panel.on_sheet_mouse_move(
                pixel_pos(ready(panel), 0, 8, 8),
                gpui::Modifiers::default(),
                cx,
            );
            let open = ready(panel);
            assert!(!open.shape_filled);
            assert!(
                !open.shape_points(open.shape_filled).contains(&(5, 5)),
                "an outline again"
            );
        });
    }

    /// Colour selection was mouse-only: neither select_slot nor
    /// set_palette_slot had an action behind it.
    #[gpui::test]
    async fn test_slot_stepping_wraps_the_palette(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.select_slot(0, cx);
            panel.step_slot(-1, cx);
            assert_eq!(ready(panel).slot, PAL_SLOTS - 1, "wraps backwards off zero");
            panel.step_slot(1, cx);
            assert_eq!(ready(panel).slot, 0, "and forwards off the end");
            panel.step_slot(3, cx);
            assert_eq!(ready(panel).slot, 3);

            // Cycling colours must not steal the active tool.
            panel.set_tool(Tool::Rect, cx);
            panel.step_slot(1, cx);
            assert_eq!(ready(panel).slot, 4);
            assert_eq!(ready(panel).tool, Tool::Rect, "still the Rect tool");
        });
    }

    #[gpui::test]
    async fn test_reset_zoom_returns_to_the_default(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.zoom_by(2, cx);
            assert_ne!(ready(panel).zoom, DEFAULT_ZOOM);
            panel.reset_zoom(cx);
            assert_eq!(ready(panel).zoom, DEFAULT_ZOOM);
        });
    }

    /// Up and down were dead keys in focus mode: the sheet is one tile, so
    /// there was nothing to scroll and only left/right stepped.
    #[gpui::test]
    async fn test_up_and_down_step_a_row_in_focus_mode(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(2, cx);
            panel.enter_focus(0, cx);
            panel.scroll_by(0.0, 1.0, cx);
            assert_eq!(ready(panel).focus, Some(2), "down steps a whole row of 2");
            panel.scroll_by(0.0, -1.0, cx);
            assert_eq!(ready(panel).focus, Some(0), "and up comes back");
            panel.scroll_by(0.0, -1.0, cx);
            assert_eq!(ready(panel).focus, Some(0), "clamped at the first tile");
        });
    }

    /// Right-drag paints the secondary colour. In a 16-slot indexed palette
    /// where slot 0 IS transparency, that means erase -- the most-used
    /// Aseprite gesture after left-drag, and the sheet registered Left
    /// handlers only.
    #[gpui::test]
    async fn test_right_drag_erases_and_restores_the_primary_slot(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(2, cx);
            // Tile 1 is solid slot 1.
            panel.on_sheet_click(
                pixel_pos(ready(panel), 1, 3, 3),
                1,
                gpui::Modifiers::default(),
                true,
                cx,
            );
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 3, 3)],
                0,
                "right-drag erased rather than painting slot 2"
            );
            assert!(
                !ready(panel).secondary_paint,
                "the flag is cleared on release"
            );

            // A left press afterwards paints the primary slot again.
            panel.on_sheet_click(
                pixel_pos(ready(panel), 1, 5, 5),
                1,
                gpui::Modifiers::default(),
                false,
                cx,
            );
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(ready(panel).store.state().indices[idx(1, 5, 5)], 2);
        });
    }

    /// Alt samples the colour under the cursor with any tool active and
    /// hands that tool straight back. Picking with the Picker TOOL still
    /// switches to Pencil, which the beginner persona relies on.
    #[gpui::test]
    async fn test_alt_click_samples_without_stealing_the_tool(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(3, cx);
            panel.set_tool(Tool::Rect, cx);
            let alt = gpui::Modifiers {
                alt: true,
                ..Default::default()
            };
            panel.on_sheet_click(pixel_pos(ready(panel), 1, 3, 3), 1, alt, false, cx);
            let open = ready(panel);
            assert_eq!(open.slot, 1, "sampled tile 1's colour");
            assert_eq!(open.tool, Tool::Rect, "and kept the Rect tool");
            assert!(!open.store.state().dirty, "sampling paints nothing");
        });
    }

    #[gpui::test]
    async fn test_the_picker_tool_still_hands_you_the_pencil(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Picker, cx);
            panel.on_sheet_click(
                pixel_pos(ready(panel), 1, 3, 3),
                1,
                gpui::Modifiers::default(),
                false,
                cx,
            );
            let open = ready(panel);
            assert_eq!(open.slot, 1);
            assert_eq!(open.tool, Tool::Pencil, "unchanged behaviour");
        });
    }

    /// The tile index appeared nowhere in the UI except the Focus button's
    /// label, so there was no way to tell tile 12 from tile 13.
    #[gpui::test]
    async fn test_the_status_line_reports_the_tile_under_the_cursor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.hover_status(), None, "nothing hovered yet");

            panel.on_sheet_mouse_move(
                pixel_pos(ready(panel), 1, 3, 4),
                gpui::Modifiers::default(),
                cx,
            );
            assert_eq!(
                panel.hover_status().as_deref(),
                Some("tile 1 · px 3,4"),
                "the tile AND the in-tile pixel"
            );

            panel.clear_hover(cx);
            assert_eq!(
                panel.hover_status(),
                None,
                "cleared when the pointer leaves"
            );
        });
    }

    /// A pad cell backs no tile. Say so rather than reporting nothing or,
    /// worse, a neighbouring tile's index.
    #[gpui::test]
    async fn test_the_status_line_names_a_pad_cell(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| panel.set_cols(2, cx));
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            // 3 tiles at 2 cols: the second row's right cell is padding.
            let z = ready(panel).zoom as f32;
            let pos = point(
                px((TILE_PX as f32 + 4.0) * z),
                px((TILE_PX as f32 + 4.0) * z),
            );
            panel.on_sheet_mouse_move(pos, gpui::Modifiers::default(), cx);
            assert_eq!(panel.hover_status().as_deref(), Some("pad cell"));
        });
    }

    /// The checker cell grows so a huge sheet cannot cost tens of thousands
    /// of quads a frame.
    #[test]
    fn checker_cell_stays_within_its_quad_budget() {
        assert_eq!(checker_cell_px(192.0, 64.0), 8.0, "a small sheet gets 8px");
        for (w, h) in [(192.0, 64.0), (4096.0, 4096.0), (16384.0, 16384.0)] {
            let cell = checker_cell_px(w, h);
            let cells = (w / cell).ceil() * (h / cell).ceil();
            assert!(
                cells <= CHECKER_MAX_CELLS,
                "{w}x{h} needs {cells} cells at {cell}px"
            );
            assert!(cell >= 8.0, "never finer than 8px");
        }
    }

    /// Ctrl constrains, NOT shift: shift already means "filled" on these
    /// exact tools, and is spoken for twice more on this sheet.
    #[test]
    fn constrain_head_squares_boxes_and_snaps_lines_to_eight_directions() {
        let a = (10, 10);
        // Rect/Ellipse: the longer axis wins, sign preserved.
        assert_eq!(constrain_head(a, (20, 13), Tool::Rect), (20, 20));
        assert_eq!(constrain_head(a, (13, 20), Tool::Rect), (20, 20));
        assert_eq!(constrain_head(a, (2, 13), Tool::Ellipse), (2, 18));
        assert_eq!(
            constrain_head(a, (4, 4), Tool::Rect),
            (4, 4),
            "up-left stays square"
        );

        // Line: shallow -> horizontal, steep -> vertical, else diagonal.
        assert_eq!(constrain_head(a, (30, 12), Tool::Line), (30, 10));
        assert_eq!(constrain_head(a, (12, 30), Tool::Line), (10, 30));
        assert_eq!(constrain_head(a, (20, 18), Tool::Line), (20, 20));
        assert_eq!(
            constrain_head(a, (0, 20), Tool::Line),
            (0, 20),
            "an exact down-left 45 is left alone"
        );
        assert_eq!(
            constrain_head(a, (0, 22), Tool::Line),
            (-2, 22),
            "a near-45 extends along the LONGER axis, even off-sheet"
        );

        // A drag that has not moved must still be well-formed.
        assert_eq!(constrain_head(a, a, Tool::Rect), a);
        // Non-shape tools are untouched.
        assert_eq!(constrain_head(a, (3, 7), Tool::Pencil), (3, 7));
    }

    #[gpui::test]
    async fn test_ctrl_constrains_a_rect_drag_to_a_square(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Rect, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 2, 2), false, cx);
            panel.on_sheet_mouse_move(
                pixel_pos(ready(panel), 0, 10, 4),
                gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
                cx,
            );
            let open = ready(panel);
            assert!(open.shape_constrained, "the drag recorded ctrl");
            let points = open.shape_points(false);
            // 2,2 -> 10,4 constrained is 2,2 -> 10,10: a square outline.
            assert!(points.contains(&(10, 10)), "the far corner squared up");
            assert!(
                points.contains(&(5, 10)),
                "the square's bottom edge runs at y=10"
            );
            assert!(
                !points.contains(&(5, 4)),
                "not the raw 8x2 box, whose bottom edge was y=4"
            );
        });
    }

    #[gpui::test]
    async fn test_ctrl_wheel_zooms_and_a_plain_wheel_does_not(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            let before = ready(panel).zoom;
            assert!(
                !panel.on_sheet_scroll(20.0, false, None, cx),
                "plain wheel declines"
            );
            assert_eq!(ready(panel).zoom, before, "so the container can scroll");

            assert!(panel.on_sheet_scroll(20.0, true, None, cx));
            assert_eq!(ready(panel).zoom, before + 1);
            assert!(panel.on_sheet_scroll(-20.0, true, None, cx));
            assert_eq!(ready(panel).zoom, before);

            assert!(
                !panel.on_sheet_scroll(0.0, true, None, cx),
                "a zero delta is not a zoom"
            );
        });
    }

    /// Zooming kept the scroll offset, so the view lurched toward the
    /// origin on every step and took whatever you were working on with it.
    #[gpui::test]
    async fn test_zoom_keeps_the_sheet_pixel_under_the_cursor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            let before = ready(panel).zoom;
            // Screen position of sheet pixel (20, 6) before the zoom.
            let anchored = |panel: &TilesetPanel| {
                let open = ready(panel);
                let o = open.scroll.offset();
                (
                    f32::from(o.x) + 20.0 * open.zoom as f32,
                    f32::from(o.y) + 6.0 * open.zoom as f32,
                )
            };
            let at = anchored(panel);

            panel.zoom_at_sheet_px(1, (20, 6), cx);
            assert_eq!(ready(panel).zoom, before + 1, "it did zoom");
            let after = anchored(panel);
            assert!(
                (after.0 - at.0).abs() < 0.01 && (after.1 - at.1).abs() < 0.01,
                "the pixel stayed put: {at:?} -> {after:?}"
            );
        });
    }

    /// Offsets are negative-or-zero, so anchoring must never scroll the
    /// sheet away from its own origin.
    #[gpui::test]
    async fn test_zoom_anchoring_never_pushes_past_the_origin(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.zoom_at_sheet_px(-1, (40, 12), cx);
            let o = ready(panel).scroll.offset();
            assert!(f32::from(o.x) <= 0.0 && f32::from(o.y) <= 0.0, "{o:?}");
        });
    }

    /// Space turns a press into a pan instead of a paint, and the sheet
    /// follows the cursor one-to-one.
    #[gpui::test]
    async fn test_space_drag_pans_instead_of_painting(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.select_slot(2, cx);
            panel.set_space_held(true, cx);

            let start = pixel_pos(ready(panel), 1, 3, 3);
            panel.on_sheet_click(start, 1, gpui::Modifiers::default(), false, cx);
            assert!(ready(panel).pan_drag.is_some(), "a pan began");
            assert!(!ready(panel).store.state().dirty, "and it painted nothing");

            // Drag left/up: the offset follows by the same delta, clamped.
            panel.on_sheet_mouse_move(
                point(start.x - px(30.), start.y - px(10.)),
                gpui::Modifiers::default(),
                cx,
            );
            let o = ready(panel).scroll.offset();
            assert_eq!(f32::from(o.x), -30.0);
            assert_eq!(f32::from(o.y), -10.0);

            panel.on_sheet_mouse_up(false, cx);
            assert!(ready(panel).pan_drag.is_none(), "release ends the pan");
        });
    }

    /// Releasing space anywhere ends a pan, so a release off the sheet
    /// cannot strand the drag and swallow the next press.
    #[gpui::test]
    async fn test_releasing_space_ends_a_pan_in_flight(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_space_held(true, cx);
            panel.on_sheet_click(
                pixel_pos(ready(panel), 0, 1, 1),
                1,
                gpui::Modifiers::default(),
                false,
                cx,
            );
            assert!(ready(panel).pan_drag.is_some());

            panel.set_space_held(false, cx);
            assert!(ready(panel).pan_drag.is_none(), "the pan is gone");
            assert!(!ready(panel).space_held);

            // And a normal press paints again.
            panel.select_slot(2, cx);
            panel.on_sheet_click(
                pixel_pos(ready(panel), 0, 1, 1),
                1,
                gpui::Modifiers::default(),
                false,
                cx,
            );
            panel.on_sheet_mouse_up(false, cx);
            assert_eq!(ready(panel).store.state().indices[idx(0, 1, 1)], 2);
        });
    }

    /// insert_column's own doc comment admitted this: undo restored the
    /// tile strip but left `cols` widened, so the sheet rewrapped and every
    /// tile appeared to move. `cols` is a view property the store cannot
    /// restore, so the panel pins it to the document revision.
    #[gpui::test]
    async fn test_undo_of_a_column_op_restores_the_view_width(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(3, cx);
            let before = ready(panel).grid_size;

            panel.insert_column(false, cx);
            assert_eq!(ready(panel).cols, 4);
            assert_ne!(ready(panel).grid_size, before);

            panel.undo_impl(cx);
            assert_eq!(ready(panel).cols, 3, "the view width came back");
            assert_eq!(ready(panel).grid_size, before, "and so did the layout");

            panel.redo_impl(cx);
            assert_eq!(ready(panel).cols, 4, "redo re-widens it");
        });
    }

    #[gpui::test]
    async fn test_undo_of_a_column_delete_restores_the_view_width(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(3, cx);
            panel.delete_column(true, cx);
            assert_eq!(ready(panel).cols, 2);
            panel.undo_impl(cx);
            assert_eq!(ready(panel).cols, 3, "a delete undoes its width too");
        });
    }

    /// A paint between the column op and the undo must not confuse the
    /// pinning: only the revision that actually changed `cols` restores it.
    #[gpui::test]
    async fn test_an_unrelated_undo_leaves_the_view_width_alone(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_cols(3, cx);
            panel.insert_column(false, cx);
            assert_eq!(ready(panel).cols, 4);

            panel.select_slot(2, cx);
            panel.set_tool(Tool::Pencil, cx);
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 0, 1, 1), false, cx);
            panel.on_sheet_mouse_up(false, cx);

            // Undo the PAINT: the width must not move.
            panel.undo_impl(cx);
            assert_eq!(ready(panel).cols, 4, "the paint did not change cols");

            // Undo the column op: now it must.
            panel.undo_impl(cx);
            assert_eq!(ready(panel).cols, 3);
        });
    }

    /// A new op after an undo clears the redo stack, so pins recorded above
    /// the current revision are unreachable and must not fire later.
    #[gpui::test]
    async fn test_a_new_op_after_an_undo_discards_stale_column_pins(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_cols(3, cx);
            panel.insert_column(false, cx);
            panel.undo_impl(cx);
            assert_eq!(ready(panel).cols, 3);

            // A different op now occupies that revision.
            panel.append_tile(cx);
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).cols,
                3,
                "undoing the append must not resurrect the old column pin"
            );
        });
    }

    /// Every nudge used to be a full read-blank-write with its own undo
    /// entry. Ten nudges meant ten mutations and ten undo steps.
    #[gpui::test]
    async fn test_many_nudges_are_one_undo_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 1)));
            }
            for _ in 0..10 {
                panel.move_selection((1, 0), cx);
            }
            assert!(!ready(panel).store.state().dirty, "nothing written yet");
            panel.commit_float(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 10, 0)],
                1,
                "landed ten pixels right"
            );
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 0, 0)],
                0,
                "source blanked once, at commit"
            );

            panel.undo_impl(cx);
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 0, 0)], 1, "ONE undo for ten nudges");
            assert_eq!(state.indices[idx(1, 10, 0)], 1, "fixture art is back");
        });
    }

    /// Pixels pushed past the sheet edge used to be filtered out by
    /// doc_pixel and destroyed. A float carries them, so moving back
    /// recovers them.
    #[gpui::test]
    async fn test_pixels_carried_off_the_sheet_come_back(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                // Tile 1's left edge: solid slot 1.
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 3, 0)));
            }
            for _ in 0..40 {
                panel.move_selection((-1, 0), cx);
            }
            assert!(
                panel.float_rect().is_none(),
                "entirely off the left edge now"
            );
            for _ in 0..40 {
                panel.move_selection((1, 0), cx);
            }
            panel.commit_float(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 0, 0)],
                1,
                "the round trip lost nothing"
            );
        });
    }

    /// Cancelling is free because the document was never touched.
    #[gpui::test]
    async fn test_cancelling_a_float_leaves_the_document_untouched(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 3, 3)));
            }
            panel.move_selection((5, 5), cx);
            assert!(ready(panel).float.is_some());

            assert!(panel.cancel_float(cx));
            assert!(ready(panel).float.is_none());
            assert!(
                !ready(panel).store.state().dirty,
                "no undo entry to roll back"
            );
            assert_eq!(ready(panel).store.state().indices[idx(1, 0, 0)], 1);
        });
    }

    /// Leaving the Select tool, saving, or pressing outside the float all
    /// put it down rather than stranding it invisibly.
    #[gpui::test]
    async fn test_changing_tool_commits_a_float(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel.set_tool(Tool::Select, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 0)));
            }
            panel.move_selection((4, 0), cx);
            assert!(ready(panel).float.is_some());

            panel.set_tool(Tool::Pencil, cx);
            assert!(ready(panel).float.is_none(), "the tool change put it down");
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 4, 0)],
                1,
                "and it landed"
            );
        });
    }

    /// A float over other art must not destroy it until commit, and then
    /// only where the float is opaque.
    #[gpui::test]
    async fn test_a_float_does_not_destroy_art_it_passes_over(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            // Tile 0 is transparent; lift a 2x1 of it and drag it ACROSS
            // tile 1's solid art and back.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (1, 0)));
            }
            for _ in 0..TILE_PX + 4 {
                panel.move_selection((1, 0), cx);
            }
            for _ in 0..TILE_PX + 4 {
                panel.move_selection((-1, 0), cx);
            }
            panel.commit_float(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 4, 0)],
                1,
                "tile 1's art survived the pass-over"
            );
        });
    }

    /// The float is shown by composing it into the sheet IMAGE while the
    /// document stays untouched, so what you see is the moved art and what
    /// undo sees is still one pending step.
    #[gpui::test]
    async fn test_the_composed_view_shows_the_float_but_the_document_does_not(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                // Tile 1's top-left 2x1, solid slot 1.
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 0)));
            }
            assert!(
                panel.float_composed_indices().is_none(),
                "nothing floating yet"
            );

            panel.move_selection((4, 0), cx);
            let view = panel
                .float_composed_indices()
                .expect("the view reflects the float");
            assert_eq!(view[idx(1, 0, 0)], 0, "source blanked in the VIEW");
            assert_eq!(view[idx(1, 4, 0)], 1, "float stamped in the VIEW");

            let doc = &ready(panel).store.state().indices;
            assert_eq!(doc[idx(1, 0, 0)], 1, "document still has the source");
            assert!(!ready(panel).store.state().dirty, "and is not dirty");
        });
    }

    /// Copy, flip and delete act on the FLOAT when one exists. Reading the
    /// sheet under it would take whatever it is hovering over.
    #[gpui::test]
    async fn test_copy_and_flip_act_on_the_float_not_the_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            // A 2x1 whose left pixel is slot 1 and right pixel transparent.
            panel.select_slot(1, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX - 1, 0), (TILE_PX, 0)));
            }
            // Move it clear of tile 1's solid art, onto transparent tile 0.
            panel.move_selection((-8, 0), cx);
            let carried = ready(panel).float.clone().expect("floating");
            assert_eq!(carried.pixels, vec![0, 1], "lifted from the tile boundary");

            panel.copy_selection(cx);
            let clip = TilesetPanel::clip(cx).expect("copied");
            assert_eq!(
                (clip.w, clip.h, clip.pixels),
                (2, 1, vec![0, 1]),
                "copied the float, not the sheet under it"
            );

            panel.flip_selection(true, cx);
            assert_eq!(
                ready(panel).float.as_ref().unwrap().pixels,
                vec![1, 0],
                "the flip rearranged the carried buffer"
            );
            assert!(
                !ready(panel).store.state().dirty,
                "and wrote nothing to the document"
            );
        });
    }

    /// Deleting a float throws the carried pixels away and blanks where
    /// they came from, in one step.
    #[gpui::test]
    async fn test_deleting_a_float_blanks_its_origin(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 0)));
            }
            panel.move_selection((6, 0), cx);
            panel.erase_selection(cx);

            assert!(ready(panel).float.is_none(), "the float is gone");
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 0, 0)], 0, "origin blanked");
            assert_eq!(
                state.indices[idx(1, 6, 0)],
                1,
                "and nothing was stamped at the destination"
            );
        });
    }

    /// A structural op puts the float down first, so it cannot be stranded
    /// against a sheet whose shape just changed underneath it.
    #[gpui::test]
    async fn test_a_structural_op_commits_the_float_first(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 0)));
            }
            panel.move_selection((4, 0), cx);
            assert!(ready(panel).float.is_some());

            panel.append_tile(cx);
            assert!(ready(panel).float.is_none(), "committed by the append");
            assert_eq!(
                ready(panel).store.state().indices[idx(1, 4, 0)],
                1,
                "the move landed before the sheet grew"
            );
        });
    }

    /// A press outside the panel ends the gesture, so it must put a float
    /// down. Clearing the marquee and leaving the float alive strands it:
    /// the art shows moved, nothing says it is uncommitted, and escape
    /// would silently take it back.
    #[gpui::test]
    async fn test_pressing_outside_the_panel_commits_a_float(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 0)));
            }
            panel.move_selection((5, 0), cx);
            assert!(ready(panel).float.is_some());

            panel.clear_selection_on_click_out(cx);

            assert!(ready(panel).float.is_none(), "the float was put down");
            let state = ready(panel).store.state();
            assert_eq!(state.indices[idx(1, 5, 0)], 1, "it landed");
            assert_eq!(state.indices[idx(1, 0, 0)], 0, "and vacated its origin");
            assert!(ready(panel).selection.is_none(), "marquee still cleared");
        });
    }

    /// A float has to be legible as a float: the sheet already shows the
    /// moved art, so without this nothing distinguishes it from a committed
    /// move -- and escape means something different in each state.
    #[gpui::test]
    async fn test_the_status_line_says_when_a_selection_is_floating(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        place_sheet_at_origin(&panel, cx);
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 0)));
            }
            assert_eq!(panel.float_status(), None, "a marquee alone is not a float");

            panel.move_selection((3, 0), cx);
            assert!(
                panel.float_status().is_some_and(|s| s.contains("floating")),
                "the state is said out loud while lifted"
            );

            panel.commit_float(cx);
            assert_eq!(panel.float_status(), None, "and goes quiet once placed");
        });
    }

    /// The clipboard lived on `OpenTileset`, and `tileset_item` builds a
    /// fresh panel entity per tab, so copying in one tileset and pasting
    /// into another silently did nothing at all.
    #[gpui::test]
    async fn test_the_clipboard_is_shared_between_open_tilesets(cx: &mut TestAppContext) {
        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let source = ready_panel(cx, source_dir.path()).await;
        let dest = ready_panel(cx, dest_dir.path()).await;

        source.update(cx, |panel, cx| {
            // Tile 1's top-left 2x2 is solid slot 1 in the fixture.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((TILE_PX, 0), (TILE_PX + 1, 1)));
            }
            panel.copy_selection(cx);
        });

        dest.update(cx, |panel, cx| {
            // Tile 0 is transparent, so a paste there is unambiguous.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (1, 1)));
            }
            panel.paste_clipboard(cx);
            assert_eq!(
                ready(panel).store.state().indices[idx(0, 0, 0)],
                1,
                "the other tileset's copy landed here"
            );
        });
    }

    /// Pixels are palette INDICES, so the same numbers under a different
    /// palette are different colours. Say so rather than silently
    /// recolouring what was pasted.
    #[gpui::test]
    async fn test_pasting_across_palettes_says_the_colours_will_differ(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selection = Some(((0, 0), (1, 1)));
            }

            // Same palette: no note.
            panel.copy_selection(cx);
            panel.paste_clipboard(cx);
            assert_eq!(ready(panel).clipboard_note, None, "palettes match");

            // A clip written against a different palette.
            let mut other = [0u16; PAL_SLOTS];
            other[1] = 0x07E0; // green, where the fixture is red
            TilesetPanel::set_clip(1, 1, vec![1], other, cx);
            panel.paste_clipboard(cx);
            assert!(
                ready(panel)
                    .clipboard_note
                    .is_some_and(|note| note.contains("different palette")),
                "the mismatch is surfaced"
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
            panel.on_sheet_mouse_down(pixel_pos(ready(panel), 1, 2, 2), false, cx);
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
