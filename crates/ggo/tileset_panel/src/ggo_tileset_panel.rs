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
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, RenderImage,
    ScrollHandle, Styled, Task, WeakEntity, Window, actions, div, img, point, px,
};
use project::ProjectPath;
use ui::prelude::*;
use workspace::Workspace;

use ggo_worldlib::sprites::io::save_tileset;
use ggo_worldlib::sprites::palette565::PAL_SLOTS;
use ggo_worldlib::sprites::tileset_doc::{TILE_PX, TilesetDocStore};

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
        ScrollDown
    ]
);

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

/// The tileset extension this editor claims from the file explorer.
const TILESET_EXT: &str = "til";

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as every other GGO panel's `init`: `zed::reload_keymaps`
    // clears and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), so re-running `bind_panel_keys` on
    // `KeymapEventChannel` keeps the editor's bindings alive across
    // reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    // Explorer-driven routing: clicking a `.til` in the project panel opens
    // the tileset editor tab instead of a (binary, unreadable) text buffer.
    workspace::register_path_open_interceptor(cx, intercept_tileset_open);
}

fn bind_panel_keys(cx: &mut App) {
    // Ctrl-z/ctrl-s need no `not_editing` guard: the editor hosts no
    // field editors, so nothing deeper ever shadows these.
    cx.bind_keys([
        KeyBinding::new("ctrl-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-s", Save, Some(KEY_CONTEXT)),
        KeyBinding::new("left", ScrollLeft, Some(KEY_CONTEXT)),
        KeyBinding::new("right", ScrollRight, Some(KEY_CONTEXT)),
        KeyBinding::new("up", ScrollUp, Some(KEY_CONTEXT)),
        KeyBinding::new("down", ScrollDown, Some(KEY_CONTEXT)),
    ]);
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

/// The two painting tools. The eraser is not its own op: it paints
/// palette index 0, the transparent slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Pencil,
    Eraser,
}

/// The open tileset: worldlib's doc store plus the view state that must
/// survive a re-click on the already-open file.
struct OpenTileset {
    rel_path: String,
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
}

impl OpenTileset {
    fn new(rel_path: String, loaded: LoadedTileset) -> Self {
        Self {
            rel_path,
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
        }
    }

    /// The sheet's on-screen size at the current zoom.
    fn zoomed_size(&self) -> (f32, f32) {
        let (w, h) = self.grid_size;
        let z = self.zoom as f32;
        (w as f32 * z, h as f32 * z)
    }

    /// The color the current tool paints with.
    fn paint_color(&self) -> u8 {
        match self.tool {
            Tool::Pencil => self.slot as u8,
            Tool::Eraser => 0,
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
            cx.background_spawn(async move { loader::load_tileset(&root, &rel) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => ViewerState::Ready(Box::new(OpenTileset::new(rel, loaded))),
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
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let next =
            (open.zoom as isize + delta).clamp(MIN_ZOOM as isize, MAX_ZOOM as isize) as usize;
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
    fn scroll_by(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
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
        let meta = loader::ViewMeta {
            zoom: Some(open.zoom),
            cols: Some(open.cols),
        };
        if let Err(e) = loader::save_view_meta(root, &open.rel_path, &meta) {
            log::error!("GGO: failed to write view sidecar for {}: {e}", open.rel_path);
        }
    }

    // ------------------------------------------------------------ editing

    /// Map a window-absolute mouse position to the sheet pixel under it:
    /// `(tile, x, y)` in doc coordinates. `None` outside the sheet (the
    /// composed grid floors at one row, so its trailing pad cells past
    /// `tile_count` must be rejected here, not painted).
    fn pixel_at(&self, pos: Point<Pixels>) -> Option<(usize, usize, usize)> {
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
        let sx = lx as usize;
        let sy = ly as usize;
        let tile = (sy / TILE_PX) * open.cols + sx / TILE_PX;
        if tile >= open.store.state().tile_count {
            return None;
        }
        Some((tile, sx % TILE_PX, sy % TILE_PX))
    }

    /// Paint the pixel under `pos` with the current tool, folding into
    /// the open stroke -- one whole drag is one undo step. Same-color
    /// paints are no-ops inside the store, so drag-painting over
    /// already-painted ground is free.
    fn paint_at(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((tile, x, y)) = self.pixel_at(pos) else {
            return;
        };
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let color = open.paint_color();
        open.store.apply_stroke_paint(tile, x, y, color);
        self.recompose_grid(cx);
    }

    /// Rebuild the composed sheet image from the store's current state --
    /// after every op, undo, and redo.
    fn recompose_grid(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let state = open.store.state();
        if let Some(grid) =
            loader::compose_grid(&state.indices, state.tile_count, open.cols, &state.palette)
        {
            open.grid = grid;
        }
        cx.notify();
    }

    fn on_sheet_mouse_down(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.store.begin_stroke();
        }
        self.paint_at(pos, cx);
        if let ViewerState::Ready(open) = &mut self.state {
            open.painting = true;
        }
    }

    fn on_sheet_mouse_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let painting = matches!(&self.state, ViewerState::Ready(open) if open.painting);
        if painting {
            self.paint_at(pos, cx);
        }
    }

    fn on_sheet_mouse_up(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.painting
        {
            open.store.end_stroke();
            open.painting = false;
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
        let line_color = cx.theme().colors().border_variant;
        // `.top_0().left_0()` matters: an absolute child with auto insets
        // sits at its STATIC position -- after the in-flow img sibling --
        // so the recorded bounds would be shifted by exactly the image
        // (the `ggo_sprite_panel` preview-overlay lesson).
        let overlay = gpui::canvas(
            move |bounds, _window, _cx| {
                *bounds_cell.borrow_mut() = Some(bounds);
            },
            move |bounds, (), window, _cx| {
                paint_tile_borders(bounds, cols, rows, line_color, window);
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
                    div()
                        .relative()
                        .w(px(w))
                        .h(px(h))
                        .child(img(open.grid.clone()).nearest(true).w(px(w)).h(px(h)))
                        .child(overlay)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                // Take focus so undo/save bindings apply.
                                window.focus(&this.focus_handle, cx);
                                this.on_sheet_mouse_down(event.position, cx);
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                            this.on_sheet_mouse_move(event.position, cx);
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                this.on_sheet_mouse_up(cx);
                            }),
                        ),
                ),
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
        let summary = format!("{} tiles · {}x{} px · {} cols", state.tile_count, w, h, open.cols);
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
            .child(div().w_2())
            .child(
                IconButton::new("ggo-tileset-zoom-out", IconName::Dash)
                    .icon_size(IconSize::XSmall)
                    .disabled(open.zoom <= MIN_ZOOM)
                    .tooltip(ui::Tooltip::text("Zoom out"))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_by(-1, cx))),
            )
            .child(Label::new(zoom_label).size(LabelSize::XSmall))
            .child(
                IconButton::new("ggo-tileset-zoom-in", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .disabled(open.zoom >= MAX_ZOOM)
                    .tooltip(ui::Tooltip::text("Zoom in"))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_by(1, cx))),
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
                this.child(
                    div().p_1().child(
                        Label::new(format!("Save failed: {e}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_ready(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .child(self.render_toolbar(cx))
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
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
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
            .on_action(cx.listener(|this, _: &ScrollLeft, _window, cx| this.scroll_by(-1.0, 0.0, cx)))
            .on_action(cx.listener(|this, _: &ScrollRight, _window, cx| this.scroll_by(1.0, 0.0, cx)))
            .on_action(cx.listener(|this, _: &ScrollUp, _window, cx| this.scroll_by(0.0, -1.0, cx)))
            .on_action(cx.listener(|this, _: &ScrollDown, _window, cx| this.scroll_by(0.0, 1.0, cx)))
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

/// Paint 1px lines along every tile boundary of the zoomed sheet -- the
/// editing-canvas orientation aid (`ggo_sprite_panel::paint_tile_grid`'s
/// idiom, over the tileset's cols x rows).
fn paint_tile_borders(
    bounds: Bounds<Pixels>,
    cols: usize,
    rows: usize,
    color: gpui::Hsla,
    window: &mut Window,
) {
    if cols == 0 || rows == 0 {
        return;
    }
    let line = px(1.);
    for i in 0..=cols {
        let x = (bounds.origin.x + bounds.size.width * (i as f32 / cols as f32))
            .min(bounds.origin.x + bounds.size.width - line);
        window.paint_quad(gpui::fill(
            Bounds::new(
                gpui::point(x, bounds.origin.y),
                gpui::size(line, bounds.size.height),
            ),
            color,
        ));
    }
    for i in 0..=rows {
        let y = (bounds.origin.y + bounds.size.height * (i as f32 / rows as f32))
            .min(bounds.origin.y + bounds.size.height - line);
        window.paint_quad(gpui::fill(
            Bounds::new(
                gpui::point(bounds.origin.x, y),
                gpui::size(bounds.size.width, line),
            ),
            color,
        ));
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
            panel.on_sheet_mouse_up(cx);

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
            panel.on_sheet_mouse_up(cx);
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
            panel.on_sheet_mouse_up(cx);
            panel.on_sheet_mouse_down(drag, cx);
            panel.on_sheet_mouse_up(cx);
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
            panel.on_sheet_mouse_up(cx);
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
            panel.on_sheet_mouse_up(cx);
            let state = ready(panel).store.state();
            assert!(!state.dirty, "off-sheet clicks must not paint");
        });
    }


    /// Palette editing: a `SetPalette` op recolors the sheet (undoably),
    /// slot 0 is locked, and out-of-range slots are rejected instead of
    /// panicking (the store indexes the palette raw).
    #[gpui::test]
    async fn test_palette_edits_recolor_the_sheet_and_slot_zero_is_locked(
        cx: &mut TestAppContext,
    ) {
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
            assert_eq!(
                ready(panel).store.state().palette[0],
                0,
                "slot 0 is locked"
            );
            panel.set_palette_slot(PAL_SLOTS + 3, 0xFFFF, cx);
            assert!(!ready(panel).store.state().dirty, "no stray dirt from no-ops");
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
            panel.on_sheet_mouse_up(cx);
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
            panel.on_sheet_mouse_up(cx);
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
            assert_eq!(open.zoom, DEFAULT_ZOOM + 3, "zoom came back from the sidecar");
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
}
