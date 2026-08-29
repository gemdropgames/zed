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
use ggo_worldlib::sprites::tileset_meta::TilesetMeta;

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
            rect_pending: None,
            sel_pending: None,
            selection: None,
            paint_erase: false,
            terrains: loaded.tileset_meta.terrains,
            terrain: None,
            til_meta_rel: loaded.til_meta_rel,
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

    /// One tool application at `cell` -- the canvas's primary-down /
    /// drag-move body, shared by both events (ggo-ide re-fires the same
    /// tool action on every `Moved` while painting).
    ///
    /// Returns true when the DOCUMENT changed, i.e. when the host has to
    /// recompose; Select, Eyedropper and a growing rect-fill mutate only
    /// session state and return false.
    ///
    /// Gated on a bound tileset: without one there is no tile pool for a
    /// cell index to mean anything, so painting is inert rather than
    /// writing indices into a map nothing can resolve -- ggo-ide's
    /// `on_map_surface_event` opens with the same `tileset_data.is_none()`
    /// early return.
    pub fn paint_at(&mut self, cell: (i32, i32)) -> bool {
        let (x, y) = cell;
        if self.tileset.is_none() {
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
    /// ONE `MapOp::RectFill`. Clears the preview whatever the tool is (a
    /// tool switch mid-drag must not leave a stale rectangle on screen).
    /// Returns true when the document changed.
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
    ///   rationale (`ggo_map_panel::create_map_inline`) exists to prevent;
    /// - rebind A -> B, then undo: the canvas, the strip AND
    ///   `build_stamp`'s `row * cols + col` are all computed against B's
    ///   tile count and column layout while the document is bound to A.
    ///
    /// (ggo-ide has the same gap. Inherited, not a regression -- but it
    /// defeats an invariant the map panel states in its own module doc, so
    /// it is fixed here rather than ported.)
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
    pub fn bind_tileset(&mut self, til_rel: String, project_root: Option<&Path>) {
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
            }
            Err(e) => self.tileset_error = Some(e),
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

    #[test]
    fn painting_an_unbound_map_is_inert() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        io::save_new_map(root, "maps/fresh.map", 4, 4).unwrap();

        let mut session = PaintSession::load(root, "maps/fresh.map", root).unwrap();
        assert!(session.tileset.is_none());
        assert!(session.tileset_error.is_some());

        assert!(!session.paint_at((1, 1)), "no tile pool, no paint");
        assert!(!session.dirty());
        assert!(
            session.store.state().cells.iter().all(|&c| c == CELL_BLANK),
            "an unbound map must not collect indices nothing can resolve"
        );
    }
}
