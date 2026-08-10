//! GGO Import panel (F5.1 Task I2): PNG -> tileset import, the fork's one
//! way to get pixels in from an external art tool.
//!
//! This is the unblock for the whole art pipeline the F5 spec resolved:
//! draw tiles in Aseprite/GIMP -> **import here** -> assemble sprites and
//! metasprites out of those tiles in the sprite panel -> paint levels in
//! `ggo_map_panel`. Nothing in the fork paints pixels; this panel is where
//! pixels arrive.
//!
//! **Tileset-only, by explicit user decision.** ggo-ide's `ImportWizard.tsx`
//! had three modes (Tileset / Sprite / Metasprite); only Tileset is ported.
//! Frames are assembled from imported TILES in the sprite panel, never
//! sliced out of a PNG, so `import::sprite_import` -- which exists in
//! worldlib and is tested there -- is deliberately not called from anywhere
//! in this fork. See the module's "Not ported" section below.
//!
//! **No import logic lives here.** Decode, crop, quantize, preview
//! composition, tile slicing, destination-path joining, collision detection
//! and the whole crop-drag state machine are
//! `ggo_worldlib::sprites::import`'s, extracted and unit-tested in ggo PR
//! #81; the indexed->RGBA expansion is the one shared
//! `palette565::indices_to_rgba` (PR #80); the write is
//! `ggo_worldlib::sprites::io::save_tileset`. This module owns the panel
//! entity, the gpui glue and the canvas CAMERA (which worldlib explicitly
//! leaves to its caller), and even that camera's maths lives in [`geom`] as
//! pure, separately-tested functions.
//!
//! The wizard state is worldlib's own [`WizardState`], pinned to
//! [`Mode::Tileset`] at construction and never switched. Reusing it rather
//! than restating a tileset-shaped subset is the point: the crop gestures,
//! the "settle, then quantize" preview discipline, the dest-path join and
//! the target list are all already tested there, case-for-case against the
//! TypeScript original.
//!
//! ## Interaction model
//!
//! Left-drag on the source canvas draws a **crop rect** (the region that
//! will be quantized and sliced); middle-drag pans and the wheel zooms, both
//! at integer scale. The crop settles on pointer-up, which is when the
//! quantized **preview** is recomputed -- the same "don't quantize every
//! pointer-move" rule `WizardState::commit_region` documents. A **tile grid**
//! is drawn INSIDE the crop at [`ggo_asset_formats::TILE_PX`], which is the
//! step -- the ONLY step -- `slice_to_tiles` cuts at; the footer reports the
//! tile count that will be written and how many of those tiles are WHOLE, so
//! a crop that will be zero-padded on its right/bottom edge is visible before
//! the commit rather than after.
//!
//! There is deliberately **no cell-size control**. ggo-ide had Cell W/H
//! inputs and a live grid at that size, but they lived inside
//! `<Show when={mode() === 'metasprite'}>` -- they sized METASPRITE FRAMES
//! and never existed in Tileset mode. Porting them here (fix round 1,
//! BLOCKING 1) drew dividers where nothing is cut and mis-flagged a
//! tile-aligned crop as ragged, because `slice_to_tiles` is hard-wired to
//! `TILE_PX`.
//!
//! ## Not ported from ggo-ide's wizard (deliberate, not oversight)
//!
//! - **Sprite and Metasprite modes** -- the user decision above.
//! - **The `{cols}` JSON sidecar** an imported tileset used to get. worldlib
//!   already declined to port it (`import`'s module doc, deviation #2: this
//!   native tool banned new sidecars) and `ggo_tileset_panel` resolves an
//!   imported sheet's columns exactly like a brand-new one's.
//! - **Legacy `.meta.json` import** -- staying dropped (spec, "Staying
//!   dropped").
//! - **An in-panel file picker / drag-and-drop.** The entry point is the
//!   project panel's "Import as tileset…" on a `.png`, which is strictly
//!   better than ggo-ide's native file dialog: the source is already a
//!   project path, so the destination and the delete-source offer both
//!   derive from it with nothing to type.
//! - **Manual palette surgery** (slot edits, ramps, sort/swap) has no
//!   replacement anywhere in the fork; the palette is whatever quantization
//!   derives. The spec calls this out as an honest, permanent loss.

mod geom;
mod loader;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use editor::Editor;
use gpui::{
    Action, App, BorderStyle, Bounds, ContentMask, Context, Corners, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, RenderImage, ScrollWheelEvent, Styled, Task,
    WeakEntity, Window, actions, bounds, div, fill, img, outline, point, px, rgb, rgba, size,
};
use project::ProjectPath;
use ui::prelude::*;
use ui::{Checkbox, ToggleState, Tooltip};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_tileset_panel::TilesetPanel;
use ggo_worldlib::sprites::import::{
    Mode, Region, WizardState, existing_collisions, join_dest_path, slice_to_tiles,
    source_rel_if_in_project,
};
use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::palette565::{PAL_SLOTS, slot_rgba};

actions!(
    ggo_import,
    [
        /// Toggles focus on the GGO import panel.
        ToggleFocus,
        /// Writes the pending import to its `.til`/`.pal`.
        Import
    ]
);

const GGO_IMPORT_PANEL_KEY: &str = "GGOImportPanel";

/// The panel's key-dispatch context (`.key_context`), which the
/// [`bind_panel_keys`] bindings are scoped to.
const KEY_CONTEXT: &str = "GgoImportPanel";

/// Fixed default width until the panel grows real settings persistence
/// (same call every other GGO panel made at this stage). As wide as the map
/// panel: this one stacks a crop canvas, a preview and a form.
const DEFAULT_WIDTH: Pixels = px(460.);

/// The source extension this panel's context-menu entry is offered on.
const PNG_EXT: &str = "png";

/// The emerald manifest that marks a project root, and the assets
/// subdirectory hanging off it. Both are hardcoded upstream -- the assets
/// dir is NOT a configurable `emerald.toml` key. Same pair
/// `ggo_metasprite_panel` and `ggo_map_panel` resolve their sidecars with.
const EMERALD_MANIFEST: &str = "emerald.toml";
const ASSETS_DIR: &str = "assets";

/// Empty-state text. The panel has no picker of its own by design: sources
/// arrive by right-clicking a `.png` in the project panel.
const EMPTY_MESSAGE: &str = "Right-click a .png in the project panel → Import as tileset…";

/// The quantized preview strip's height.
const PREVIEW_HEIGHT: Pixels = px(120.);

/// Palette swatch box (px, square) -- same size `ggo_tileset_panel` draws.
const SWATCH_PX: f32 = 16.0;

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as every other GGO panel's `init`: `zed::reload_keymaps`
    // clears and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), and keymap assets are upstream files this
    // fork doesn't edit. Re-running `bind_panel_keys` on `KeymapEventChannel`
    // keeps the panel's bindings alive across reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    // Right-clicking a `.png` offers "Import as tileset…". Deliberately NOT
    // a `register_path_open_interceptor`: a LEFT click on a `.png` must keep
    // opening upstream's image viewer, which is a perfectly good way to look
    // at one. Importing is an explicit, destination-writing action, so it
    // gets an explicit menu entry.
    workspace::register_context_menu_contributor(cx, contribute_import_menu);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| ImportPanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ImportPanel>(window, cx);
        });
    })
    .detach();
}

/// No panel-specific keybinds: the two destination fields are read straight
/// off their editors on every render (`sync_dest_fields`), so there is
/// nothing for Enter to "apply", and Import is button-only (it writes files;
/// a stray keystroke should not). Kept as its own fn -- rather than inlined
/// into `init` -- so it matches the other GGO panels' shape exactly: `init`
/// calls it once at startup AND the `KeymapEventChannel` observer calls it
/// again on every reload.
fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([]);
}

/// Does `path` name a PNG? One rule, so the menu predicate and the tests
/// agree.
fn is_png_path(path: &ProjectPath) -> bool {
    path.path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(PNG_EXT))
}

/// `workspace::ContextMenuContributor` for a `.png` FILE: "Import as
/// tileset…".
///
/// Not gated to the `assets/` tree, unlike `ggo_map_panel`'s "New Map…". A
/// source PNG is an INPUT, not an asset -- art often lands in `art/` or
/// `raw/` beside the project, and ggo-ide's own wizard picked arbitrary
/// files off disk. What must resolve inside `assets/` is the DESTINATION,
/// and that is derived separately ([`split_png_path`]) with a documented
/// fallback for a source outside an emerald project.
///
/// MUST NOT touch the project panel or any GGO panel: contributors run while
/// `ProjectPanel` is leased (see `Workspace::context_menu_contributions`).
/// Everything panel-shaped is deferred into the entry's handler via
/// [`ggo_common::panel_entry_handler`], which runs after the lease is
/// released.
fn contribute_import_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    if is_dir || !is_png_path(path) {
        return Vec::new();
    }
    // Declines a path outside the primary worktree AND a non-local project
    // (SSH remote / collab guest): this panel reads and writes with
    // `std::fs` against the worktree's `abs_path`, which on a remote project
    // names a directory that does not exist on this machine. Offering an
    // entry that could only fail is worse than not offering it.
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    vec![
        ui::ContextMenuEntry::new("Import as tileset…")
            .icon(ui::IconName::Download)
            .handler(import_png_handler(cx.weak_entity(), rel))
            .into(),
    ]
}

/// The "Import as tileset…" entry's handler. Split out from
/// [`contribute_import_menu`] so a test can invoke exactly what the menu
/// invokes -- `ContextMenuEntry` keeps its handler private, so a contributed
/// entry cannot be fired from a test any other way.
fn import_png_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(workspace, move |panel: &Entity<ImportPanel>, window, cx| {
        let rel = rel.clone();
        panel.update(cx, |panel, cx| panel.open_source(&rel, window, cx));
    })
}

/// Walk up from `start`'s own directory to the nearest emerald project root
/// (the nearest ancestor holding `emerald.toml`, mirroring emerald's
/// `Project::discover`), returning that project's `assets/` dir. Same
/// function `ggo_metasprite_panel` uses to resolve a `.spr`'s root.
fn emerald_asset_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start.parent();
    while let Some(dir) = cur {
        if dir.join(EMERALD_MANIFEST).is_file() {
            let assets = dir.join(ASSETS_DIR);
            return assets.is_dir().then_some(assets);
        }
        cur = dir.parent();
    }
    None
}

/// The asset root an imported tileset must be written against, plus the
/// source PNG's path relative to THAT root.
///
/// **This is the F4 `ggo-sprfix` trap.** Emerald treats asset rels as
/// **asset-root-relative**, where the asset root is `<project>/assets`
/// (`crates/cli/src/commands/pack.rs` packs `project.root.join("assets")`
/// and `crates/assets/src/lib.rs` strips that dir off every asset path), so
/// a correct rel never carries an `assets/` segment. Importing
/// `<proj>/assets/art/hero.png` therefore has to yield root `<proj>/assets`
/// and rel `art/hero.png`, so the tileset lands at `art/hero.til` -- the
/// name a `.map`'s `til_path` or a `.spr`'s sidecar will later hold. Writing
/// `assets/art/hero.til` instead is precisely the bug `ggo-sprfix` exists to
/// repair, and it is not caught by the file landing in the right PLACE (it
/// does either way): what breaks is the NAME every downstream binder stores.
///
/// Falls back to `(project_root, rel)` when the source isn't inside an
/// emerald project's `assets/` tree -- a PNG in `art/` beside the project, or
/// in a non-emerald worktree, still imports, just rooted at the worktree.
fn split_png_path(project_root: &Path, rel: &str) -> (PathBuf, String) {
    let abs = project_root.join(rel);
    if let Some(assets) = emerald_asset_root(&abs)
        && let Ok(under) = abs.strip_prefix(&assets)
    {
        let under = under
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        return (assets, under);
    }
    (project_root.to_path_buf(), rel.to_string())
}

/// The root a commit actually writes against, and the destination's rel
/// relative to THAT root -- [`split_png_path`]'s rule re-applied to the
/// RESOLVED DESTINATION instead of to the source.
///
/// **Fix round 1, BLOCKING 2.** The destination directory is user-editable,
/// so it can move the write out of the frame the SOURCE was resolved in: a
/// PNG in `art/` has no `assets/` ancestor, so `split_png_path` falls back to
/// the worktree root -- and typing `assets/tiles` into Dir then aimed the
/// write INTO the asset tree while still naming it `assets/tiles/x.til`,
/// which is exactly the `ggo-sprfix` shape. Re-deriving from where the bytes
/// are actually going is what keeps the contract true for a retargeted
/// import; deriving once, from the source, is what made it false.
///
/// Idempotent for a destination already in the source's own frame, and a
/// no-op (worktree root passes through) when the destination is outside any
/// emerald `assets/` tree.
///
/// This is also what makes an out-of-assets PNG importable INTO the assets
/// tree at all: worldlib's `safe_join` rejects `..`, so a `../assets/tiles`
/// dir could never have worked.
fn resolve_dest(root: &Path, rel_stem: &str) -> (PathBuf, String) {
    let abs = root.join(rel_stem);
    if let Some(assets) = emerald_asset_root(&abs)
        && let Ok(under) = abs.strip_prefix(&assets)
    {
        return (
            assets,
            under
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/"),
        );
    }
    (root.to_path_buf(), rel_stem.to_string())
}

/// The directory part of a `/`-separated rel (`"art/hero.png"` -> `"art"`,
/// a bare `"hero.png"` -> `""`) -- the destination directory an import
/// defaults to, which is wherever the source PNG already lives.
fn parent_dir(rel: &str) -> String {
    match rel.rfind('/') {
        Some(i) => rel[..i].to_string(),
        None => String::new(),
    }
}

/// The WORKTREE-relative path of an asset written at asset-root-relative
/// `asset_rel` under `asset_root`.
///
/// The other half of the [`split_png_path`] rule, and the reason both halves
/// have to exist: the `.til` is WRITTEN with an asset-root-relative rel (so
/// every downstream binder names it correctly), but `ggo_tileset_panel`
/// opens documents by WORKTREE-relative path (its interceptor is fed
/// `rel_in_primary_worktree`). Handing it the asset rel would open
/// `<worktree>/art/hero.til`, which does not exist. `None` when the two
/// roots aren't related, in which case the panel simply doesn't hand off.
fn worktree_rel_for(project_root: &Path, asset_root: &Path, asset_rel: &str) -> Option<String> {
    let abs = asset_root.join(asset_rel);
    let under = abs.strip_prefix(project_root).ok()?;
    Some(
        under
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
}

/// Every file already sitting in the destination directory, as asset-root-
/// relative rels -- the `existing` list [`existing_collisions`] filters the
/// commit's targets against.
///
/// ggo-ide passed the assets page's already-fetched entry snapshot; the fork
/// has no such snapshot, so the one directory a commit can write into is
/// scanned at commit time. A missing directory is an empty list, not an
/// error: it just means nothing can collide.
fn existing_rels(root: &Path, dir_rel: &str) -> Vec<String> {
    let dir = if dir_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir_rel)
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(|name| join_dest_path(dir_rel, &name))
        .collect()
}

/// The overwrite prompt's message, worded like ggo-ide's `overwritePrompt`.
fn overwrite_message(collisions: &[String]) -> String {
    format!(
        "{} already {} — overwrite?",
        collisions.join(", "),
        if collisions.len() > 1 {
            "exist"
        } else {
            "exists"
        }
    )
}

// ------------------------------------------------------------- view state

/// An in-flight middle-mouse pan drag on the crop canvas.
#[derive(Clone, Copy)]
struct PanDrag {
    start_cursor: [f32; 2],
    start_pan: [f32; 2],
}

/// The two form fields: destination directory + stem.
struct Fields {
    dir: Entity<Editor>,
    stem: Entity<Editor>,
}

/// What a committed import wrote and what to do next with it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Imported {
    /// The `.til`'s ASSET-ROOT-relative rel -- the name downstream binders
    /// will hold (see [`split_png_path`]).
    asset_rel: String,
    /// The same file's WORKTREE-relative path, for the tileset panel
    /// handoff (see [`worktree_rel_for`]).
    worktree_rel: Option<String>,
    tile_count: usize,
}

/// A loaded source PNG plus everything the view needs.
struct OpenImport {
    /// The worktree-relative path as CLICKED -- what identifies the source to
    /// the user and to the explorer.
    source_rel: String,
    source_abs: PathBuf,
    /// The ASSET ROOT this import will write against, captured at open time
    /// so a commit can't land somewhere else if the worktree is repointed
    /// meanwhile (`ggo_world_panel`'s `OpenWorld::root` idiom).
    root: PathBuf,
    /// worldlib's own wizard, pinned to [`Mode::Tileset`].
    wizard: WizardState,
    source_image: Arc<RenderImage>,
    /// The quantized preview's image, rebuilt whenever the wizard's preview
    /// changes (crop settle, reserve-transparent toggle) -- never per render.
    preview_image: Option<Arc<RenderImage>>,
    zoom: usize,
    pan: [f32; 2],
    pan_drag: Option<PanDrag>,
    /// True between the canvas's primary-down and its matching up.
    cropping: bool,
    /// The canvas element's on-screen bounds, recorded at prepaint so the
    /// mouse handlers can map window coords to image pixels
    /// (`ggo_world_panel`'s `last_bounds` idiom).
    canvas_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    fields: Option<Fields>,
}

impl OpenImport {
    fn new(
        source_rel: String,
        source_abs: PathBuf,
        root: PathBuf,
        loaded: loader::LoadedPng,
    ) -> Self {
        let file_name = source_rel
            .rsplit('/')
            .next()
            .unwrap_or(&source_rel)
            .to_string();
        let mut wizard = WizardState::new(file_name, loaded.decoded);
        // Tileset is the ONLY mode this panel has; `set_mode` is also what
        // computes the first quantized preview.
        wizard.set_mode(Mode::Tileset);
        Self {
            source_rel,
            source_abs,
            root,
            wizard,
            source_image: loaded.image,
            preview_image: None,
            zoom: geom::DEFAULT_ZOOM,
            pan: [0.0, 0.0],
            pan_drag: None,
            cropping: false,
            canvas_bounds: Rc::new(RefCell::new(None)),
            fields: None,
        }
    }

    /// Canvas-local coordinates for a window-space `position`, or `None`
    /// before the first paint has recorded the canvas bounds.
    fn canvas_local(&self, position: gpui::Point<Pixels>) -> Option<[f32; 2]> {
        let bounds = (*self.canvas_bounds.borrow())?;
        Some([
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ])
    }

    /// The image pixel under a window-space `position`, clamped into the
    /// image ([`geom::image_coord`]).
    fn image_coord_at(&self, position: gpui::Point<Pixels>) -> Option<(i32, i32)> {
        let local = self.canvas_local(position)?;
        Some(geom::image_coord(
            local,
            self.zoom,
            self.pan,
            self.wizard.src_w,
            self.wizard.src_h,
        ))
    }

    /// The region a commit will quantize and slice.
    fn crop(&self) -> Region {
        geom::effective_region(self.wizard.region, self.wizard.src_w, self.wizard.src_h)
    }

    /// The root a commit writes against and the destination's rel relative
    /// to THAT root -- re-derived from the resolved DESTINATION, not
    /// inherited from the source (see [`resolve_dest`]).
    fn dest(&self) -> (PathBuf, String) {
        resolve_dest(&self.root, &self.wizard.dest_rel_stem())
    }

    /// Every file a commit at the current destination would (over)write.
    ///
    /// `WizardState::targets` computes the same pair, but off the wizard's
    /// RAW stem -- i.e. in the source's frame, before [`resolve_dest`] has
    /// had its say. Using it here would re-introduce the `assets/`-prefixed
    /// rel that re-rooting exists to remove.
    fn dest_targets(&self) -> Vec<String> {
        let (_, stem) = self.dest();
        vec![format!("{stem}.til"), format!("{stem}.pal")]
    }
}

enum ViewerState {
    /// Nothing opened yet.
    Empty,
    Loading {
        rel_path: String,
    },
    Ready(Box<OpenImport>),
    Error(String),
}

pub struct ImportPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    /// A failure or a note worth showing (a refused commit, a delete that
    /// didn't land). Successes live in [`Self::last_import`] instead, so the
    /// two can't contradict each other.
    status: Option<String>,
    /// The last SUCCESSFUL commit. Kept on the PANEL rather than on
    /// [`OpenImport`] so it survives the source being deleted underneath it
    /// -- which is exactly when the user most needs to be told what was
    /// written.
    last_import: Option<Imported>,
}

impl ImportPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
            status: None,
            last_import: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible worktree).
    /// MUST NOT run while the workspace itself is mid-update (it reads the
    /// workspace entity) -- see the deferral in `set_active`.
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        cx.notify();
    }

    /// Load the worktree-relative `.png` path `rel` -- the panel's entry
    /// point from the project panel's context menu.
    ///
    /// No unsaved-document guard, unlike the map/world/sprite panels, and the
    /// distinction is real rather than an omission: those hold a DOCUMENT
    /// read off disk that a switch could lose edits to. This panel holds a
    /// pending import -- a crop rect over someone else's file. Nothing it
    /// holds exists on disk yet, nothing it drops was ever saved, and the
    /// source PNG is untouched until an explicit Import. There is nothing to
    /// prompt about, which is also why there is no `Panel::prepare_to_close`.
    ///
    /// Refreshes the root FIRST because `project_root` is only re-discovered
    /// on panel activation, and a right-click in the explorer can reach a
    /// panel that has never been activated. Safe here: the caller is a
    /// context-menu entry handler, which runs outside both the project
    /// panel's lease and any `Workspace` update.
    pub fn open_source(&mut self, rel: &str, _window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_root(cx);
        self.load_source(rel, cx);
    }

    /// Kick off the off-thread decode of `rel`. A stale result (superseded by
    /// a later open) is dropped by generation check.
    fn load_source(&mut self, rel: &str, cx: &mut Context<Self>) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let source_rel = rel.to_string();
        let source_abs = project_root.join(&source_rel);
        // ONE walk, destructured once: the two halves are the same answer to
        // the same question, and re-asking it after the decode could have
        // returned a different one (fix round 1, FOLD IN 3).
        let (root, under) = split_png_path(&project_root, &source_rel);
        let dest_dir = parent_dir(&under);
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            rel_path: source_rel.clone(),
        };
        self.status = None;
        cx.notify();

        let load = {
            let source_abs = source_abs.clone();
            cx.background_spawn(async move { loader::load_png(&source_abs) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => {
                        let mut open =
                            OpenImport::new(source_rel.clone(), source_abs, root.clone(), loaded);
                        // Default the destination to wherever the source
                        // already lives, ASSET-ROOT-relative.
                        open.wizard.set_dest_dir(dest_dir);
                        Self::rebuild_preview(&mut open);
                        ViewerState::Ready(Box::new(open))
                    }
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Recompose the preview image from the wizard's CURRENT quantized
    /// preview. Called once per settle (crop release, reserve-transparent
    /// toggle), never per render.
    fn rebuild_preview(open: &mut OpenImport) {
        open.preview_image = open.wizard.preview.as_ref().and_then(loader::preview_image);
    }

    fn ready(&self) -> Option<&OpenImport> {
        match &self.state {
            ViewerState::Ready(open) => Some(open),
            _ => None,
        }
    }

    /// The last successful commit, worded for the UI.
    fn import_summary(&self) -> Option<String> {
        self.last_import.as_ref().map(|imported| {
            format!(
                "Imported {} tiles → {}",
                imported.tile_count, imported.asset_rel
            )
        })
    }

    // ------------------------------------------------------- crop gestures

    /// Left-mouse down on the canvas at window-space `position`: start a crop
    /// drag anchored at that image pixel. Its own method (rather than an
    /// inline listener body) so a test can drive exactly what the element
    /// drives -- an `on_mouse_down` closure is not reachable otherwise.
    fn crop_down(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Take focus so the panel's bindings apply (and any in-progress field
        // edit stops winning the key context).
        window.focus(&self.focus_handle, cx);
        let Some((x, y)) = self.ready().and_then(|open| open.image_coord_at(position)) else {
            return;
        };
        if let ViewerState::Ready(open) = &mut self.state {
            open.cropping = true;
            open.wizard.on_primary_down(x, y);
            cx.notify();
        }
    }

    /// Continue an in-flight crop drag. The outline tracks every move; the
    /// PREVIEW does not (see [`Self::crop_up`]).
    fn crop_move(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((x, y)) = self.ready().and_then(|open| open.image_coord_at(position)) else {
            return;
        };
        if let ViewerState::Ready(open) = &mut self.state {
            if !open.cropping {
                return;
            }
            open.wizard.on_moved(x, y);
            cx.notify();
        }
    }

    /// The crop settles here: worldlib re-quantizes, and the preview image
    /// is rebuilt from the result. This is the "settle, then quantize"
    /// discipline `WizardState::on_moved` documents -- quantizing every
    /// pointer-move would re-run a 16-color median cut over the whole crop
    /// per frame.
    fn crop_up(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            if !open.cropping {
                return;
            }
            open.cropping = false;
            open.wizard.on_released();
            Self::rebuild_preview(open);
            cx.notify();
        }
    }

    fn clear_crop(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.cropping = false;
            open.wizard.clear_crop();
            Self::rebuild_preview(open);
            cx.notify();
        }
    }

    fn set_reserve_transparent(&mut self, on: bool, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.wizard.set_reserve_transparent(on);
            Self::rebuild_preview(open);
            cx.notify();
        }
    }

    /// Middle-mouse pan handling for a move event. Returns true if the event
    /// belonged to an in-flight pan (handled or cancelled).
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

    fn step_zoom(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let next = geom::zoom_by(open.zoom, delta);
        if next != open.zoom {
            open.zoom = next;
            cx.notify();
        }
    }

    /// Wheel zoom anchored on the cursor ([`geom::zoom_at`]).
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

    // ------------------------------------------------------------- fields

    /// Create the two form fields on the first Ready render, seeded from the
    /// wizard. They are never re-synced afterwards: unlike the map panel's
    /// resize fields (which mirror a document that undo/redo can change under
    /// them), these fields ARE the state -- nothing else writes the
    /// destination.
    fn ensure_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        if open.fields.is_some() {
            return;
        }
        let (dir, stem) = (open.wizard.dest_dir.clone(), open.wizard.dest_stem.clone());
        let fields = Fields {
            dir: Self::new_field(&dir, window, cx),
            stem: Self::new_field(&stem, window, cx),
        };
        if let ViewerState::Ready(open) = &mut self.state {
            open.fields = Some(fields);
        }
    }

    fn new_field(value: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        let value = value.to_string();
        cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(value, window, cx);
            editor
        })
    }

    /// Push the two destination fields into the wizard, so
    /// `dest_rel_stem`/`targets`/`can_commit` always describe what is
    /// currently TYPED.
    ///
    /// Run on every render rather than on a change subscription: both setters
    /// are plain assignments (no requantize, no disk), and this keeps one
    /// source of truth -- the editors -- instead of two that can drift.
    fn sync_dest_fields(&mut self, cx: &mut Context<Self>) {
        let Some((dir, stem)) = self
            .ready()
            .and_then(|open| open.fields.as_ref())
            .map(|fields| (fields.dir.clone(), fields.stem.clone()))
        else {
            return;
        };
        let (dir, stem) = (dir.read(cx).text(cx), stem.read(cx).text(cx));
        if let ViewerState::Ready(open) = &mut self.state {
            open.wizard.set_dest_dir(dir);
            open.wizard.set_dest_stem(stem);
        }
    }

    // ------------------------------------------------------------- commit

    /// The Import button / action: confirm any overwrite FIRST, then write,
    /// then offer to delete the source, then show the result.
    ///
    /// The collision check is `existing_collisions` over the destination
    /// directory's actual contents. It has to happen before the write and not
    /// as part of it, because `save_tileset` goes through `atomic_write`,
    /// which renames over an existing file with no prompt of its own -- so an
    /// unconfirmed commit onto an existing name would silently destroy a
    /// tileset other assets are bound to.
    fn import_impl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_dest_fields(cx);
        let workspace = self.workspace.clone();
        let Some(open) = self.ready() else { return };
        if !open.wizard.can_commit() {
            self.status = Some("Nothing to import: give the tileset a name".to_string());
            cx.notify();
            return;
        }
        // The collision scan runs in the SAME frame the write will use --
        // `OpenImport::dest`, re-derived from the destination (see
        // `resolve_dest`) -- so a retargeted import can't check one directory
        // and clobber another.
        let (dest_root, dest_stem) = open.dest();
        let targets = open.dest_targets();
        let collisions = existing_collisions(
            &existing_rels(&dest_root, &parent_dir(&dest_stem)),
            &targets,
        );
        let confirm = if collisions.is_empty() {
            Task::ready(true)
        } else {
            ggo_common::confirm_destructive(
                &overwrite_message(&collisions),
                "Overwrite",
                false,
                window,
                cx,
            )
        };

        cx.spawn_in(window, async move |this, cx| {
            if !confirm.await {
                return;
            }
            let Ok(Some((imported, source))) = this.update(cx, |this, cx| this.commit(cx)) else {
                return;
            };
            if let Some((abs, rel)) = source {
                Self::offer_source_delete(&this, abs, rel, cx).await;
            }
            // Show what was just made. A missing tileset panel is a no-op
            // (`open_in_panel` returns false), same graceful degradation
            // every other GGO handoff makes.
            if let (Some(workspace), Some(rel)) = (workspace, imported.worktree_rel) {
                // Same reason as `offer_source_delete`'s: route through the
                // window this task was spawned in rather than through an
                // entity's associated window.
                cx.update(|window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            ggo_common::open_in_panel(
                                workspace,
                                window,
                                cx,
                                move |panel: &mut TilesetPanel, window, cx| {
                                    panel.open_rel_path(&rel, window, cx)
                                },
                            );
                        })
                        .ok();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Slice the quantized preview into tiles and write the `.til`/`.pal`.
    ///
    /// Returns the commit's outcome plus the source PNG's
    /// `(absolute, asset-root-relative)` pair when it is eligible for the
    /// delete offer, or `None` when the write failed (the status line then
    /// carries the error).
    ///
    /// Synchronous, same call `ggo_map_panel::save_impl` makes: a tileset is
    /// one small atomic write and the user is waiting on their own click.
    #[allow(clippy::type_complexity)]
    fn commit(&mut self, cx: &mut Context<Self>) -> Option<(Imported, Option<(PathBuf, String)>)> {
        let project_root = self.project_root.clone();
        let ViewerState::Ready(open) = &mut self.state else {
            return None;
        };
        let preview = open.wizard.preview.as_ref()?;
        // The root the bytes go under is re-derived from the DESTINATION, not
        // inherited from the source -- see `resolve_dest`. `til_rel` is
        // therefore asset-root-relative even when the source was not in an
        // `assets/` tree at all.
        let (dest_root, dest_stem) = open.dest();
        let til_rel = format!("{dest_stem}.til");
        let (indices, tile_count) = slice_to_tiles(&preview.indices, preview.w, preview.h);
        if let Err(e) =
            io::save_tileset(&dest_root, &til_rel, &indices, tile_count, &preview.palette)
        {
            self.status = Some(format!("Import failed: {e}"));
            self.last_import = None;
            cx.notify();
            return None;
        }
        // Offered only for a source inside a REAL emerald asset root --
        // ggo-ide's own `assetsPrefix` guard, ported: a PNG outside
        // `assets/` was never going to collide with the packer on a
        // same-stem `.til`, which is the entire reason the offer exists. The
        // `emerald_asset_root` re-check matters because `split_png_path`
        // FALLS BACK to the worktree root for a source outside `assets/` --
        // without it, that fallback would make every in-worktree PNG look
        // "in the asset root" and get offered for deletion.
        let source = emerald_asset_root(&open.source_abs)
            .is_some_and(|assets| assets == open.root)
            .then(|| source_rel_if_in_project(&open.source_abs, &open.root))
            .flatten()
            .map(|rel| (open.source_abs.clone(), rel));
        let worktree_rel = project_root
            .as_deref()
            .and_then(|project_root| worktree_rel_for(project_root, &dest_root, &til_rel));
        let imported = Imported {
            asset_rel: til_rel,
            worktree_rel,
            tile_count,
        };
        self.status = None;
        self.last_import = Some(imported.clone());
        cx.notify();
        Some((imported, source))
    }

    /// Offer to delete the source PNG now that its tiles live in a `.til`.
    ///
    /// ggo-ide offered this because the packer collides on a same-stem PNG;
    /// the offer is a confirm, never automatic, and a declined or failed
    /// delete leaves a successful import successful.
    async fn offer_source_delete(
        this: &WeakEntity<Self>,
        abs: PathBuf,
        rel: String,
        cx: &mut gpui::AsyncWindowContext,
    ) {
        // `cx.update` (the AsyncWindowContext's OWN window), NOT
        // `this.update_in`: `WeakEntity::update_in` resolves the window from
        // the ENTITY's associated window, and a panel entity built outside a
        // window update has none -- the prompt would silently never be
        // raised and the offer would read as "declined". The window this task
        // was spawned in is the one that must show the prompt anyway.
        let Ok(confirm) = cx.update(|window, cx| {
            ggo_common::confirm_destructive(
                &format!("Delete the source PNG {rel}?"),
                "Delete",
                false,
                window,
                cx,
            )
        }) else {
            return;
        };
        if !confirm.await {
            return;
        }
        let removed = std::fs::remove_file(&abs);
        this.update(cx, |this, cx| {
            match removed {
                Ok(()) => {
                    // The open document's file is gone: keeping the crop
                    // surface would offer gestures over a source that can no
                    // longer be re-read. What was WRITTEN survives the
                    // transition -- `last_import` lives on the panel, not on
                    // the doc that just went away.
                    this.state = ViewerState::Empty;
                }
                Err(e) => {
                    // No toast surface yet (F5.2 owns notifications), but a
                    // silent no-op would be indistinguishable from a bug.
                    log::error!("GGO: failed to delete source PNG {rel}: {e}");
                    this.status = Some(format!("Imported, but deleting {rel} failed: {e}"));
                }
            }
            cx.notify();
        })
        .ok();
    }

    // ------------------------------------------------------------- render

    fn render_message(&self, message: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .justify_center()
            .items_center()
            .gap_1()
            .child(Label::new(message).color(Color::Muted))
            .children(self.import_summary().map(|summary| {
                Label::new(summary)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .children(self.status.clone().map(|status| {
                Label::new(status)
                    .size(LabelSize::XSmall)
                    .color(Color::Warning)
            }))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }

    /// Source path, source size, crop readout, zoom pair.
    fn render_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_header is only called in the Ready state");
        };
        let crop = open.crop();
        let summary = if open.wizard.region.is_some() {
            format!(
                "{}x{} px · crop {}x{} at ({}, {})",
                open.wizard.src_w, open.wizard.src_h, crop.w, crop.h, crop.x, crop.y
            )
        } else {
            format!(
                "{}x{} px · whole image",
                open.wizard.src_w, open.wizard.src_h
            )
        };
        v_flex()
            .gap_0p5()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(open.source_rel.clone()).size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new(summary)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("ggo-import-clear-crop", "Clear crop")
                            .disabled(open.wizard.region.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.clear_crop(cx))),
                    )
                    .child(
                        IconButton::new("ggo-import-zoom-out", IconName::Dash)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Zoom out"))
                            .disabled(open.zoom <= geom::MIN_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| this.step_zoom(-1, cx))),
                    )
                    .child(Label::new(format!("{}x", open.zoom)).size(LabelSize::XSmall))
                    .child(
                        IconButton::new("ggo-import-zoom-in", IconName::Plus)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Zoom in"))
                            .disabled(open.zoom >= geom::MAX_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| this.step_zoom(1, cx))),
                    ),
            )
            .into_any_element()
    }

    /// The source image with the crop rect and the tile grid over it.
    /// Left-drag crops; middle-drag pans; the wheel zooms on the cursor.
    fn render_canvas(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_canvas is only called in the Ready state");
        };
        let crop = open.crop();
        let scene = CropScene {
            image: Some(open.source_image.clone()),
            w: open.wizard.src_w,
            h: open.wizard.src_h,
            pan: open.pan,
            zoom: open.zoom,
            crop,
            grid: geom::crop_grid_lines(crop),
            background: cx.theme().colors().editor_background,
            border: cx.theme().colors().border,
            accent: rgb(0xebcb8b).into(),
            grid_color: rgba(0x2a78d673).into(),
        };
        let bounds_slot = open.canvas_bounds.clone();
        let element = gpui::canvas(
            move |canvas_bounds, _window, _cx| {
                *bounds_slot.borrow_mut() = Some(canvas_bounds);
                scene
            },
            move |canvas_bounds, scene, window, _cx| paint_crop(&scene, canvas_bounds, window),
        )
        .size_full();

        div()
            .id("ggo-import-canvas")
            .size_full()
            .overflow_hidden()
            .child(element)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.crop_down(event.position, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.handle_pan_move(event, cx) {
                    return;
                }
                // The button came up outside the canvas, so no mouse-up
                // reached this element: settle the crop here rather than
                // leaving the drag armed for the next pass of the cursor.
                if event.pressed_button != Some(MouseButton::Left) {
                    this.crop_up(cx);
                    return;
                }
                this.crop_move(event.position, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _window, cx| this.crop_up(cx)),
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
                    .ready()
                    .and_then(|open| open.canvas_local(event.position))
                else {
                    return;
                };
                this.zoom_at_cursor(if dy > 0.0 { 1 } else { -1 }, local, cx);
            }))
            .into_any_element()
    }

    /// The quantized preview -- the pixels the commit will actually write,
    /// drawn through the palette quantization derived.
    fn render_preview(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_preview is only called in the Ready state");
        };
        let body = match (&open.preview_image, &open.wizard.preview) {
            (Some(image), Some(preview)) => div()
                .p_1()
                .child(
                    img(image.clone())
                        .w(px(preview.w as f32))
                        .h(px(preview.h as f32)),
                )
                .into_any_element(),
            _ => Label::new("No preview")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
        };
        div()
            .id("ggo-import-preview")
            .h(PREVIEW_HEIGHT)
            .overflow_scroll()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(body)
            .into_any_element()
    }

    /// The 16 slots quantization derived. Slot 0 renders as an outlined empty
    /// box when it is the reserved transparent entry, matching how the grid
    /// draws it everywhere else in the fork.
    fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_palette is only called in the Ready state");
        };
        let Some(preview) = &open.wizard.preview else {
            return div().into_any_element();
        };
        let palette = preview.palette;
        h_flex()
            .flex_wrap()
            .gap_0p5()
            .p_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .children((0..PAL_SLOTS).map(|slot| {
                let [r, g, b, a] = slot_rgba(&palette, slot as u8);
                let color = u32::from_be_bytes([0, r, g, b]);
                div()
                    .id(("ggo-import-swatch", slot))
                    .w(px(SWATCH_PX))
                    .h(px(SWATCH_PX))
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .when(a != 0, |el| el.bg(rgb(color)))
                    .when(a == 0, |el| el.bg(rgba(0x00000000)))
                    .tooltip(ui::Tooltip::text(format!(
                        "{slot}: #{:04X}{}",
                        palette[slot],
                        if a == 0 { " (transparent)" } else { "" }
                    )))
            }))
            .into_any_element()
    }

    /// Destination, the transparent-slot toggle, the slice readout and
    /// Import.
    fn render_footer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_footer is only called in the Ready state");
        };
        let Some(fields) = &open.fields else {
            return div().into_any_element();
        };
        let crop = open.crop();
        let (cols, rows, tiles) = geom::tiles_for(crop);
        let whole = geom::whole_tiles(crop);
        let ragged = geom::is_ragged(crop);
        let readout = format!("{tiles} tiles ({cols}x{rows}) · {whole} whole");
        // The RESOLVED targets, so what is shown is what is written -- see
        // `OpenImport::dest_targets`.
        let targets = open.dest_targets().join(", ");
        let can_commit = open.wizard.can_commit();
        let reserve = open.wizard.reserve_transparent;
        let weak = cx.weak_entity();

        v_flex()
            .gap_1()
            .p_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(Self::field("Dir", 96., &fields.dir, cx))
                    .child(Self::field("Name", 96., &fields.stem, cx)),
            )
            .child(
                h_flex().gap_1().flex_wrap().items_center().child(
                    Checkbox::new("ggo-import-reserve", ToggleState::from(reserve))
                        .label("Slot 0 transparent")
                        .on_click(move |toggle, _window, cx| {
                            let on = matches!(toggle, ToggleState::Selected);
                            weak.update(cx, |this, cx| this.set_reserve_transparent(on, cx))
                                .ok();
                        }),
                ),
            )
            .child(
                Label::new(readout)
                    .size(LabelSize::XSmall)
                    .color(if ragged { Color::Warning } else { Color::Muted }),
            )
            .when(ragged, |el| {
                el.child(
                    Label::new(
                        "the crop isn't a whole number of tiles — edge tiles are zero-padded",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Warning),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new(format!("→ {targets}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("ggo-import-commit", "Import")
                            .disabled(!can_commit)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.import_impl(window, cx)),
                            ),
                    ),
            )
            .children(self.import_summary().map(|summary| {
                Label::new(summary)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .children(self.status.clone().map(|status| {
                Label::new(status)
                    .size(LabelSize::XSmall)
                    .color(Color::Warning)
            }))
            .into_any_element()
    }

    /// One labelled input, in the minimal bordered box the map panel's resize
    /// fields use (primitive gpui/ui components only -- no widget framework).
    fn field(
        label: &str,
        width: f32,
        editor: &Entity<Editor>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
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
                    .w(px(width))
                    .px_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .child(editor.clone()),
            )
            .into_any_element()
    }

    fn render_ready(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let header = self.render_header(cx);
        let canvas = self.render_canvas(cx);
        let preview = self.render_preview(cx);
        let palette = self.render_palette(cx);
        let footer = self.render_footer(cx);
        v_flex()
            .size_full()
            .child(header)
            .child(div().flex_1().min_h_0().child(canvas))
            .child(preview)
            .child(palette)
            .child(footer)
            .into_any_element()
    }
}

// --------------------------------------------------------------- painting

/// Everything the crop canvas's paint closure needs, captured at render time.
struct CropScene {
    image: Option<Arc<RenderImage>>,
    w: usize,
    h: usize,
    pan: [f32; 2],
    zoom: usize,
    crop: Region,
    /// Interior tile-divider offsets INSIDE the crop, relative to its own
    /// origin ([`geom::crop_grid_lines`]).
    grid: (Vec<usize>, Vec<usize>),
    background: Hsla,
    border: Hsla,
    accent: Hsla,
    grid_color: Hsla,
}

/// The on-screen rect of the image-space box at `(x, y, w, h)`.
fn image_rect(
    canvas: Bounds<Pixels>,
    pan: [f32; 2],
    zoom: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Bounds<Pixels> {
    let z = zoom.max(1) as f32;
    // The SIZE comes from `geom::image_pixel_size` rather than being
    // restated here, so the zoom ladder has one definition of "how big is
    // this at Nx" shared with the hit-testing that reads it back.
    let (w, h) = geom::image_pixel_size(w, h, zoom);
    bounds(
        point(
            canvas.origin.x + px(pan[0] + x as f32 * z),
            canvas.origin.y + px(pan[1] + y as f32 * z),
        ),
        size(px(w), px(h)),
    )
}

fn paint_crop(scene: &CropScene, canvas: Bounds<Pixels>, window: &mut Window) {
    window.with_content_mask(Some(ContentMask { bounds: canvas }), |window| {
        window.paint_quad(fill(canvas, scene.background));
        if scene.w == 0 || scene.h == 0 {
            return;
        }
        let image_bounds = image_rect(canvas, scene.pan, scene.zoom, 0, 0, scene.w, scene.h);
        if let Some(image) = &scene.image {
            let _ = window.paint_image(
                image_bounds,
                image_bounds,
                Corners::default(),
                image.clone(),
                0,
                false,
            );
        }
        window.paint_quad(outline(image_bounds, scene.border, BorderStyle::default()));

        let crop = scene.crop;
        let crop_bounds = image_rect(
            canvas, scene.pan, scene.zoom, crop.x, crop.y, crop.w, crop.h,
        );
        // The tile grid, drawn INSIDE the crop: these are the cuts
        // `slice_to_tiles` will make, so they are anchored on the crop's own
        // origin, not the image's.
        let z = scene.zoom.max(1) as f32;
        for x in &scene.grid.0 {
            window.paint_quad(fill(
                bounds(
                    point(
                        crop_bounds.origin.x + px(*x as f32 * z),
                        crop_bounds.origin.y,
                    ),
                    size(px(1.), crop_bounds.size.height),
                ),
                scene.grid_color,
            ));
        }
        for y in &scene.grid.1 {
            window.paint_quad(fill(
                bounds(
                    point(
                        crop_bounds.origin.x,
                        crop_bounds.origin.y + px(*y as f32 * z),
                    ),
                    size(crop_bounds.size.width, px(1.)),
                ),
                scene.grid_color,
            ));
        }
        window.paint_quad(outline(crop_bounds, scene.accent, BorderStyle::default()));
    });
}

impl Render for ImportPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_fields(window, cx);
        self.sync_dest_fields(cx);
        let body = match &self.state {
            ViewerState::Empty => self.render_message(EMPTY_MESSAGE.to_string(), cx),
            ViewerState::Loading { rel_path } => {
                self.render_message(format!("Loading {rel_path}…"), cx)
            }
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(cx),
        };
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Import, window, cx| this.import_impl(window, cx)))
            .bg(cx.theme().colors().panel_background)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for ImportPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ImportPanel {}

impl Panel for ImportPanel {
    fn persistent_name() -> &'static str {
        "GGO Import"
    }

    fn panel_key() -> &'static str {
        GGO_IMPORT_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call as every other GGO panel: no settings persistence yet,
        // and Bottom isn't a sensible spot for a canvas + form stack.
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
        // `Download` -- a downward arrow into a tray -- is upstream's own
        // glyph for "bring this in" (`agent_ui`'s thread IMPORT button uses
        // exactly it, and `extensions_ui` uses it for installing), which is
        // what this panel does. No panel uses it as a dock icon, and the
        // GGO neighbours are all taken: `Blocks` is `ggo_tileset_panel`,
        // `SquareDot` `ggo_map_panel`, `Image` `ggo_metasprite_panel`,
        // `Public` `ggo_world_panel`.
        Some(IconName::Download)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO Import")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8, `ggo_metasprite_panel` 9,
        // `ggo_charts_panel` 10, `ggo_emu_panel` 11, `ggo_tileset_panel` 12,
        // `ggo_map_panel` 14 -- which deliberately left 13 for this panel
        // (grep activation_priority across crates/).
        13
    }

    // No `prepare_to_close`: a pending import is not a document and nothing
    // it holds has ever been written -- see `open_source`'s doc.

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own update
            // (dock toggle), and `refresh_root` needs to READ the workspace
            // to find the project root -- reading it re-entrantly panics
            // (same as every other GGO panel).
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_root(cx)).ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_asset_formats::TILE_PX;
    use gpui::TestAppContext;
    use loader::{two_tone_rgba, write_png_fixture};
    use project::{FakeFs, Project, WorktreeId};
    use workspace::{AppState, MultiWorkspace};

    /// The fixture source PNG: 32x16, so at the default 16px cell it is
    /// exactly 2x1 tiles -- an unambiguous tile count -- and its red/blue
    /// split falls on the tile boundary, so a left-half crop provably
    /// changes both the tile count AND the palette.
    const SRC_W: usize = 32;
    const SRC_H: usize = 16;

    /// A real-fs emerald project: `emerald.toml` at the root, art under
    /// `assets/`.
    ///
    /// The layout is the point of the fixture, not decoration: the asset root
    /// is `<root>/assets`, so an import of `assets/art/hero.png` must write
    /// `art/hero.til` with NO `assets/` segment (the F4 `ggo-sprfix`
    /// contract).
    fn write_project(root: &Path) -> PathBuf {
        std::fs::write(root.join(EMERALD_MANIFEST), "[project]\n").unwrap();
        let assets = root.join(ASSETS_DIR);
        std::fs::create_dir_all(assets.join("art")).unwrap();
        write_png_fixture(
            &assets.join("art/hero.png"),
            SRC_W as u32,
            SRC_H as u32,
            &two_tone_rgba(SRC_W, SRC_H),
        );
        // A second PNG OUTSIDE the asset root: importable, but never offered
        // for source deletion.
        write_png_fixture(
            &root.join("art/outside.png"),
            SRC_W as u32,
            SRC_H as u32,
            &two_tone_rgba(SRC_W, SRC_H),
        );
        std::fs::write(root.join("assets/notes.txt"), "x").unwrap();
        assets
    }

    fn new_panel(cx: &mut TestAppContext, root: &Path) -> Entity<ImportPanel> {
        let root = root.to_path_buf();
        cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = ImportPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        })
    }

    /// Load the fixture PNG into a fresh panel and return it Ready.
    async fn ready_panel(cx: &mut TestAppContext, root: &Path) -> Entity<ImportPanel> {
        write_project(root);
        let panel = new_panel(cx, root);
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_source("assets/art/hero.png", cx);
        });
        cx.executor().run_until_parked();
        panel
    }

    fn ready(panel: &ImportPanel) -> &OpenImport {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
    }

    // --------------------------------------------------------- pure rules

    /// **The `ggo-sprfix` contract.** The asset root is derived ON DISK (a
    /// `.png` has no `worlds/`-style anchor in its path), and the import's
    /// rel is relative to THAT -- so a `.til` written from it can never carry
    /// an `assets/` segment. Outside an emerald project's `assets/` tree, the
    /// worktree root stands in and the rel passes through unchanged.
    #[test]
    fn split_png_path_derives_the_asset_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let assets = write_project(root);

        assert_eq!(
            split_png_path(root, "assets/art/hero.png"),
            (assets.clone(), "art/hero.png".to_string())
        );
        assert_eq!(
            split_png_path(root, "assets/hero.png"),
            (assets, "hero.png".to_string())
        );
        // Inside the project but OUTSIDE assets/: no asset root applies.
        assert_eq!(
            split_png_path(root, "art/outside.png"),
            (root.to_path_buf(), "art/outside.png".to_string())
        );
        // Not an emerald project at all.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            split_png_path(bare.path(), "a.png"),
            (bare.path().to_path_buf(), "a.png".to_string())
        );
    }

    /// The other half of the rule: the asset rel names the file for
    /// downstream binders, the worktree rel names it for the explorer and the
    /// tileset panel. Confusing the two is what the F4 bug was.
    #[test]
    fn worktree_rel_for_re_adds_the_assets_segment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let assets = write_project(root);
        assert_eq!(
            worktree_rel_for(root, &assets, "art/hero.til").as_deref(),
            Some("assets/art/hero.til")
        );
        // Asset root == worktree root (the non-emerald fallback).
        assert_eq!(
            worktree_rel_for(root, root, "art/hero.til").as_deref(),
            Some("art/hero.til")
        );
        // Unrelated roots simply don't hand off.
        let other = tempfile::tempdir().unwrap();
        assert_eq!(worktree_rel_for(other.path(), &assets, "a.til"), None);
    }

    /// **Fix round 1, BLOCKING 2.** The destination is user-editable, so the
    /// asset root is re-derived from where the bytes are ACTUALLY going. A
    /// source outside `assets/` retargeted into it must yield the asset root
    /// and a rel with no `assets/` segment -- the `ggo-sprfix` shape is
    /// precisely what the old source-only derivation produced.
    #[test]
    fn resolve_dest_re_derives_the_asset_root_from_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let assets = write_project(root);

        // Worktree-rooted (an out-of-assets source), retargeted INTO assets.
        assert_eq!(
            resolve_dest(root, "assets/tiles/outside"),
            (assets.clone(), "tiles/outside".to_string())
        );
        // Already in the source's own frame: idempotent.
        assert_eq!(
            resolve_dest(&assets, "art/hero"),
            (assets.clone(), "art/hero".to_string())
        );
        assert_eq!(resolve_dest(&assets, "hero"), (assets, "hero".to_string()));
        // Destination outside any assets/ tree: the root passes through.
        assert_eq!(
            resolve_dest(root, "art/outside"),
            (root.to_path_buf(), "art/outside".to_string())
        );
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_dest(bare.path(), "a/b"),
            (bare.path().to_path_buf(), "a/b".to_string())
        );
    }

    #[test]
    fn parent_dir_is_the_directory_part_of_a_rel() {
        assert_eq!(parent_dir("art/hero.png"), "art");
        assert_eq!(parent_dir("a/b/c.png"), "a/b");
        assert_eq!(parent_dir("hero.png"), "");
    }

    /// The collision scan lists the destination directory only, as
    /// asset-root-relative rels -- what `existing_collisions` needs to filter
    /// the commit's targets against.
    #[test]
    fn existing_rels_lists_the_destination_directory_only() {
        let dir = tempfile::tempdir().unwrap();
        let assets = write_project(dir.path());
        let rels = existing_rels(&assets, "art");
        assert_eq!(rels, vec!["art/hero.png".to_string()]);
        assert!(
            existing_rels(&assets, "").contains(&"notes.txt".to_string()),
            "an empty dir rel scans the asset root itself"
        );
        assert!(
            existing_rels(&assets, "").iter().all(|r| !r.contains('/')),
            "the scan must not recurse"
        );
        assert!(
            existing_rels(&assets, "nope").is_empty(),
            "a missing directory can't collide"
        );
    }

    #[test]
    fn overwrite_message_agrees_in_number() {
        assert_eq!(
            overwrite_message(&["a.til".to_string()]),
            "a.til already exists — overwrite?"
        );
        assert_eq!(
            overwrite_message(&["a.til".to_string(), "a.pal".to_string()]),
            "a.til, a.pal already exist — overwrite?"
        );
    }

    // ------------------------------------------------------- registration

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock. Goes through
    /// `MultiWorkspace::test_new` because `register_action` handlers are only
    /// mounted into the dispatch tree once something renders
    /// `Workspace::actions` (same lesson as the other GGO panels').
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
                workspace.panel::<ImportPanel>(cx).is_some(),
                "ImportPanel should have been added to the workspace by init()"
            );
            assert!(!workspace.right_dock().read(cx).is_open());
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<ImportPanel>(cx)
                .expect("ImportPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(workspace.right_dock().read(cx).is_open());
        });
    }

    // ---------------------------------------------------------- the flow

    /// The decode lands Ready with a live quantized preview, the destination
    /// pre-filled from the source's own asset-root-relative location, and the
    /// asset root derived (not the worktree root).
    #[gpui::test]
    async fn test_open_png_reaches_ready_with_a_preview(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);

        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/art/hero.png");
            assert_eq!(open.root, assets, "the asset root, NOT the worktree root");
            assert_eq!((open.wizard.src_w, open.wizard.src_h), (SRC_W, SRC_H));
            assert_eq!(open.wizard.mode, Mode::Tileset);
            assert_eq!(open.wizard.dest_dir, "art");
            assert_eq!(open.wizard.dest_stem, "hero");
            assert_eq!(
                open.dest_targets(),
                vec!["art/hero.til".to_string(), "art/hero.pal".to_string()],
                "targets are asset-root-relative"
            );
            let preview = open.wizard.preview.as_ref().expect("a live preview");
            assert_eq!((preview.w, preview.h), (SRC_W, SRC_H));
            assert!(open.preview_image.is_some());
            // Slot 0 is reserved transparent by default, so the two source
            // colors land in slots 1+.
            assert!(open.wizard.reserve_transparent);
            assert_eq!(
                open.crop(),
                Region {
                    x: 0,
                    y: 0,
                    w: SRC_W,
                    h: SRC_H
                }
            );
        });
    }

    #[gpui::test]
    async fn test_a_malformed_png_reports_an_error(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        std::fs::write(dir.path().join("assets/art/bad.png"), b"not a png").unwrap();
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_source("assets/art/bad.png", cx);
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _| {
            assert!(matches!(&panel.state, ViewerState::Error(_)));
        });
    }

    /// **The headline test.** A real PNG imports to a `.til`/`.pal` that
    /// round-trips through worldlib with the expected tile count, and BOTH
    /// land at ASSET-ROOT-relative paths -- `assets/art/hero.til` on disk,
    /// named `art/hero.til`. A rel with an `assets/` segment is the F4 bug.
    #[gpui::test]
    async fn test_import_writes_an_assets_root_relative_tileset(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.run_until_parked();
        // The source lives inside the asset root, so the delete offer fires;
        // decline it -- this test is about the write.
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        let imported = panel
            .read_with(cx, |panel, _| panel.last_import.clone())
            .expect("a successful import");
        assert_eq!(imported.asset_rel, "art/hero.til");
        assert!(
            !imported.asset_rel.contains(ASSETS_DIR),
            "an asset rel must never carry an assets/ segment"
        );
        assert_eq!(
            imported.worktree_rel.as_deref(),
            Some("assets/art/hero.til")
        );
        assert_eq!(
            imported.tile_count,
            (SRC_W / TILE_PX) * (SRC_H / TILE_PX),
            "32x16 at 16px tiles is 2 tiles"
        );

        assert!(assets.join("art/hero.til").is_file());
        assert!(assets.join("art/hero.pal").is_file());
        assert!(
            !dir.path().join("assets/assets").exists(),
            "nothing may be written under a doubled assets/ path"
        );

        // Round-trip through worldlib's own reader, against the ASSET root.
        let reopened = io::open_tileset(&assets, "art/hero.til").unwrap();
        assert_eq!(reopened.tile_count, imported.tile_count);
        assert_eq!(reopened.pal_path, "art/hero.pal");
        assert!(!reopened.missing_pal, "the import wrote a real .pal");
        assert_eq!(
            reopened.indices.len(),
            imported.tile_count * TILE_PX * TILE_PX
        );
        // Tile 0 is the red half, tile 1 the blue half: two different
        // non-transparent indices, so the quantization provably survived.
        let tile1 = reopened.indices[TILE_PX * TILE_PX];
        assert_ne!(reopened.indices[0], tile1);
        assert_ne!(reopened.indices[0], 0, "slot 0 is reserved transparent");

        // The source PNG is untouched by a declined delete.
        assert!(assets.join("art/hero.png").is_file());
    }

    /// A crop restricts what gets quantized and sliced: the left half of the
    /// fixture is one tile, and the blue half never reaches the sheet.
    #[gpui::test]
    async fn test_a_crop_limits_the_import_to_the_cropped_region(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();

        // Drive the crop gesture the way the canvas does, in image pixels.
        panel.update(cx, |panel, cx| {
            {
                let ViewerState::Ready(open) = &mut panel.state else {
                    panic!("expected Ready")
                };
                open.cropping = true;
                open.wizard.on_primary_down(0, 0);
                open.wizard
                    .on_moved((TILE_PX - 1) as i32, (TILE_PX - 1) as i32);
            }
            panel.crop_up(cx);
        });
        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(
                open.crop(),
                Region {
                    x: 0,
                    y: 0,
                    w: TILE_PX,
                    h: TILE_PX
                },
                "the drag is inclusive of both endpoints"
            );
            assert_eq!(
                open.wizard.preview.as_ref().map(|p| (p.w, p.h)),
                Some((TILE_PX, TILE_PX)),
                "the preview requantized on release"
            );
        });

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        let reopened = io::open_tileset(&assets, "art/hero.til").unwrap();
        assert_eq!(reopened.tile_count, 1, "only the cropped tile is written");
        assert!(
            reopened.indices.iter().all(|&i| i == reopened.indices[0]),
            "the crop covered only the solid red half"
        );
    }

    /// A commit onto existing files prompts BEFORE writing, and Cancel writes
    /// nothing -- neither the `.til` nor the `.pal`.
    #[gpui::test]
    async fn test_collision_prompt_cancel_writes_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        // Something already at the destination, with recognisable bytes.
        std::fs::write(assets.join("art/hero.til"), vec![0xAAu8; 128]).unwrap();
        std::fs::write(assets.join("art/hero.pal"), vec![0xBBu8; 32]).unwrap();
        let cx = cx.add_empty_window();

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some("art/hero.til, art/hero.pal already exist — overwrite?".to_string()),
            "a colliding commit must prompt FIRST"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert_eq!(
            std::fs::read(assets.join("art/hero.til")).unwrap(),
            vec![0xAAu8; 128],
            "Cancel must not overwrite the .til"
        );
        assert_eq!(
            std::fs::read(assets.join("art/hero.pal")).unwrap(),
            vec![0xBBu8; 32],
            "Cancel must not overwrite the .pal either"
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.last_import.is_none(), "nothing was imported");
        });

        // Going through with it now overwrites both.
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.simulate_prompt_answer("Overwrite");
        cx.run_until_parked();
        cx.simulate_prompt_answer("Cancel"); // the source-delete offer
        cx.run_until_parked();
        assert_ne!(
            std::fs::read(assets.join("art/hero.til")).unwrap(),
            vec![0xAAu8; 128]
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.last_import.as_ref().map(|i| i.asset_rel.clone()),
                Some("art/hero.til".to_string())
            );
        });
    }

    /// The source-PNG cleanup: offered for a source inside the asset root,
    /// deletes it on confirm, and drops the panel back to Empty (the file it
    /// was showing is gone) while keeping the status line.
    #[gpui::test]
    async fn test_source_delete_offer_removes_the_png_on_confirm(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);
        let cx = cx.add_empty_window();

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.run_until_parked();
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some("Delete the source PNG art/hero.png?".to_string()),
            "the offer names the source ASSET-root-relative"
        );
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(!assets.join("art/hero.png").exists(), "the source is gone");
        assert!(assets.join("art/hero.til").is_file(), "the import survives");
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(panel.state, ViewerState::Empty),
                "the panel can't keep showing a deleted source"
            );
            assert!(
                panel
                    .import_summary()
                    .is_some_and(|s| s.contains("art/hero.til")),
                "what was written survives the transition"
            );
        });
    }

    /// **Fix round 1, BLOCKING 2, end to end.** A PNG outside `assets/`,
    /// retargeted by the user into the assets tree, must still be NAMED
    /// asset-root-relative. Before the destination was re-derived this wrote
    /// the file correctly but reported `assets/tiles/outside.til` -- the
    /// exact rel `ggo-sprfix` exists to repair, waiting for the first
    /// downstream consumer to persist it.
    ///
    /// It is also the only way an out-of-assets PNG can reach the assets tree
    /// at all: worldlib's `safe_join` rejects `..`, so no `../assets/...`
    /// destination could ever have worked.
    #[gpui::test]
    async fn test_retargeting_an_outside_source_into_assets_keeps_the_rel_asset_root_relative(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        let assets = dir.path().join(ASSETS_DIR);
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_source("art/outside.png", cx);
        });
        cx.executor().run_until_parked();
        let cx = cx.add_empty_window();

        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready")
            };
            assert_eq!(
                open.root.as_path(),
                dir.path(),
                "source root is the worktree"
            );
            open.wizard.set_dest_dir("assets/tiles".to_string());
            // Deliberately calling the raw, disallowed `WizardState::targets`
            // to demonstrate the `assets/`-prefixed-rel bug `dest_targets()`
            // exists to avoid -- see clippy.toml's disallowed-methods entry.
            #[allow(clippy::disallowed_methods)]
            let raw_targets = open.wizard.targets();
            assert_eq!(
                raw_targets[0], "assets/tiles/outside.til",
                "the wizard's RAW targets still carry the segment..."
            );
            assert_eq!(
                open.dest_targets(),
                vec![
                    "tiles/outside.til".to_string(),
                    "tiles/outside.pal".to_string()
                ],
                "...and the resolved ones, which are what the panel uses, do not"
            );
        });

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "the source is outside assets/, so no delete offer"
        );

        let imported = panel
            .read_with(cx, |panel, _| panel.last_import.clone())
            .expect("a successful import");
        assert_eq!(imported.asset_rel, "tiles/outside.til");
        assert!(
            !imported.asset_rel.contains(ASSETS_DIR),
            "a retargeted import must not name itself assets/..."
        );
        assert_eq!(
            imported.worktree_rel.as_deref(),
            Some("assets/tiles/outside.til")
        );
        assert!(assets.join("tiles/outside.til").is_file());
        assert!(assets.join("tiles/outside.pal").is_file());
        assert!(
            !dir.path().join("assets/assets").exists(),
            "and nothing lands under a doubled assets/ path"
        );
        // The rel is right FROM THE ASSET ROOT, which is the frame every
        // downstream binder resolves in.
        assert_eq!(
            io::open_tileset(&assets, "tiles/outside.til")
                .unwrap()
                .tile_count,
            imported.tile_count
        );
    }

    /// A source OUTSIDE the asset root is never offered for deletion -- it
    /// was never going to collide with the packer, which is the only reason
    /// the offer exists (ggo-ide's `assetsPrefix` guard).
    #[gpui::test]
    async fn test_a_source_outside_the_asset_root_is_never_offered_for_deletion(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path());
        let panel = new_panel(cx, dir.path());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_source("art/outside.png", cx);
        });
        cx.executor().run_until_parked();
        let cx = cx.add_empty_window();

        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(
                open.root.as_path(),
                dir.path(),
                "outside assets/, the worktree root stands in"
            );
        });

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "no collision, and no delete offer for an out-of-assets source"
        );
        assert!(dir.path().join("art/outside.png").is_file());
        assert!(dir.path().join("art/outside.til").is_file());
    }

    /// A blank destination name can't commit -- the button is disabled and
    /// the action is inert, so nothing lands at a bare `.til`.
    #[gpui::test]
    async fn test_blanking_the_name_field_refuses_the_commit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        // The form fields are real `Editor`s, which need the settings store.
        cx.update(|cx| {
            AppState::test(cx);
        });
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        // Through the REAL field path: build the form fields, blank the name
        // editor, and let `import_impl`'s own `sync_dest_fields` read it back
        // (fix round 1, FOLD IN 6 -- this used to poke the wizard directly
        // while `fields` was None, so the editor->wizard sync it claimed to
        // cover never ran).
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.ensure_fields(window, cx)));
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                let stem = ready(panel)
                    .fields
                    .as_ref()
                    .expect("fields built")
                    .stem
                    .clone();
                stem.update(cx, |editor, cx| editor.set_text("   ", window, cx));
            })
        });
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.run_until_parked();

        assert!(
            !cx.has_pending_prompt(),
            "a refused commit prompts about nothing"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                ready(panel).wizard.dest_stem,
                "   ",
                "the blanked field reached the wizard"
            );
            assert!(!ready(panel).wizard.can_commit());
            assert!(panel.last_import.is_none());
            assert!(panel.status.is_some(), "and the refusal is said out loud");
        });
        assert!(!dir.path().join("assets/art/.til").exists());
    }

    // ---------------------------------------------- explorer-driven entry

    /// A workspace with `init()` run -- so the REAL contributor is in the
    /// registry -- over a worktree rooted at the REAL temp project.
    async fn routed_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Workspace>,
        Entity<ImportPanel>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        write_project(root);
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
            // The handoff target. Registering it here is also what proves
            // the two panels can coexist in one workspace.
            ggo_tileset_panel::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            serde_json::json!({
                "emerald.toml": "",
                "art": { "outside.png": "" },
                "assets": { "art": { "hero.png": "" }, "notes.txt": "" },
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
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<ImportPanel>(cx)
                .expect("init() adds the panel")
        });
        // The tileset panel needs NO root override: the fake worktree is
        // rooted at the REAL temp path, so its own `refresh_root` resolves to
        // the same directory the fixture wrote into.
        panel.update(cx, |panel, _| {
            panel.root_override = Some(root.to_path_buf())
        });
        (workspace, panel, worktree_id, cx)
    }

    fn project_path(worktree_id: WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    /// The entry is offered for `.png` FILES and nothing else -- not for
    /// other extensions, not for a directory that happens to be named
    /// `*.png`.
    #[gpui::test]
    async fn test_context_menu_offers_import_only_for_png(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, worktree_id, cx) = routed_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };
        assert_eq!(contributed("assets/art/hero.png", false, cx), 1);
        assert_eq!(
            contributed("art/outside.png", false, cx),
            1,
            "a source outside assets/ is still importable"
        );
        assert_eq!(contributed("assets/notes.txt", false, cx), 0);
        assert_eq!(contributed("emerald.toml", false, cx), 0);
        assert_eq!(
            contributed("assets/art", true, cx),
            0,
            "Import is a file action"
        );
        assert_eq!(
            contributed("assets/art/hero.png", true, cx),
            0,
            "a directory named like a PNG is still a directory"
        );
    }

    /// A path outside the primary worktree is declined -- the same
    /// `rel_in_primary_worktree` gate that declines a NON-LOCAL project (an
    /// SSH remote or a collab guest, where this panel's `std::fs` reads name
    /// a directory that doesn't exist on this machine). Both take the exact
    /// same branch; a second worktree is the half that can be built in a
    /// test.
    #[gpui::test]
    async fn test_context_menu_declines_outside_the_primary_worktree(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, worktree_id, cx) = routed_workspace(cx, dir.path()).await;
        let second = tempfile::tempdir().unwrap();
        let second_id = workspace
            .update(cx, |workspace, cx| {
                workspace.project().update(cx, |project, cx| {
                    project.create_worktree(second.path(), true, cx)
                })
            })
            .await
            .unwrap()
            .read_with(cx, |worktree, _| worktree.id());
        assert_ne!(second_id, worktree_id);

        let contributed = workspace.update_in(cx, |workspace, window, cx| {
            workspace
                .context_menu_contributions(&project_path(second_id, "hero.png"), false, window, cx)
                .len()
        });
        assert_eq!(
            contributed, 0,
            "only the primary worktree's paths are claimed"
        );
    }

    /// The entry's OWN handler loads the clicked PNG into this panel, and a
    /// committed import then opens the result in `ggo_tileset_panel` -- the
    /// "see what you just made" handoff, which is the one place the
    /// asset-rel/worktree-rel distinction is load-bearing at runtime.
    #[gpui::test]
    async fn test_menu_handler_loads_the_png_and_the_commit_opens_the_tileset_panel(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _, cx) = routed_workspace(cx, dir.path()).await;

        let handler = import_png_handler(workspace.downgrade(), "assets/art/hero.png".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/art/hero.png");
        });

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.import_impl(window, cx)));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Cancel"); // keep the source
        cx.run_until_parked();

        let tileset = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<TilesetPanel>(cx))
            .expect("the tileset panel is docked");
        assert_eq!(
            tileset.read_with(cx, |tileset, _| tileset
                .open_rel_path_now()
                .map(str::to_string)),
            Some("assets/art/hero.til".to_string()),
            "the handoff uses the WORKTREE rel, not the asset rel -- the asset \
             rel `art/hero.til` names no file in the worktree"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.right_dock().read(cx).is_open());
        });
    }
}
