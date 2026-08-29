//! The map-editing SESSION: a `.map` being edited, minus every trace of the
//! surface editing it.
//!
//! Everything here used to live on `ggo_map_panel`'s `OpenMap`/`MapPanel`
//! pair, where "the document plus its tileset cache plus the tool state
//! machine" and "the canvas view state" were one struct. They are separated
//! because map editing is moving INTO `ggo_world_panel` (spec
//! 2026-08-29, world-hosted map editing): the world editor hosts a session
//! of its own, so the half that is not gpui has to be a library surface
//! rather than panel internals.
//!
//! The split line is "would the standalone panel's deletion take it with
//! it?". The store, the resolved tileset, the strip, the stamp state
//! (flip/palSub/palette selection), the tool, the pending drags, the
//! terrains and the save target all survive that deletion and live here.
//! Zoom, pan, canvas bounds, the composed canvas image, the resize editors
//! and the terrain-name editor are the panel's own and stay there.
//!
//! Nothing here touches gpui state: a session is constructed, mutated and
//! saved from plain code, which is why its tests need no `TestAppContext`.
//! The one gpui type it does hand back is `RenderImage`, and only as the
//! output of a compose the host caches.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::RenderImage;
use ui::IconName;

use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::map_doc::{
    CELL_BLANK, MapDocStore, MapOp, Stamp, build_stamp, palette_sel_rect, unpack_cell,
};
use ggo_worldlib::sprites::terrain::{self, Terrain};
use ggo_worldlib::sprites::tileset_meta::{TilesetMeta, load_tileset_meta, save_tileset_meta};

use crate::geom;
use crate::loader;

/// ggo-ide's `pages::assets::map::MapTool`, ported verbatim (order matches
/// its own tool rail). Toolbar-button-only, same as there -- ggo-ide's map
/// editor has no letter hotkeys either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MapTool {
    #[default]
    Brush,
    RectFill,
    /// Flood-fill the clicked region with the stamp's first cell.
    Fill,
    /// Drag a cell rectangle for copy / paste / delete.
    Select,
    /// Paint the selected autotile terrain (shift-drag erases).
    Terrain,
    Eyedropper,
    Eraser,
}

impl MapTool {
    pub const ALL: [MapTool; 7] = [
        MapTool::Brush,
        MapTool::RectFill,
        MapTool::Fill,
        MapTool::Select,
        MapTool::Terrain,
        MapTool::Eyedropper,
        MapTool::Eraser,
    ];

    pub fn icon(self) -> IconName {
        match self {
            MapTool::Brush => IconName::Pencil,
            MapTool::RectFill => IconName::SelectAll,
            MapTool::Fill => IconName::Sparkle,
            MapTool::Select => IconName::Maximize,
            MapTool::Terrain => IconName::Blocks,
            MapTool::Eyedropper => IconName::Crosshair,
            MapTool::Eraser => IconName::Eraser,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MapTool::Brush => "Brush",
            MapTool::RectFill => "Rect fill",
            MapTool::Fill => "Flood fill",
            MapTool::Select => "Select cells",
            MapTool::Terrain => "Terrain (shift-drag erases)",
            MapTool::Eyedropper => "Eyedropper",
            MapTool::Eraser => "Eraser",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            MapTool::Brush => "ggo-map-tool-brush",
            MapTool::RectFill => "ggo-map-tool-rect",
            MapTool::Fill => "ggo-map-tool-fill",
            MapTool::Select => "ggo-map-tool-select",
            MapTool::Terrain => "ggo-map-tool-terrain",
            MapTool::Eyedropper => "ggo-map-tool-eyedropper",
            MapTool::Eraser => "ggo-map-tool-eraser",
        }
    }
}

/// Raw drag corners → inclusive `(x0, y0, x1, y1)` with `x0 <= x1`,
/// `y0 <= y1`.
pub(crate) fn normalize_rect((x0, y0, x1, y1): (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

/// One `.map` under edit: the document store, the display cache for the
/// tileset it is bound to, and the tool state a gesture reads and writes.
pub struct PaintSession {
    /// The map's path relative to [`Self::root`] -- the frame
    /// `io::save_map` writes in, and the frame the `til_path` inside it
    /// resolves in.
    pub rel_path: String,
    /// The ASSET ROOT this map was LOADED from, captured at open time so a
    /// save can't land somewhere else if the worktree is repointed
    /// meanwhile (`ggo_world_panel`'s `OpenWorld::root` idiom).
    pub root: PathBuf,
    pub store: MapDocStore,
    pub tileset: Option<loader::Tileset>,
    pub tileset_error: Option<String>,
    pub strip: Option<Arc<RenderImage>>,
    pub tool: MapTool,
    pub hflip: bool,
    pub vflip: bool,
    pub pal_sub: u16,
    /// Strip drag-select anchor/far corner (col/row) -- ggo-ide's
    /// `palAnchor`/`palFar`. `palette_sel_rect` normalizes them into a
    /// rect; `build_stamp` turns that rect into the active stamp.
    pub pal_anchor: (i32, i32),
    pub pal_far: (i32, i32),
    /// A strip drag-select is in flight -- ggo-ide's `palDragging`. Lives
    /// with the anchor/far corners it gates rather than on a host, so the
    /// strip widget is one shareable piece (spec 2026-08-29).
    pub pal_dragging: bool,
    /// Rect-fill drag preview, raw (unnormalized) corners -- ggo-ide's
    /// `rectPending`.
    pub rect_pending: Option<(i32, i32, i32, i32)>,
    /// Select-tool drag, raw corners; settles into `selection` on release.
    pub sel_pending: Option<(i32, i32, i32, i32)>,
    /// The cell selection, normalized inclusive corners.
    pub selection: Option<(i32, i32, i32, i32)>,
    /// Shift was held at the gesture's start: the terrain tool erases.
    pub paint_erase: bool,
    /// The bound tileset's autotile terrains (from its editor sidecar).
    pub terrains: Vec<Terrain>,
    /// Index into `terrains` the Terrain tool paints with.
    pub terrain: Option<usize>,
    /// The sidecar key for saving terrains; `None` outside the worktree.
    pub til_meta_rel: Option<String>,
    /// The 8-neighbour mask the terrain editor assigns next.
    pub mask_draft: u8,
    pub terrain_error: Option<String>,
    pub save_error: Option<String>,
}

impl PaintSession {
    pub fn new(rel_path: String, root: PathBuf, loaded: loader::LoadedMap) -> Self {
        let data = loaded.data;
        PaintSession {
            rel_path,
            root,
            store: MapDocStore::new(data.til_path, data.pal_path, data.w, data.h, data.cells),
            tileset: loaded.tileset,
            tileset_error: loaded.tileset_error,
            strip: loaded.strip,
            tool: MapTool::default(),
            hflip: false,
            vflip: false,
            pal_sub: geom::PAL_SUB_MIN,
            pal_anchor: (0, 0),
            pal_far: (0, 0),
            pal_dragging: false,
            rect_pending: None,
            sel_pending: None,
            selection: None,
            paint_erase: false,
            terrains: loaded.tileset_meta.terrains,
            terrain: None,
            til_meta_rel: loaded.til_meta_rel,
            mask_draft: 0,
            terrain_error: None,
            save_error: None,
        }
    }

    /// Open the asset-root-relative `.map` at `rel` under `root`. Does
    /// disk IO, so a gpui host runs it off the UI thread; the composed
    /// canvas image [`loader::load_map`] also returns is dropped here --
    /// a host caches whichever surface it draws.
    pub fn load(root: &Path, rel: &str, project_root: &Path) -> Result<Self, String> {
        let loaded = loader::load_map(root, rel, project_root)?;
        Ok(Self::new(rel.to_string(), root.to_path_buf(), loaded))
    }

    /// The brush's current stamp -- ggo-ide's `current_stamp`, i.e.
    /// worldlib's `palette_sel_rect` + `build_stamp` over the strip
    /// selection, with the live flip/palSub folded in. A single-cell
    /// selection yields a 1x1 stamp, so "brush" and "multi-tile stamp" are
    /// the same code path (as they are in worldlib).
    pub fn current_stamp(&self) -> Stamp {
        let (cols, tile_count) = self
            .tileset
            .as_ref()
            .map_or((1, 0), |ts| (ts.cols.max(1), ts.tile_count));
        build_stamp(
            palette_sel_rect(self.pal_anchor, self.pal_far),
            cols,
            tile_count,
            self.pal_sub,
            self.hflip,
            self.vflip,
        )
    }

    /// The single cell a rect-fill paints: the stamp's first cell, matching
    /// ggo-ide (`RectFill` takes one `cell`, not a stamp).
    pub fn fill_cell(&self) -> u16 {
        self.current_stamp()
            .cells
            .first()
            .copied()
            .unwrap_or(CELL_BLANK)
    }

    /// Whether the document has unsaved edits.
    pub fn dirty(&self) -> bool {
        self.store.dirty()
    }

    pub fn apply(&mut self, op: MapOp) {
        self.store.apply(op);
    }

    /// Whether a gesture could do ANYTHING right now -- the guard
    /// [`Self::paint_at`] applies before it looks at the cell, exposed so a
    /// host can skip the repaint an inert gesture would otherwise cost
    /// (`false` here means `paint_at` is a guaranteed no-op, on the
    /// document AND on session state).
    ///
    /// Two ways a gesture is inert. Without a bound tileset there is no
    /// tile pool for a cell index to mean anything, so painting must not
    /// write indices into a map nothing can resolve -- ggo-ide's
    /// `on_map_surface_event` opens with the same `tileset_data.is_none()`
    /// early return. And the Terrain tool paints a SELECTED terrain, so
    /// with none selected there is nothing to resolve neighbours against.
    pub fn can_paint(&self) -> bool {
        if self.tileset.is_none() {
            return false;
        }
        self.tool != MapTool::Terrain
            || self
                .terrain
                .is_some_and(|index| index < self.terrains.len())
    }

    /// Switch the active tool. Always discards the pending rect-fill and
    /// select previews: a half-dragged rectangle must not survive into
    /// another tool's gesture, nor arm the next drag with a stale anchor.
    /// The SETTLED [`Self::selection`] deliberately survives -- Copy,
    /// Paste and Delete act on it from any tool.
    pub fn set_tool(&mut self, tool: MapTool) {
        self.tool = tool;
        self.rect_pending = None;
        self.sel_pending = None;
    }

    /// Canvas primary-down: open a gesture whose every application folds
    /// into ONE undo entry, reading `erase` (shift) once so a terrain drag
    /// keeps erasing for its whole length.
    ///
    /// A new gesture starts from no pending rect and no pending selection:
    /// [`Self::paint_at`]'s RectFill and Select arms EXTEND a pending
    /// rectangle, so one whose release never arrived -- the button came up
    /// off-canvas, or the window lost focus mid-drag -- would otherwise
    /// turn the next single click into a drag from the abandoned anchor.
    pub fn begin_gesture(&mut self, erase: bool) {
        self.paint_erase = erase;
        self.rect_pending = None;
        self.sel_pending = None;
        self.store.begin_stroke();
    }

    /// Canvas primary-up: close the undo entry, settle a select drag into
    /// [`Self::selection`], and commit a pending rect-fill. Returns whether
    /// the DOCUMENT changed (i.e. whether the host has to recompose).
    pub fn end_gesture(&mut self) -> bool {
        self.store.end_stroke();
        self.commit_selection();
        self.commit_rect()
    }

    /// One tool application at `cell` -- the canvas's primary-down /
    /// drag-move body, shared by both events (ggo-ide re-fires the same
    /// tool action on every `Moved` while painting).
    ///
    /// Returns true when the DOCUMENT changed, i.e. when the host has to
    /// recompose; Select, Eyedropper and a growing rect-fill mutate only
    /// session state and return false, as does an inert gesture
    /// ([`Self::can_paint`]).
    pub fn paint_at(&mut self, cell: (i32, i32)) -> bool {
        let (x, y) = cell;
        if !self.can_paint() {
            return false;
        }
        match self.tool {
            MapTool::Brush => {
                let stamp = self.current_stamp();
                self.apply(MapOp::Brush { x, y, stamp });
                true
            }
            MapTool::Eraser => {
                self.apply(MapOp::Erase { x, y });
                true
            }
            MapTool::Fill => {
                let cell = self.fill_cell();
                self.apply(MapOp::Fill { x, y, cell });
                true
            }
            MapTool::Select => {
                match &mut self.sel_pending {
                    Some(rect) => {
                        rect.2 = x;
                        rect.3 = y;
                    }
                    None => self.sel_pending = Some((x, y, x, y)),
                }
                false
            }
            MapTool::Terrain => {
                // `can_paint` already established there is one; this is the
                // borrow, not a second gate.
                let Some(terrain) = self.terrain.and_then(|i| self.terrains.get(i)) else {
                    return false;
                };
                let state = self.store.state();
                let writes = terrain::resolve(
                    &state.cells,
                    state.w,
                    state.h,
                    &[(x, y)],
                    terrain,
                    !self.paint_erase,
                );
                self.apply(MapOp::SetCells(writes));
                true
            }
            MapTool::Eyedropper => {
                self.eyedrop(x, y);
                false
            }
            MapTool::RectFill => {
                match &mut self.rect_pending {
                    Some(rect) => {
                        rect.2 = x;
                        rect.3 = y;
                    }
                    None => self.rect_pending = Some((x, y, x, y)),
                }
                false
            }
        }
    }

    /// Gesture release for the rect-fill tool: the pending preview becomes
    /// ONE `MapOp::RectFill`. Returns true when the document changed.
    ///
    /// Deliberately NOT symmetric with [`Self::commit_selection`], which is
    /// why they are two calls rather than one `end_gesture`: this clears
    /// `rect_pending` whatever the tool is, because a tool switch mid-drag
    /// must not leave a stale rectangle drawn on the canvas, while
    /// `commit_selection` settles only under the Select tool, because a
    /// settled `selection` is what Copy/Paste/Delete act on and must
    /// survive the switch. Both halves are ggo-ide's, ported.
    pub fn commit_rect(&mut self) -> bool {
        let pending = self.rect_pending.take();
        let Some((x0, y0, x1, y1)) = (self.tool == MapTool::RectFill)
            .then_some(pending)
            .flatten()
        else {
            return false;
        };
        let cell = self.fill_cell();
        self.apply(MapOp::RectFill {
            x0,
            y0,
            x1,
            y1,
            cell,
        });
        true
    }

    /// Gesture release for the select tool: the raw drag corners settle
    /// into the normalized [`Self::selection`].
    pub fn commit_selection(&mut self) {
        if self.tool == MapTool::Select
            && let Some(rect) = self.sel_pending.take()
        {
            self.selection = Some(normalize_rect(rect));
        }
    }

    /// Drop the cell selection, settled and pending both. Returns whether
    /// there was one to drop -- the branch Escape takes before it gives up
    /// the mode.
    pub fn clear_selection(&mut self) -> bool {
        let had = self.selection.is_some() || self.sel_pending.is_some();
        self.selection = None;
        self.sel_pending = None;
        had
    }

    /// The selected cells as a stamp, clipped to the map -- what Copy puts
    /// on the host's cell clipboard.
    pub fn selection_stamp(&self) -> Option<Stamp> {
        let (x0, y0, x1, y1) = self.selection?;
        let state = self.store.state();
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

    /// Stamp `stamp` with its top-left at `(x, y)` and leave the pasted
    /// block selected. Returns whether the document changed.
    ///
    /// Ends any open stroke first, in both this and
    /// [`Self::delete_selection`]: a paste or delete is its own undo step,
    /// never an amendment of a drag whose release landed off-canvas and so
    /// left the stroke open.
    pub fn paste_stamp(&mut self, stamp: Stamp, (x, y): (i32, i32)) -> bool {
        if self.tileset.is_none() {
            return false;
        }
        let (w, h) = (stamp.w as i32, stamp.h as i32);
        self.store.end_stroke();
        self.apply(MapOp::Brush { x, y, stamp });
        self.selection = Some((x, y, x + w - 1, y + h - 1));
        true
    }

    /// Blank the selected cells as ONE `RectFill`, so one undo puts all of
    /// them back. Returns whether the document changed.
    pub fn delete_selection(&mut self) -> bool {
        let Some((x0, y0, x1, y1)) = self.selection.filter(|_| self.tileset.is_some()) else {
            return false;
        };
        self.store.end_stroke();
        self.apply(MapOp::RectFill {
            x0,
            y0,
            x1,
            y1,
            cell: CELL_BLANK,
        });
        true
    }

    /// Resize the document, clamping both dimensions to the editor's
    /// limits ([`geom::clamp_dim`]) -- typing `9999` gives a 256-wide map
    /// rather than a refusal.
    pub fn resize(&mut self, w: u16, h: u16) {
        self.apply(MapOp::Resize {
            w: geom::clamp_dim(w as i64),
            h: geom::clamp_dim(h as i64),
        });
    }

    /// Strip primary-down on tile `cell`: arm a drag-select anchored
    /// there. A miss -- off the strip, or in the sheet's zero-filled
    /// partial-row padding -- arms nothing. Returns whether anything moved.
    pub fn strip_press(&mut self, cell: Option<(i32, i32)>) -> bool {
        let Some(cell) = cell else { return false };
        self.pal_dragging = true;
        self.pal_anchor = cell;
        self.pal_far = cell;
        true
    }

    /// Extend an in-flight strip drag-select to `cell`. `held` is whether
    /// the left button is still down: a move without it means the release
    /// happened outside the strip, which ends the drag. A miss while
    /// dragging leaves the selection where it was rather than smearing it
    /// to the sheet's edge. Returns whether anything moved.
    pub fn strip_move(&mut self, cell: Option<(i32, i32)>, held: bool) -> bool {
        if !self.pal_dragging {
            return false;
        }
        if !held {
            self.pal_dragging = false;
            return false;
        }
        match cell {
            Some(cell) => {
                self.pal_far = cell;
                true
            }
            None => false,
        }
    }

    /// Release over the strip: the drag-select is finished, keeping its
    /// selection.
    pub fn strip_release(&mut self) {
        self.pal_dragging = false;
    }

    /// One eyedropper pick at cell `(x, y)` -- ggo-ide's `map_eyedrop`:
    /// adopts the picked cell's hflip/vflip/palSub, and moves the strip
    /// selection to its source tile ONLY when that tile is still in range
    /// for the bound tileset (an out-of-range or blank cell leaves the
    /// selection where it was).
    pub fn eyedrop(&mut self, x: i32, y: i32) {
        let state = self.store.state();
        if x < 0 || x >= state.w as i32 || y < 0 || y >= state.h as i32 {
            return;
        }
        let fields = unpack_cell(state.cells[y as usize * state.w as usize + x as usize]);
        self.hflip = fields.hflip;
        self.vflip = fields.vflip;
        self.pal_sub = fields.pal_sub;
        let tile_count = self.tileset.as_ref().map_or(0, |ts| ts.tile_count);
        if (fields.tile as usize) < tile_count {
            let cols = self.tileset.as_ref().map_or(1, |ts| ts.cols);
            let anchor = geom::tile_cell(fields.tile, cols);
            self.pal_anchor = anchor;
            self.pal_far = anchor;
        }
    }

    /// Undo or redo, then put the CACHED tileset back in step with
    /// whatever the store now says is bound. Returns whether the step
    /// happened.
    ///
    /// The resync is the whole point of routing both through one function
    /// (fix round 1, BLOCKING 1). `MapOp::BindTileset` is an undoable op
    /// like any other, but [`Self::tileset`]/[`Self::strip`] are a display
    /// cache OUTSIDE the store -- so an undo across a bind used to leave
    /// the session drawing, stamp-indexing and gating against a tileset
    /// the document is no longer bound to. Two ways that bit:
    ///
    /// - bind, then undo: the store's `til_path` goes back to `""` while
    ///   the cache stays populated, so [`Self::paint_at`]'s
    ///   `tileset.is_none()` gate PASSES and you can paint an unbound map
    ///   and save a file full of tile indices with nothing to resolve them
    ///   against -- exactly the artifact a fresh map's unbound-by-design
    ///   rationale exists to prevent;
    /// - rebind A -> B, then undo: the canvas, the strip AND
    ///   `build_stamp`'s `row * cols + col` are all computed against B's
    ///   tile count and column layout while the document is bound to A.
    ///
    /// (ggo-ide has the same gap. Inherited, not a regression -- but it
    /// defeats an invariant this crate states in its own module doc, so it
    /// is fixed here rather than ported.)
    pub fn step_history(
        &mut self,
        step: fn(&mut MapDocStore) -> bool,
        project_root: Option<&Path>,
    ) -> bool {
        let before = self.store.state().til_path;
        if !step(&mut self.store) {
            return false;
        }
        let after = self.store.state().til_path;
        if before != after {
            let (resolved, meta) = self.resolve_tileset(project_root, &after);
            self.set_tileset(resolved);
            self.adopt_tileset_meta(meta);
        }
        true
    }

    /// Bind (or rebind) the tileset at asset-root-relative `til_rel`.
    ///
    /// Resolves the tileset FIRST and applies `MapOp::BindTileset` only on
    /// success -- ggo-ide's `onBindTileset` never applies the op on a
    /// failed open, ported verbatim, and for good reason: a binding the
    /// editor can't resolve would leave the document dirty and pointing at
    /// something it cannot draw. The `.pal` rel comes from worldlib's own
    /// `open_tileset` result rather than being re-derived here, so the
    /// `.til`/`.pal` pairing rule stays single-sourced in worldlib.
    ///
    /// Synchronous, same call as [`Self::save`]: one tileset is a few tens
    /// of KB and the user is waiting on the result of their own click.
    ///
    /// Returns true when the binding LANDED, i.e. when the host has to
    /// recompose. A failure leaves the document (and therefore the composed
    /// pixels) exactly as they were and only records
    /// [`Self::tileset_error`], so a host that recomposed anyway would pay
    /// a full map compose to redraw an identical image.
    pub fn bind_tileset(&mut self, til_rel: String, project_root: Option<&Path>) -> bool {
        // Resolve FIRST: a binding the editor can't open must not reach
        // the document (see this fn's doc). The resolved tileset then goes
        // straight into the cache via `set_tileset`, so bind and
        // undo-across-a-bind install it exactly the same way.
        let (resolved, meta) = self.resolve_tileset(project_root, &til_rel);
        match resolved {
            Ok(tileset) => {
                let pal_path = tileset.pal_path.clone();
                self.apply(MapOp::BindTileset {
                    til_path: til_rel,
                    pal_path,
                });
                self.set_tileset(Ok(tileset));
                self.adopt_tileset_meta(meta);
                true
            }
            Err(e) => {
                self.tileset_error = Some(e);
                false
            }
        }
    }

    /// Install an already-resolved tileset as the display cache -- shared
    /// by [`Self::bind_tileset`] and [`Self::step_history`], so "what the
    /// session holds for the bound tileset" has ONE definition. An `Err`
    /// (an empty binding, or a `.til` that won't open) clears the cache to
    /// `None` rather than leaving the previous tileset in place; that
    /// clearing is what re-arms [`Self::paint_at`]'s unbound gate. The
    /// stamp selection resets either way: a `(col, row)` means a different
    /// tile -- or no tile -- under a different sheet.
    ///
    /// Takes the `Result` rather than the path so the caller keeps the one
    /// disk read it already did ([`Self::bind_tileset`] has to resolve
    /// first to decide whether to apply the op at all).
    fn set_tileset(&mut self, resolved: Result<loader::Tileset, String>) {
        match resolved {
            Ok(tileset) => {
                self.strip = loader::compose_strip(&tileset);
                self.tileset = Some(tileset);
                self.tileset_error = None;
            }
            Err(e) => {
                self.tileset = None;
                self.strip = None;
                self.tileset_error = Some(e);
            }
        }
        self.pal_anchor = (0, 0);
        self.pal_far = (0, 0);
    }

    /// Load `til_rel` laid out at the sidecar's `cols`, returning the
    /// sidecar too (its terrains follow the binding).
    fn resolve_tileset(
        &self,
        project_root: Option<&Path>,
        til_rel: &str,
    ) -> (
        Result<loader::Tileset, String>,
        (TilesetMeta, Option<String>),
    ) {
        let (meta, meta_rel) = match project_root {
            Some(project_root) => (
                loader::tileset_meta(&self.root, project_root, til_rel),
                loader::tileset_meta_rel(&self.root, project_root, til_rel),
            ),
            None => (Default::default(), None),
        };
        let resolved = loader::load_tileset(&self.root, til_rel, meta.cols);
        (resolved, (meta, meta_rel))
    }

    fn adopt_tileset_meta(&mut self, (meta, meta_rel): (TilesetMeta, Option<String>)) {
        self.terrains = meta.terrains;
        self.til_meta_rel = meta_rel;
        self.terrain = None;
        self.terrain_error = None;
    }

    // ------------------------------------------------------------ terrains

    /// The tile the terrain editor assigns: the stamp's first cell's tile,
    /// or `None` when there is no tileset or the stamp is blank.
    pub fn anchor_tile(&self) -> Option<u16> {
        let cell = self.fill_cell();
        (self.tileset.is_some() && cell != CELL_BLANK).then(|| unpack_cell(cell).tile)
    }

    /// Add a terrain and select it. An empty name is refused (the sidecar
    /// keys terrains by name).
    pub fn add_terrain(&mut self, name: String, project_root: Option<&Path>) {
        if name.is_empty() || self.tileset.is_none() {
            return;
        }
        self.terrains.push(Terrain {
            name,
            tiles: vec![],
        });
        self.terrain = Some(self.terrains.len() - 1);
        self.save_terrains(project_root);
    }

    pub fn rename_terrain(&mut self, name: String, project_root: Option<&Path>) {
        if name.is_empty() {
            return;
        }
        let Some(terrain) = self.terrain.and_then(|i| self.terrains.get_mut(i)) else {
            return;
        };
        terrain.name = name;
        self.save_terrains(project_root);
    }

    pub fn remove_terrain(&mut self, project_root: Option<&Path>) {
        let Some(index) = self.terrain.take().filter(|i| *i < self.terrains.len()) else {
            return;
        };
        self.terrains.remove(index);
        self.save_terrains(project_root);
    }

    /// Select terrain `index`, or deselect when it is out of range.
    pub fn select_terrain(&mut self, index: usize) {
        self.terrain = (index < self.terrains.len()).then_some(index);
    }

    /// Give [`Self::anchor_tile`] the drafted mask in the selected terrain.
    pub fn assign_anchor_tile(&mut self, project_root: Option<&Path>) {
        let mask = terrain::canonical(self.mask_draft);
        let Some(tile) = self.anchor_tile() else {
            return;
        };
        let Some(terrain) = self.terrain.and_then(|i| self.terrains.get_mut(i)) else {
            return;
        };
        terrain.assign(tile, mask);
        self.save_terrains(project_root);
    }

    pub fn unassign_tile(&mut self, tile: u16, project_root: Option<&Path>) {
        let Some(terrain) = self.terrain.and_then(|i| self.terrains.get_mut(i)) else {
            return;
        };
        terrain.remove_tile(tile);
        self.save_terrains(project_root);
    }

    /// Write the terrains into the bound tileset's editor sidecar, keeping
    /// whatever else (zoom, cols) the tileset panel stored there. A tileset
    /// outside the worktree has no sidecar key, which is reported rather
    /// than silently dropped.
    pub fn save_terrains(&mut self, project_root: Option<&Path>) {
        self.terrain_error = match (project_root, &self.til_meta_rel) {
            (Some(root), Some(rel)) => {
                let mut meta = load_tileset_meta(root, rel);
                meta.terrains = self.terrains.clone();
                save_tileset_meta(root, rel, &meta).err()
            }
            _ => Some("tileset is outside the worktree; terrains not saved".to_string()),
        };
    }

    /// Compose the LIVE document into straight-alpha RGBA8 plus its pixel
    /// size, for a host that keeps its own image cache. `None` while the
    /// map is unbound.
    pub fn live_rgba(&self) -> Option<(Vec<u8>, u32, u32)> {
        let tileset = self.tileset.as_ref()?;
        Some(loader::compose_live_rgba(&self.store.state(), tileset))
    }

    /// The live compose as a gpui image. Called once per mutation (not per
    /// render): a 256x256 map composes ~16.8M pixels, so this is the one
    /// expensive step and it must not run on repaints that changed nothing
    /// -- the same reason ggo-ide caches its compose on
    /// `MapDocStore::generation`.
    pub fn live_image(&self) -> Option<Arc<RenderImage>> {
        let tileset = self.tileset.as_ref()?;
        loader::compose_live_image(&self.store.state(), tileset)
    }

    /// `state()` -> `save_map` -> `mark_saved`. Synchronous by choice, same
    /// call `ggo_world_panel::save_impl` makes: a `.map` is one small
    /// atomic write, and writing then marking in one step avoids the
    /// marked-depth race a mid-flight edit would cause (which is exactly
    /// what ggo-ide needs `io::map_save_race_safe` for).
    ///
    /// Writes ONLY the `.map` -- the bound `.til`/`.pal` are read-only
    /// context for a map editor (`map_doc`'s module doc), and `save_map`
    /// enforces that. A failure lands in [`Self::save_error`] for the host
    /// to render AND comes back as an `Err` for the host to branch on.
    pub fn save(&mut self) -> Result<(), String> {
        // `self.root`, NOT the worktree root: the doc must be written back
        // where it was read from (see [`Self::root`]).
        match io::save_map(&self.root, &self.rel_path, &self.store.state()) {
            Ok(()) => {
                self.store.mark_saved();
                self.save_error = None;
                Ok(())
            }
            Err(e) => {
                self.save_error = Some(e.to_string());
                Err(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ggo_worldlib::sprites::io;
    use ggo_worldlib::sprites::map_doc::{CELL_BLANK, MapDocStore, MapState, pack_cell};
    use ggo_worldlib::sprites::palette565::PAL_SLOTS;
    use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;

    /// `loader.rs`'s fixture tileset, verbatim: `tiles` solid-index tiles
    /// with two real colors in the palette.
    fn write_tileset(root: &Path, stem: &str, tiles: usize) {
        let mut indices = vec![0u8; tiles * TILE_PIXELS];
        for (t, chunk) in indices.chunks_exact_mut(TILE_PIXELS).enumerate() {
            chunk.fill((t % PAL_SLOTS) as u8);
        }
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800; // pure 565 red
        palette[2] = 0x07E0; // green
        io::save_tileset(
            root,
            &format!("tiles/{stem}.til"),
            &indices,
            tiles,
            &palette,
        )
        .unwrap();
    }

    /// The second fixture tileset's tile count: 9 tiles lays out at
    /// `grid_cols`' 8-column fallback across 2 rows -- a different SHAPE
    /// from `fx`'s 2x1, which is what makes a rebind's coordinate system
    /// observable rather than just its pixels.
    const WIDE_TILES: usize = 9;

    /// A 4x4 blank map bound to a 2-tile tileset, under a fresh temp root.
    fn bound_fixture(root: &Path) {
        write_tileset(root, "fx", 2);
        let state = MapState {
            w: 4,
            h: 4,
            cells: vec![CELL_BLANK; 16],
            til_path: "tiles/fx.til".to_string(),
            pal_path: "tiles/fx.pal".to_string(),
            dirty: false,
        };
        io::save_map(root, "maps/m.map", &state).unwrap();
    }

    #[test]
    fn brush_paints_the_default_stamp_and_select_tracks_pending() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        assert!(!session.dirty());

        assert!(session.paint_at((1, 1)), "brush changes the document");
        let state = session.store.state();
        assert_eq!(state.cells[4 + 1], pack_cell(0, 0, false, false));
        assert!(session.dirty());

        session.tool = MapTool::Select;
        assert!(
            !session.paint_at((0, 0)),
            "select never touches the document"
        );
        assert!(!session.paint_at((2, 1)));
        session.commit_selection();
        assert_eq!(session.selection, Some((0, 0, 2, 1)));

        session.tool = MapTool::Terrain;
        assert!(
            !session.can_paint(),
            "the terrain tool with nothing selected has no terrain to resolve"
        );
        assert!(!session.paint_at((0, 0)));

        assert!(session.step_history(MapDocStore::undo, None));
        assert!(!session.dirty(), "one undo reverts the single brush");
    }

    #[test]
    fn save_writes_where_it_was_read_from_and_clears_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        assert!(session.paint_at((2, 3)));
        session.save().unwrap();

        assert!(!session.dirty(), "a landed save clears dirty");
        assert!(session.save_error.is_none());
        let on_disk = io::open_map(root, "maps/m.map").unwrap();
        assert_eq!(
            on_disk.cells[3 * 4 + 2],
            pack_cell(0, 0, false, false),
            "the painted cell is in the file the map was read from"
        );
    }

    /// A bind the editor can't resolve must not reach the document -- and
    /// must say so, since a host that recomposed on it would pay a full
    /// map compose to redraw identical pixels.
    #[test]
    fn a_refused_bind_reports_false_and_leaves_the_document_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        assert!(!session.bind_tileset("tiles/absent.til".to_string(), None));
        assert_eq!(
            session.store.state().til_path,
            "tiles/fx.til",
            "the refused binding must not reach the store"
        );
        assert!(!session.dirty());
        assert!(session.tileset.is_some(), "the old tileset stays cached");
        assert!(session.tileset_error.is_some(), "with a reason to show");

        assert!(session.bind_tileset("tiles/fx.til".to_string(), None));
        assert!(session.tileset_error.is_none());
    }

    /// The terrain tool resolves against worldlib's own 47-mask rule and
    /// lands the whole neighbourhood as ONE op -- painting a second cell
    /// must re-resolve the FIRST one too, and a single undo must take both
    /// writes back together.
    #[test]
    fn terrain_paint_resolves_neighbours_and_lands_as_one_op() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tileset(root, "fx", 3);
        let state = MapState {
            w: 4,
            h: 4,
            cells: vec![CELL_BLANK; 16],
            til_path: "tiles/fx.til".to_string(),
            pal_path: "tiles/fx.pal".to_string(),
            dirty: false,
        };
        io::save_map(root, "maps/m.map", &state).unwrap();

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        session.set_tool(MapTool::Terrain);
        session.add_terrain("ground".to_string(), Some(root));
        assert_eq!(session.terrain, Some(0), "a new terrain is selected");
        assert!(
            session.terrain_error.is_none(),
            "and reaches the tileset's sidecar: {:?}",
            session.terrain_error
        );
        // Tile 0 is the isolated tile, 1 has an east neighbour, 2 a west
        // one -- the smallest fixture where a re-resolve is visible.
        session.terrains[0].assign(0, 0);
        session.terrains[0].assign(1, terrain::EAST);
        session.terrains[0].assign(2, terrain::WEST);
        session.save_terrains(Some(root));

        assert!(session.can_paint(), "a selected terrain is paintable");
        assert!(session.paint_at((0, 0)));
        assert_eq!(unpack_cell(session.store.state().cells[0]).tile, 0);

        // The second cell's write is what `terrain::resolve` says it is --
        // asserted against worldlib itself, not a restatement of its rule.
        let before = session.store.state();
        let expected = terrain::resolve(
            &before.cells,
            before.w,
            before.h,
            &[(1, 0)],
            &session.terrains[0],
            true,
        );
        assert_eq!(expected.len(), 2, "the east paint re-resolves (0, 0) too");
        assert!(session.paint_at((1, 0)));
        let after = session.store.state();
        for (x, y, cell) in expected {
            assert_eq!(
                after.cells[y as usize * after.w as usize + x as usize],
                cell,
                "cell ({x}, {y})"
            );
        }

        assert!(session.step_history(MapDocStore::undo, None));
        let undone = session.store.state();
        assert_eq!(
            unpack_cell(undone.cells[0]).tile,
            0,
            "one undo took the whole 2-cell resolve back -- it is ONE SetCells"
        );
        assert_eq!(undone.cells[1], CELL_BLANK);
    }

    /// Copy/paste round-trip through a host-held clipboard: the selection
    /// becomes a stamp, the stamp lands where the paste says, the pasted
    /// block becomes the selection, and delete blanks exactly it.
    #[test]
    fn selection_copies_pastes_and_deletes_as_whole_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        let tile0 = pack_cell(0, 0, false, false);
        assert!(session.paint_at((1, 1)));

        session.set_tool(MapTool::Select);
        session.begin_gesture(false);
        session.paint_at((1, 1));
        session.paint_at((2, 2));
        assert!(!session.end_gesture(), "a select drag edits no cells");
        assert_eq!(session.selection, Some((1, 1, 2, 2)));

        // The host's clipboard: a plain `Stamp`, which is the whole point
        // -- it survives switching to another map and back.
        let clipboard = session.selection_stamp().expect("the selection copies");
        assert_eq!((clipboard.w, clipboard.h), (2, 2));
        assert_eq!(
            clipboard.cells,
            vec![tile0, CELL_BLANK, CELL_BLANK, CELL_BLANK]
        );

        assert!(session.paste_stamp(clipboard, (0, 2)));
        let state = session.store.state();
        assert_eq!(state.cells[2 * 4], tile0, "the block landed at (0, 2)");
        assert_eq!(
            session.selection,
            Some((0, 2, 1, 3)),
            "and the paste is what is selected afterwards"
        );

        assert!(session.delete_selection());
        let state = session.store.state();
        assert_eq!(state.cells[2 * 4], CELL_BLANK, "delete blanked the block");
        assert_eq!(state.cells[4 + 1], tile0, "and nothing outside it");

        assert!(session.step_history(MapDocStore::undo, None));
        assert_eq!(
            session.store.state().cells[2 * 4],
            tile0,
            "one undo restores the whole block -- the delete is ONE rect fill"
        );

        assert!(session.clear_selection());
        assert_eq!(session.selection, None);
        assert!(
            !session.clear_selection(),
            "and there is nothing left to clear"
        );
        assert!(
            !session.delete_selection(),
            "delete with nothing selected is a no-op"
        );
    }

    #[test]
    fn resize_clamps_to_the_editor_limits_and_undoes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        session.resize(8, 2);
        let state = session.store.state();
        assert_eq!((state.w, state.h), (8, 2));

        session.resize(9999, 0);
        let state = session.store.state();
        assert_eq!(
            (state.w, state.h),
            (geom::MAX_MAP_DIM, geom::MIN_MAP_DIM),
            "both ends clamp rather than refusing"
        );

        assert!(session.step_history(MapDocStore::undo, None));
        let state = session.store.state();
        assert_eq!((state.w, state.h), (8, 2), "a resize is one undo step");
    }

    /// The strip's drag-select trio, now that `pal_dragging` lives with the
    /// corners it gates: a press arms and anchors, a held move extends
    /// (misses hold), a release keeps the picked rect, and a move without
    /// the button ends the drag instead of extending it.
    #[test]
    fn strip_drag_arms_extends_and_ends_with_the_button() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        assert!(!session.strip_press(None), "a press on a miss arms nothing");
        assert!(!session.pal_dragging);

        assert!(session.strip_press(Some((1, 0))));
        assert!(session.pal_dragging);
        assert_eq!((session.pal_anchor, session.pal_far), ((1, 0), (1, 0)));

        assert!(session.strip_move(Some((0, 0)), true));
        assert_eq!(session.pal_anchor, (1, 0), "the anchor holds");
        assert_eq!(session.pal_far, (0, 0));
        assert!(
            !session.strip_move(None, true),
            "a miss mid-drag leaves the selection put"
        );
        assert_eq!(session.pal_far, (0, 0));

        session.strip_release();
        assert!(!session.pal_dragging);
        assert_eq!(session.current_stamp().cells.len(), 2, "a 2-tile stamp");

        session.strip_press(Some((0, 0)));
        assert!(!session.strip_move(Some((1, 0)), false));
        assert!(!session.pal_dragging, "a move without the button ends it");
        assert_eq!(session.pal_far, (0, 0), "without extending the selection");
    }

    #[test]
    fn painting_an_unbound_map_is_inert() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        io::save_new_map(root, "maps/fresh.map", 4, 4).unwrap();

        let mut session = PaintSession::load(root, "maps/fresh.map", root).unwrap();
        assert!(session.tileset.is_none());
        assert!(session.tileset_error.is_some());

        assert!(!session.can_paint(), "no tile pool, nothing to paint with");
        assert!(!session.paint_at((1, 1)), "no tile pool, no paint");
        assert!(!session.dirty());
        assert!(
            session.store.state().cells.iter().all(|&c| c == CELL_BLANK),
            "an unbound map must not collect indices nothing can resolve"
        );
    }

    /// A flood fill stops at the painted wall instead of covering the map,
    /// and lands as one undo step.
    #[test]
    fn flood_fill_covers_only_the_connected_region() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        for y in 0..4 {
            assert!(session.paint_at((2, y)));
        }
        session.set_tool(MapTool::Fill);
        assert!(session.paint_at((0, 0)));

        let tile0 = pack_cell(0, 0, false, false);
        let state = session.store.state();
        for y in 0..4usize {
            assert_eq!(
                &state.cells[y * 4..][..4],
                &[tile0, tile0, tile0, CELL_BLANK],
                "row {y}: column 3 is walled off by the painted column 2"
            );
        }

        assert!(session.step_history(MapDocStore::undo, None));
        assert_eq!(
            session.store.state().cells[0],
            CELL_BLANK,
            "the fill is its own undo step"
        );
    }

    #[test]
    fn the_eraser_blanks_a_painted_cell_and_undo_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        assert!(session.paint_at((2, 0)));
        let painted = session.store.state().cells;
        assert_ne!(painted[2], CELL_BLANK);

        session.set_tool(MapTool::Eraser);
        assert!(session.paint_at((2, 0)));
        assert_eq!(session.store.state().cells[2], CELL_BLANK);

        assert!(session.step_history(MapDocStore::undo, None));
        assert_eq!(
            session.store.state().cells,
            painted,
            "undo must restore the erased cell"
        );
    }

    /// The eyedropper adopts the picked cell's flips and palSub and moves
    /// the strip selection onto its source tile -- and a BLANK cell leaves
    /// the selection alone (worldlib's own rule, ported from ggo-ide).
    #[test]
    fn the_eyedropper_adopts_the_cell_and_moves_the_strip_selection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tileset(root, "fx", 2);
        let mut cells = vec![CELL_BLANK; 16];
        cells[1] = pack_cell(1, 5, true, false);
        io::save_map(
            root,
            "maps/m.map",
            &MapState {
                w: 4,
                h: 4,
                cells,
                til_path: "tiles/fx.til".to_string(),
                pal_path: "tiles/fx.pal".to_string(),
                dirty: false,
            },
        )
        .unwrap();

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        session.set_tool(MapTool::Eyedropper);
        assert!(
            !session.paint_at((1, 0)),
            "picking must not edit the document"
        );
        assert_eq!(session.pal_sub, 5);
        assert!(session.hflip);
        assert!(!session.vflip);
        assert_eq!(
            session.pal_anchor,
            (1, 0),
            "tile 1 sits at (1, 0) in a 2-column strip"
        );
        assert_eq!(session.pal_far, (1, 0));
        assert!(!session.dirty());

        // A blank cell adopts the flips it reads (all false) but leaves the
        // tile selection put -- it names no tile in range.
        session.paint_at((3, 3));
        assert_eq!(
            session.pal_anchor,
            (1, 0),
            "a blank cell has no source tile"
        );
        assert_eq!(session.pal_sub, 0);
    }

    /// The whole `rect_pending` lifecycle: a drag previews without touching
    /// the document, the release commits ONE fill, an abandoned gesture does
    /// not arm the next one, and a tool switch discards the preview.
    ///
    /// The abandoned-gesture half is a regression (fix round 1 BLOCKING 3):
    /// [`PaintSession::paint_at`]'s RectFill arm EXTENDS a pending rect, so
    /// a gesture whose release never reached the canvas turned the next
    /// one-cell click into a fill from the stale anchor.
    #[test]
    fn rect_fill_commits_once_on_release_and_never_from_a_stale_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);
        let filled = pack_cell(0, 0, false, false);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        session.set_tool(MapTool::RectFill);
        session.begin_gesture(false);
        assert!(!session.paint_at((0, 0)));
        assert!(!session.paint_at((2, 1)), "the drag extends the preview");
        assert!(
            session.store.state().cells.iter().all(|&c| c == CELL_BLANK),
            "a rect-fill drag must not touch the document until release"
        );
        assert_eq!(session.rect_pending, Some((0, 0, 2, 1)));

        assert!(session.end_gesture());
        assert!(session.rect_pending.is_none());
        let state = session.store.state();
        for y in 0..2usize {
            for x in 0..3usize {
                assert_eq!(state.cells[y * 4 + x], filled, "({x}, {y})");
            }
        }
        assert_eq!(state.cells[2 * 4], CELL_BLANK, "row 2 untouched");

        assert!(session.step_history(MapDocStore::undo, None));
        assert!(
            session.store.state().cells.iter().all(|&c| c == CELL_BLANK),
            "one undo takes the whole rectangle back"
        );

        // Gesture 1: pressed at (0, 0), then abandoned -- no release ever
        // arrives. Gesture 2 is a fresh one-cell click at (3, 2).
        session.begin_gesture(false);
        session.paint_at((0, 0));
        assert_eq!(session.rect_pending, Some((0, 0, 0, 0)));
        session.begin_gesture(false);
        session.paint_at((3, 2));
        assert_eq!(
            session.rect_pending,
            Some((3, 2, 3, 2)),
            "a new gesture starts a NEW rect, it does not extend the abandoned one"
        );
        assert!(session.end_gesture());
        let state = session.store.state();
        assert_eq!(
            state.cells.iter().filter(|&&c| c == filled).count(),
            1,
            "a one-cell click fills exactly one cell"
        );
        assert_eq!(state.cells[2 * 4 + 3], filled);
        assert_eq!(
            state.cells[0], CELL_BLANK,
            "the abandoned anchor stays blank"
        );

        session.begin_gesture(false);
        session.paint_at((1, 1));
        assert!(session.rect_pending.is_some());
        session.set_tool(MapTool::Eraser);
        assert!(
            session.rect_pending.is_none(),
            "the pending rect must not survive the tool switch"
        );
    }

    /// A strip drag-select picks a multi-tile stamp (flips and palSub folded
    /// in), a brush lays that whole rectangle down per application, and the
    /// drag's applications fold into ONE undo entry.
    #[test]
    fn a_multi_tile_stamp_paints_a_block_and_the_drag_undoes_as_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        session.pal_anchor = (0, 0);
        session.pal_far = (1, 0);
        session.hflip = true;
        session.vflip = true;
        session.pal_sub = 5;
        let stamp = session.current_stamp();
        assert_eq!((stamp.w, stamp.h), (2, 1));
        assert_eq!(
            stamp.cells,
            vec![pack_cell(0, 5, true, true), pack_cell(1, 5, true, true)]
        );

        session.begin_gesture(false);
        assert!(session.paint_at((0, 0)));
        assert!(session.paint_at((0, 2)));
        session.end_gesture();
        let state = session.store.state();
        assert_eq!(state.cells[0], pack_cell(0, 5, true, true));
        assert_eq!(state.cells[1], pack_cell(1, 5, true, true));
        assert_eq!(state.cells[2], CELL_BLANK, "the stamp is 2 wide, not 3");
        assert_eq!(state.cells[2 * 4], pack_cell(0, 5, true, true));

        assert!(session.step_history(MapDocStore::undo, None));
        assert!(
            session.store.state().cells.iter().all(|&c| c == CELL_BLANK),
            "one undo reverts the whole drag"
        );
        assert!(
            !session.step_history(MapDocStore::undo, None),
            "a second undo has nothing to revert"
        );
    }

    /// **Regression, fix round 1 BLOCKING 1.** Undo/redo across a bind has
    /// to drag the CACHED tileset along with the store's `til_path` --
    /// [`PaintSession::step_history`]'s whole reason for existing.
    ///
    /// Both halves are asserted. Bind then undo: the cache must clear, or
    /// `paint_at`'s unbound gate passes on an unbound document and the map
    /// collects indices nothing can resolve. Rebind A -> B then undo: the
    /// cache must follow the store BACK, tile count and columns included,
    /// or the strip and `build_stamp`'s `row * cols + col` are computed
    /// against the wrong sheet.
    #[test]
    fn undo_across_a_bind_resyncs_the_cached_tileset() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tileset(root, "fx", 2);
        write_tileset(root, "wide", WIDE_TILES);
        io::save_new_map(root, "maps/fresh.map", 4, 4).unwrap();

        let mut session = PaintSession::load(root, "maps/fresh.map", root).unwrap();
        assert!(session.tileset.is_none(), "a new map is born unbound");

        assert!(session.bind_tileset("tiles/fx.til".to_string(), None));
        assert!(session.strip.is_some());

        assert!(session.step_history(MapDocStore::undo, None));
        assert_eq!(session.store.state().til_path, "", "the store unbinds");
        assert!(
            session.tileset.is_none(),
            "and the cached tileset must unbind WITH it"
        );
        assert!(session.strip.is_none(), "the strip is gone too");
        assert!(session.tileset_error.is_some(), "with a reason to show");
        assert!(
            !session.paint_at((0, 0)),
            "painting an unbound map is inert again after the undo"
        );
        assert!(!session.dirty());

        assert!(session.step_history(MapDocStore::redo, None));
        assert_eq!(
            session.tileset.as_ref().map(|ts| (ts.tile_count, ts.cols)),
            Some((2, 2)),
            "and the redo brings the cache back too"
        );

        assert!(session.bind_tileset("tiles/wide.til".to_string(), None));
        assert_eq!(
            session.tileset.as_ref().map(|ts| (ts.tile_count, ts.cols)),
            Some((WIDE_TILES, 8))
        );

        assert!(session.step_history(MapDocStore::undo, None));
        assert_eq!(session.store.state().til_path, "tiles/fx.til");
        assert_eq!(
            session.tileset.as_ref().map(|ts| (ts.tile_count, ts.cols)),
            Some((2, 2)),
            "the cached tileset must follow the store back"
        );
        assert!(session.strip.is_some(), "and the strip recomposes for it");

        // The stamp coordinate system followed too: strip cell (0, 1) is
        // tile 8 under an 8-wide sheet and does not exist at all in a
        // 2-tile, 2-column one.
        session.pal_anchor = (0, 1);
        session.pal_far = (0, 1);
        assert_eq!(
            session.current_stamp().cells,
            vec![CELL_BLANK],
            "row 1 does not exist in a 2-tile, 2-column sheet"
        );
    }

    /// The bound tileset's editor sidecar drives BOTH the strip's column
    /// count and the terrain list, and an undo across the bind puts the
    /// previous tileset's (absent) sidecar back.
    #[test]
    fn the_tileset_sidecar_drives_the_strip_cols_and_the_terrains() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        bound_fixture(root);
        write_tileset(root, "wide", WIDE_TILES);
        save_tileset_meta(
            root,
            "tiles/wide.til",
            &TilesetMeta {
                cols: Some(3),
                terrains: vec![Terrain {
                    name: "grass".to_string(),
                    tiles: vec![],
                }],
                ..Default::default()
            },
        )
        .unwrap();

        let mut session = PaintSession::load(root, "maps/m.map", root).unwrap();
        assert!(session.bind_tileset("tiles/wide.til".to_string(), Some(root)));
        assert_eq!(
            session.tileset.as_ref().map(|ts| ts.cols),
            Some(3),
            "cols come from the sidecar, not the fallback"
        );
        assert_eq!(
            session.terrains.first().map(|t| t.name.as_str()),
            Some("grass")
        );
        assert_eq!(session.til_meta_rel.as_deref(), Some("tiles/wide.til"));

        assert!(session.step_history(MapDocStore::undo, Some(root)));
        assert_eq!(
            session.tileset.as_ref().map(|ts| ts.cols),
            Some(2),
            "fx.til has no sidecar, so it is back to the default layout"
        );
        assert!(session.terrains.is_empty());
    }
}
