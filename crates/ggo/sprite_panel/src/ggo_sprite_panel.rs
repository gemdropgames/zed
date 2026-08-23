//! GGO Sprite panel: authoring for BOTH usages of the one `.spr` format
//! -- a *sprite* (a single frame) and a *metasprite* (clip definitions
//! over several frames). Hence the crate's name; it was
//! `ggo_metasprite_panel` through F5.1, which understated half of what it
//! does (spec Naming, F5.2 task S2).
//!
//! Frame strip, playback preview, animation EDITING (clip CRUD, frame
//! ops, undo/redo/save over worldlib's `SpriteDocStore`), per-cell tile
//! assignment, and the hardware budget line (F2 tasks M4-M6); the tile
//! PICKER those assignments draw from, from-scratch creation of either
//! usage, and rename (F5.2 task S2 -- together, the step that ends the
//! pipeline's dependence on ggo-ide for sprite creation).
//!
//! Structural mirror of `ggo_world_panel` -- `Panel` impl,
//! keybinding-reload observer, off-thread loading with a load-generation
//! guard, blur/Enter-committed single-line editors -- with the
//! sprite-specific pieces split out: `loader` owns everything off the UI
//! thread (`.spr` open + per-frame compose + the picker sheet),
//! `playback` owns the pure range/loop/offset/fit math, `edits` owns the
//! pure edit rules (new-clip defaults, range validation, duration
//! parsing, post-op selection bookkeeping), `tiles` owns the preview and
//! picker hit math and the hw meter line; this module owns the panel
//! entity, the store wiring, the transport timer loop, and all gpui
//! glue. Op semantics mirror ggo-ide's `sprites/timeline.rs` message
//! handlers; guards are still re-checked here BEFORE apply -- the store
//! rejects out-of-range indices with a `DocError` these days (worldlib
//! DocOp hardening, ggo PR #73) rather than panicking, but a stale-index
//! click is a UI race to swallow silently, not an error to surface.
//!
//! Which sprite is open is driven ENTIRELY by the file explorer (F4 X1):
//! clicking a `.spr` there routes here through [`intercept_sprite_open`],
//! and the project panel's context menu routes the file ops and the two
//! "New …" entries here as well ([`contribute_sprite_menu`]); the panel
//! has no picker of its own. The two entries that need input a
//! `window.prompt` cannot collect -- which tileset to bind, and the typed
//! new name -- raise a [`PanelForm`] here rather than a dialog, per the
//! spec's rule that forms live in the panel that owns the domain.

mod editor_meta;
mod edits;
mod loader;
mod sprite_item;
mod onion;
mod playback;
mod tiles;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::{Editor, EditorEvent};
use gpui::{
    App, Bounds, Context, Entity, EntityId, FocusHandle, Focusable,
    IntoElement, KeyBinding, KeyContext, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Render, RenderImage, Styled, Subscription, Task, WeakEntity, Window, actions, div, img, px,
};
use project::ProjectPath;
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, DropdownMenu, ToggleState};
use workspace::Workspace;

use ggo_worldlib::sprites::cow::{ClipEdit, FrameTransform, SpriteState};
use ggo_worldlib::sprites::io::{self, open_sprite, save_sprite};
use ggo_worldlib::sprites::sprite_doc::{
    Anchor, DocOp, MAX_SPRITE_TILES, MIN_SPRITE_TILES, SpriteDocStore, blank_sprite_state,
    clamp_clip_name_bytes,
};
use ggo_worldlib::sprites::tileset_doc::pack_indices_to_til;
use ggo_worldlib::sprites::timeline_ops::{playback_frame_at, playback_total_ms};

actions!(
    ggo_sprite,
    [
        /// Toggles playback of the open sprite's active clip range.
        PlayPause,
        /// Undoes the last edit to the open sprite.
        Undo,
        /// Redoes the last undone edit to the open sprite.
        Redo,
        /// Saves the open sprite to its `.spr`/`.til`/`.pal` trio.
        Save,
        /// Commits the focused clip/duration field editor's text.
        CommitField,
        /// Deselects the active pool tile, so preview clicks stop
        /// assigning it to cells.
        DeselectTile
    ]
);


/// The panel's key-dispatch context identifier. [`dispatch_context`]
/// additionally stamps `editing`/`not_editing` (project_panel's pattern)
/// so plain-key bindings (space) can be scoped away from focused text
/// editors -- see [`bind_panel_keys`].
///
/// [`dispatch_context`]: SpritePanel::dispatch_context
const KEY_CONTEXT: &str = "GgoSpritePanel";


/// Frame-strip thumbnail box (px, square -- frames fit inside it via
/// `playback::fit_size`).
const THUMB_PX: f32 = 48.0;

/// Large center preview box (px, square).
const PREVIEW_PX: f32 = 240.0;

/// One tile-picker cell's on-screen edge (px, square -- pool tiles are
/// always `TILE_PX` square, so the sheet only needs a uniform scale, no
/// fit math). Also the unit `tiles::picker_tile_at` divides a click by.
const PICKER_CELL_PX: f32 = 24.0;

/// Upper bound for the picker-cols stepper -- wide enough to mirror any
/// plausible source sheet, small enough that the side column stays usable.
const MAX_PICKER_COLS: usize = 32;

/// The tile picker column's width: [`loader::PICKER_COLS`] cells plus the
/// column's own padding.
const PICKER_WIDTH: Pixels = px(PICKER_CELL_PX * loader::PICKER_COLS as f32 + 12.);

/// The clip-CRUD side column's width.
const CLIPS_WIDTH: Pixels = px(148.);

/// Playback timer cadence. 16ms tracks a 60Hz frame; the ACTUAL frame
/// shown each tick is recomputed from wall-clock elapsed time
/// (`playback_frame_at`), so a late tick skips ahead rather than
/// slowing playback down.
const TICK: Duration = Duration::from_millis(16);

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as `ggo_world_panel::init`: `zed::reload_keymaps` clears
    // and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), and keymap assets are upstream files
    // this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` keeps the panel's bindings alive across
    // reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    // Explorer-driven routing: clicking a `.spr` in the project panel loads
    // it HERE instead of opening a (binary, unreadable) editor tab. This is
    // the panel's only way in -- there is no in-panel file picker.
    workspace::register_path_open_interceptor(cx, intercept_sprite_open);

    // Right-clicking that same `.spr` offers the sprite file ops upstream's
    // menu can't: Duplicate (which has to rewrite the copy's sidecar rels,
    // not just copy bytes) and Delete.
    workspace::register_context_menu_contributor(cx, contribute_sprite_menu);

}

/// The sprite extension this panel claims from the file explorer.
const SPRITE_EXT: &str = "spr";

/// Empty-state text. The panel has no picker of its own by design (F4 X1):
/// sprites arrive by clicking a `.spr` in the project panel.
const EMPTY_MESSAGE: &str = "Open a .spr file from the project panel";

/// The assets subdirectory hanging off an emerald project root. Hardcoded
/// upstream -- it is NOT a configurable `emerald.toml` key. The
/// project-root walk itself is `ggo_common::emerald_project_root`.
const ASSETS_DIR: &str = "assets";

/// Walk up from the directory `dir` (inclusive) to the nearest emerald
/// project root (`ggo_common::emerald_project_root`), returning that
/// project's `assets/` dir.
fn asset_root_of_dir(dir: &Path) -> Option<PathBuf> {
    let assets = ggo_common::emerald_project_root(dir)?.join(ASSETS_DIR);
    assets.is_dir().then_some(assets)
}

/// [`asset_root_of_dir`] for a FILE: start the walk at its parent.
fn emerald_asset_root(start: &Path) -> Option<PathBuf> {
    asset_root_of_dir(start.parent()?)
}

/// Is `dir` the asset root of an emerald project, or a directory under it?
/// The gate on "New Sprite…"/"New Metasprite…", for the same reason
/// `ggo_map_panel` gates "New Map…" on it: the asset root is the frame a
/// `.spr`'s `til_path`/`pal_path` resolve in, so a sprite written outside
/// that tree could not name its tileset correctly (the F4 `ggo-sprfix`
/// contract).
fn is_assets_dir(dir: &Path) -> bool {
    asset_root_of_dir(dir).is_some_and(|assets| dir.starts_with(&assets))
}

/// The asset root a `.spr` resolves its `til_path`/`pal_path` against, plus
/// the sprite's path relative to THAT root.
///
/// Emerald treats those sidecar rels as **asset-root-relative**, where the
/// asset root is `<project>/assets`: `crates/cli/src/commands/pack.rs:42`
/// packs `project.root.join("assets")`, `crates/assets/src/lib.rs:398` strips
/// that dir off every asset path, and `:244`/`:308` derive `til_path`/
/// `pal_path` from the *stripped* name -- so a correct sidecar rel never
/// carries an `assets/` segment.
///
/// Clicking `<wilds>/assets/hh.spr` therefore has to yield root
/// `<wilds>/assets` and rel `hh.spr`, so a save writes `hh.til`/`hh.pal`.
/// Treating the WORKTREE root as the asset root is precisely what wrote
/// `assets/hh.til` into `hh.spr` in the first place.
///
/// This is the sprite analog of world_panel's `split_world_path`, but the
/// rule differs in kind: a world announces its root with a literal `worlds/`
/// path component, so that split is pure string work, whereas a sprite's
/// root is only discoverable on disk.
///
/// Falls back to `(project_root, rel)` when the file isn't inside an emerald
/// project's `assets/` tree -- a bare `.spr` in a non-emerald worktree keeps
/// resolving exactly as it does today.
fn split_sprite_path(project_root: &Path, rel: &str) -> (PathBuf, String) {
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

/// Does `path` name a sprite? The one rule, shared by the open interceptor
/// and the context-menu contributor so a file that routes into this panel
/// is exactly a file whose menu offers this panel's ops.
fn is_sprite_path(path: &ProjectPath) -> bool {
    path.path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(SPRITE_EXT))
}

/// `workspace::PathOpenInterceptor` for `*.spr`: claim the path, open the
/// panel, and load it. Declines (so the normal open path runs) for any other
/// file, for a path outside the primary worktree, and when no panel is
/// docked.
fn intercept_sprite_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !is_sprite_path(path) {
        return false;
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    open_sprite_item(workspace, rel, window, cx);
    true
}

/// Open (or focus) the center-pane sprite tab for worktree-relative `rel`
/// -- one item per file, activate on re-open. Public: the world panel's
/// Sprite-goto and the import panel's post-import handoff land here.
pub fn open_sprite_item(
    workspace: &mut Workspace,
    rel: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<sprite_item::SpriteEditorItem>(cx)
        .find(|item| item.read(cx).rel() == rel);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let item = cx.new(|cx| sprite_item::SpriteEditorItem::new(rel, weak, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

/// `workspace::ContextMenuContributor` for `*.spr`: the sprite file ops the
/// project panel's own menu can't offer.
///
/// **Duplicate** is here rather than upstream's generic "Duplicate" because
/// a `.spr` is not a self-contained file: it stores the asset-root-relative
/// rels of its `.til` tileset and `.pal` palette (spec Domain model -- the
/// `.spr` is the container, the pixels live in the sidecars). Copying the
/// bytes alone would produce a second sprite pointing at the FIRST one's
/// sidecars, which silently flips the original into `pool_shared` mode and
/// makes either sprite's save rewrite the other's tiles. See
/// [`SpritePanel::duplicate_sprite`].
///
/// **Rename** routes to the panel rather than doing the work here: it
/// needs text entry and `window.prompt` has no text field, so the typed
/// name is collected by [`SpritePanel::begin_rename`]'s form (the spec's
/// rule -- forms live in panels, the menu only routes). It was deferred
/// out of F5.0/G2 for exactly that missing surface.
///
/// On an assets DIRECTORY the menu instead offers **New Sprite…** and
/// **New Metasprite…** -- the same one-format-two-usages split the spec's
/// domain model draws (a sprite is one frame; a metasprite is clip
/// definitions over several).
///
/// MUST NOT touch any panel: contributors run while `ProjectPanel` is
/// leased (see `Workspace::context_menu_contributions`). All panel work is
/// deferred into the handlers, which run after the lease is released.
/// (The `is_file`/`is_dir` stats [`is_assets_dir`] makes are not panel
/// work and are legal here, same as in `ggo_map_panel`.)
fn contribute_sprite_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    if is_dir {
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
        return vec![
            ui::ContextMenuEntry::new("New Sprite…")
                .icon(ui::IconName::Plus)
                .handler(new_sprite_handler(
                    cx.weak_entity(),
                    path.worktree_id,
                    NewKind::Sprite,
                    rel.clone(),
                    dir_abs.clone(),
                ))
                .into(),
            ui::ContextMenuEntry::new("New Metasprite…")
                .icon(ui::IconName::Plus)
                .handler(new_sprite_handler(
                    cx.weak_entity(),
                    path.worktree_id,
                    NewKind::Metasprite,
                    rel,
                    dir_abs,
                ))
                .into(),
        ];
    }
    if !is_sprite_path(path) {
        return Vec::new();
    }
    vec![
        ui::ContextMenuEntry::new("Duplicate Sprite")
            .icon(ui::IconName::Copy)
            .handler(duplicate_sprite_handler(cx.weak_entity(), rel.clone()))
            .into(),
        ui::ContextMenuEntry::new("Rename Sprite…")
            .icon(ui::IconName::Pencil)
            .handler(rename_sprite_handler(cx.weak_entity(), rel.clone()))
            .into(),
        ui::ContextMenuEntry::new("Delete Sprite")
            .icon(ui::IconName::Trash)
            .handler(delete_sprite_handler(cx.weak_entity(), rel))
            .into(),
    ]
}

/// The "New Sprite…"/"New Metasprite…" entries' handler: seed the project
/// panel's inline name editor (New File's UX) in the clicked directory;
/// the commit reveals the sprite panel with the tileset-binding form,
/// name already fixed. Named for the same reason as
/// [`duplicate_sprite_handler`].
fn new_sprite_handler(
    workspace: WeakEntity<Workspace>,
    worktree_id: project::WorktreeId,
    kind: NewKind,
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
                    new_sprite_validate(dir_abs),
                    new_sprite_commit(workspace, kind, dir_rel.clone()),
                    window,
                    cx,
                );
            });
        },
    )
}

/// The inline sprite-name gate: [`new_sprite_rel`]'s stem rules plus the
/// already-exists refusal, surfaced while typing.
fn new_sprite_validate(dir_abs: PathBuf) -> impl Fn(&str) -> Option<String> + 'static {
    move |typed| match new_sprite_rel("", typed) {
        Err(error) => Some(error),
        Ok(file) => dir_abs
            .join(&file)
            .exists()
            .then(|| format!("{file} already exists here.")),
    }
}

/// The inline sprite commit: reveal + focus the sprite panel and open its
/// tileset-binding form with the typed name fixed -- the binding still
/// needs choosing, see [`create_sprite`] on why.
fn new_sprite_commit(
    workspace: WeakEntity<Workspace>,
    kind: NewKind,
    dir_rel: String,
) -> impl FnOnce(String, &mut Window, &mut App) + 'static {
    move |typed, window, cx| {
        (sprite_item_entry_handler(workspace, None, move |panel, window, cx| {
            let typed = typed.clone();
            let dir_rel = dir_rel.clone();
            panel.update(cx, |panel, cx| {
                panel.new_sprite_named(kind, &dir_rel, typed, window, cx)
            });
        }))(window, cx);
    }
}

/// Find-or-open the [`sprite_item::SpriteEditorItem`] for `target_rel` and
/// run `f` on its inner panel -- the item-era replacement for
/// `ggo_common::panel_entry_handler` (which reveals a dock panel that no
/// longer exists). `target_rel: None` always creates a fresh EMPTY item
/// (the "New Sprite…" form's host -- its document doesn't exist yet).
fn sprite_item_entry_handler(
    workspace: WeakEntity<Workspace>,
    target_rel: Option<String>,
    f: impl Fn(&Entity<SpritePanel>, &mut Window, &mut App) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        // Find/create inside the workspace update, but run `f` (and the
        // root refresh) OUTSIDE it: panel ops read the workspace entity,
        // which panics re-entrantly while it is being updated.
        let panel = workspace.update(cx, |workspace, cx| {
            let existing = target_rel.as_ref().and_then(|rel| {
                workspace
                    .items_of_type::<sprite_item::SpriteEditorItem>(cx)
                    .find(|item| item.read(cx).rel() == *rel)
            });
            let item = match existing {
                Some(item) => {
                    workspace.activate_item(&item, true, true, window, cx);
                    item
                }
                None => {
                    let weak = workspace.weak_handle();
                    let item = match target_rel.clone() {
                        Some(rel) => cx.new(|cx| {
                            sprite_item::SpriteEditorItem::new(rel, weak, window, cx)
                        }),
                        None => cx.new(|cx| sprite_item::SpriteEditorItem::new_empty(weak, cx)),
                    };
                    workspace.add_item_to_active_pane(
                        Box::new(item.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                    item
                }
            };
            item.read(cx).panel().clone()
        });
        // A freshly created panel's root only resolves asynchronously via
        // `open_rel_path`; the op in `f` may need it right now.
        panel.update(cx, |panel, cx| panel.refresh_root(cx));
        f(&panel, window, cx);
    }
}

/// The "Rename Sprite…" entry's handler -- see [`duplicate_sprite_handler`].
fn rename_sprite_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = rel.clone();
    sprite_item_entry_handler(workspace, Some(target), move |panel, window, cx| {
        let rel = rel.clone();
        panel.update(cx, |panel, cx| panel.begin_rename(rel, window, cx));
    })
}

/// The "Duplicate Sprite" entry's handler. Split out from
/// [`contribute_sprite_menu`] so a test can invoke exactly what the menu
/// invokes -- `ContextMenuEntry` keeps its handler private, so a
/// contributed entry cannot be fired from a test any other way.
fn duplicate_sprite_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = rel.clone();
    sprite_item_entry_handler(workspace, Some(target), move |panel, _window, cx| {
        let rel = rel.clone();
        panel.update(cx, |panel, cx| panel.duplicate_sprite(&rel, cx));
    })
}

/// The "Delete Sprite" entry's handler -- see [`duplicate_sprite_handler`]
/// for why it is a named function.
fn delete_sprite_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = rel.clone();
    sprite_item_entry_handler(workspace, Some(target), move |panel, window, cx| {
        let rel = rel.clone();
        panel
            .update(cx, |panel, cx| panel.delete_sprite(rel, window, cx))
            .detach();
    })
}

/// The extensions a duplicated sprite claims: the `.spr` itself and the two
/// sidecars [`save_sprite`] writes beside it. A candidate name is free only
/// when ALL THREE are, so a duplicate can never land on a name whose
/// `.til`/`.pal` would clobber another sprite's.
const SPRITE_TRIO_EXTS: [&str; 3] = [SPRITE_EXT, "til", "pal"];

/// The first free `-copy` name for `base` (an extension-less rel):
/// `hero` -> `hero-copy`, then `hero-copy-2`, `hero-copy-3`, ... `taken`
/// answers whether a candidate is already in use. Pure, so the naming rule
/// is testable without a filesystem.
fn free_copy_base(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let first = format!("{base}-copy");
    if !taken(&first) {
        return first;
    }
    (2u32..)
        .map(|n| format!("{base}-copy-{n}"))
        .find(|candidate| !taken(candidate))
        .expect("a free -copy-N name exists long before N overflows")
}

// ------------------------------------------------------- creating a `.spr`

/// Which usage of the ONE `.spr` format a "New …" entry seeds. Not two
/// file types (spec Domain model): a sprite is a single frame, a
/// metasprite is clip definitions over several frames, and both are
/// `SpriteState` in a `.spr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewKind {
    Sprite,
    Metasprite,
}

impl NewKind {
    fn label(self) -> &'static str {
        match self {
            NewKind::Sprite => "New Sprite",
            NewKind::Metasprite => "New Metasprite",
        }
    }
}

/// A new sprite's grid, in tiles. 2x2 `TILE_PX` tiles is the smallest
/// size that reads as a sprite rather than a single tile, and every
/// dimension is editable afterwards (`DocOp::Resize`).
const NEW_SPRITE_TILES: u8 = 2;

/// How many frames a new METASPRITE seeds. Two is the minimum that makes
/// a clip mean anything (a one-frame "animation" is a sprite), and the
/// seeded clip spans exactly this range.
const NEW_METASPRITE_FRAMES: usize = 2;

/// The seeded clip's name on a new metasprite -- `edits::default_new_clip`'s
/// own `clip{N}` scheme at N = 1, so the first clip a user adds by hand
/// continues the same sequence.
const NEW_METASPRITE_CLIP: &str = "clip1";

/// One row of the new-sprite form's tileset dropdown: a `.til` under the
/// asset root, plus **the other sprites that already bind it**
/// (`io::scan_til_sharers`).
///
/// The sharer list is not decoration. A sprite's pool IS its `.til`, and
/// `io::save_sprite` writes BOTH sidecars back unconditionally
/// (`io.rs:537-540`), so binding a new sprite to a tileset another sprite
/// owns means every later save of either one rewrites the other's tiles
/// AND palette -- and `pool_shared` then blocks `DocOp::Dedup` and
/// `DocOp::PaletteRemap` on both. That is exactly the hazard
/// [`contribute_sprite_menu`] cites as the reason Duplicate un-shares, so
/// it cannot be something a New entry falls into by DEFAULT.
///
/// Sharing is still legitimate and deliberately offered -- worldlib models
/// it on purpose -- so sprite-owned tilesets stay in the list. What
/// changes is that the default skips them
/// ([`default_tileset_choice`]) and picking one is labelled and warned
/// about.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TilesetChoice {
    /// Asset-root-relative `.til` rel -- what gets stored in the `.spr`.
    rel: String,
    /// Other `.spr` files already bound to it, asset-root-relative.
    sharers: Vec<String>,
}

impl TilesetChoice {
    /// The dropdown row's text: the rel, plus who already uses it.
    fn label(&self) -> String {
        match self.sharers.split_first() {
            None => self.rel.clone(),
            Some((first, [])) => format!("{} (used by {first})", self.rel),
            Some((first, rest)) => {
                format!("{} (used by {first} +{} more)", self.rel, rest.len())
            }
        }
    }

    /// The inline warning shown while this row is the selection, or `None`
    /// when binding it shares nothing.
    fn share_warning(&self) -> Option<String> {
        let first = self.sharers.first()?;
        let others = match self.sharers.len() {
            1 => String::new(),
            n => format!(" (+{} more)", n - 1),
        };
        Some(format!(
            "{} is already used by {first}{others} — saving either sprite \
             rewrites the other's tiles and palette, and Dedup / Palette \
             remap are blocked on both",
            self.rel
        ))
    }
}

/// Which sprites already bind which tileset, in one pass over the asset
/// root: resolved `til_rel` -> the `.spr` rels bound to it.
///
/// **Deliberately not `io::scan_til_sharers`.** That function answers a
/// different question: it gates on `MIN_SHARERS = 2` and returns EMPTY
/// below it (`io.rs:230-242`), because from an already-open sprite's point
/// of view a sole referrer -- itself -- isn't "sharing" with anyone. Here
/// the sprite doing the binding does not exist yet, so the case that
/// matters is exactly the one that function suppresses: a tileset with
/// ONE owner reads back as unshared, and binding to it is precisely what
/// would make it two. Asking it directly was this fix's first attempt and
/// it left the BLOCKING-1 default in place; the test
/// `test_new_sprite_form_defaults_to_an_unshared_tileset` is what caught it.
///
/// Best-effort, like worldlib's own scans: a `.spr` that fails to open
/// (missing or unreadable sidecars -- the `ggo-sprfix` corruption, say) is
/// skipped rather than failing the whole listing. `til_path` comes back
/// RESOLVED, so it is directly comparable to `io::list_tilesets`' rels.
///
/// Cost: one `io::open_sprite` per sprite, run ONCE when the form opens (a
/// menu action), never per render.
fn tileset_owners(root: &Path) -> HashMap<String, Vec<String>> {
    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    for spr_rel in io::list_sprites(root) {
        if let Ok(opened) = io::open_sprite(root, &spr_rel) {
            owners.entry(opened.til_path).or_default().push(spr_rel);
        }
    }
    owners
}

/// Every `.til` under `root`, each with the sprites already bound to it.
fn tileset_choices(root: &Path) -> Vec<TilesetChoice> {
    let owners = tileset_owners(root);
    io::list_tilesets(root)
        .into_iter()
        .map(|rel| {
            let sharers = owners.get(&rel).cloned().unwrap_or_default();
            TilesetChoice { rel, sharers }
        })
        .collect()
}

/// Which row a freshly opened form starts on: **the first tileset nobody
/// is using yet**, falling back to the first row when every tileset is
/// already bound by some sprite (or there are none).
///
/// The fallback is deliberately not "no selection": a user who only has
/// shared tilesets must still be able to create a sprite, and the warning
/// plus the labelled dropdown make the consequence visible. What must
/// never happen is silently defaulting onto a sprite-owned tileset -- with
/// a plain `selected: 0` over `list_tilesets`' SORTED walk, the default
/// was whichever `.til` sorted first, which in a real project is very
/// likely one a sprite already owns.
fn default_tileset_choice(choices: &[TilesetChoice]) -> usize {
    choices
        .iter()
        .position(|choice| choice.sharers.is_empty())
        .unwrap_or(0)
}

/// Write a blank `.spr` of `kind` into the worktree-relative directory
/// `dir_rel`, bound to the asset-root-relative tileset `til_rel`,
/// returning the new file's worktree-relative path.
///
/// **The new sprite is BOUND, to a tileset the user picked in the panel**
/// -- the one place this diverges from `ggo_map_panel::create_blank_map`,
/// and for a reason that comes from the format rather than from taste.
/// M2 chose *unbound* for a new `.map` because a map's cells are pool
/// indices, so a GUESSED tileset yields a file that looks authored and is
/// wrong. Every word of that applies to a `.spr` -- its frame cells are
/// pool indices and its pixels are palette indices, so a wrong tileset
/// misreads both. What does NOT carry over is the remedy: a `.map` has a
/// legal unbound representation (`til_path: ""`, `MapOp::BindTileset`
/// attaches one later), while a `.spr` has none -- `io::open_sprite`
/// hard-errors when the `.til`/`.pal` it names can't be read, so an
/// "unbound" `.spr` is a file that cannot be opened, including by this
/// panel. And a sprite's pool IS its `.til`, byte for byte, so binding is
/// also what gives the tile picker anything to pick from; an unbound
/// sprite would be a frame grid with one blank tile and no way to get
/// more, which is exactly the ggo-ide dependency this task exists to end.
///
/// So: nothing is guessed, and nothing is written until a tileset has
/// been chosen -- the choice is made in the panel's form (spec: forms
/// live in the panel, the menu only routes), and this function is only
/// reached once it has been.
///
/// **Divergence from ggo-ide**, worth stating rather than leaving silent:
/// its own New Sprite (`pages/assets/sprite.rs:125-171`) wrote a PRIVATE
/// trio -- `{stem}.spr`/`.til`/`.pal` straight out of `blank_sprite_state`
/// -- which is a third option, also bound and also unguessed, and it is
/// the one this panel CANNOT take. There, a one-blank-tile private pool is
/// a starting point because ggo-ide has a pixel editor to grow it with.
/// Here there is none by design (spec: import-only art), `ggo_tileset_panel`
/// is read-only, and `DocOp` has no pool-grow op -- so a private `.til`
/// would be one blank tile with no way to ever get a second, i.e. a sprite
/// that cannot be authored.
///
/// The pool is the tileset's own bytes, so the `save_sprite` write-back
/// to `til_rel` is byte-identical to what was read (pinned by
/// `test_new_sprite_leaves_the_bound_tileset_byte_identical`); `pal_rel`
/// comes from `open_tileset`, which derives it from the `.til` stem.
///
/// One deliberate side effect: binding to a `.til` with no readable
/// companion `.pal` adopts `open_tileset`'s synthesized 16-gray fallback
/// (`missing_pal`), and `save_sprite` then WRITES it -- so creating the
/// sprite also creates the `.pal` the tileset was missing. That is the
/// wanted outcome (a sprite must have a palette on disk, and the grays are
/// what the tileset panel already shows for that file), but it means a
/// "New Sprite" can add a file next to a tileset it did not otherwise
/// touch. Pinned by
/// `test_new_sprite_binding_a_pal_less_tileset_writes_the_fallback_palette`.
/// The worktree-relative `.spr` rel an inline-typed name lands at, or
/// `Err(message)` for a name the editor must refuse -- [`rename_target`]'s
/// stem rules, aimed at a directory instead of a sibling.
fn new_sprite_rel(dir_rel: &str, typed: &str) -> Result<String, String> {
    let typed = typed.trim();
    let stem = typed
        .strip_suffix(&format!(".{SPRITE_EXT}"))
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
    let file = format!("{stem}.{SPRITE_EXT}");
    Ok(if dir_rel.is_empty() {
        file
    } else {
        format!("{}/{file}", dir_rel.trim_end_matches('/'))
    })
}

fn create_sprite(
    project_root: &Path,
    dir_rel: &str,
    kind: NewKind,
    til_rel: &str,
    name: &str,
) -> Result<String, String> {
    let source_rel = new_sprite_rel(dir_rel, name)?;
    let (root, rel_path) = split_sprite_path(project_root, &source_rel);
    if root.join(&rel_path).exists() {
        // The inline editor pre-checks this; re-checked so a race can
        // never truncate an existing sprite.
        return Err(format!("{source_rel} already exists"));
    }

    let tileset = io::open_tileset(&root, til_rel).map_err(|e| e.to_string())?;
    if tileset.tile_count == 0 {
        return Err(format!("{til_rel} has no tiles"));
    }
    let mut state =
        blank_sprite_state(NEW_SPRITE_TILES, NEW_SPRITE_TILES).map_err(|e| e.to_string())?;
    // The pool IS the bound `.til` (that is what `open_sprite` reads into
    // it), so binding means adopting its tiles and palette wholesale --
    // every frame cell then addresses a tile that actually exists.
    state.pool = pack_indices_to_til(&tileset.indices, tileset.tile_count);
    state.tile_count = tileset.tile_count;
    state.palette = tileset.palette;
    if kind == NewKind::Metasprite {
        let first = state.frames[0].clone();
        state.frames.resize(NEW_METASPRITE_FRAMES, first);
        state.clips.push(ClipEdit {
            name: NEW_METASPRITE_CLIP.to_string(),
            from: 0,
            to: NEW_METASPRITE_FRAMES - 1,
            loop_: true,
        });
    }
    save_sprite(&root, &rel_path, &state, til_rel, &tileset.pal_path).map_err(|e| e.to_string())?;
    Ok(source_rel)
}

// ------------------------------------------------------ renaming a `.spr`

/// The worktree-relative path a rename of `source_rel` to the typed
/// `text` targets, or `Err(message)` for a name the panel must refuse.
///
/// Same-directory, name-only: a `/` would move the file, and moving a
/// `.spr` can invalidate its sidecars -- `io::resolve_sidecar`'s
/// bare-sibling fallback resolves a stored bare `hero.til` relative to the
/// `.spr`'s OWN directory, so a sprite that arrived that way stops
/// resolving the moment it leaves that directory. Staying put keeps both
/// sidecar forms (asset-root-relative and bare sibling) valid without
/// rewriting a single byte of the `.spr`.
///
/// A trailing `.spr` in the typed text is accepted and not doubled, since
/// the field is prefilled with the stem and a user may well retype the
/// extension.
fn rename_target(source_rel: &str, text: &str) -> Result<String, String> {
    let typed = text.trim();
    let stem = typed
        .strip_suffix(&format!(".{SPRITE_EXT}"))
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
    let file = format!("{stem}.{SPRITE_EXT}");
    Ok(match source_rel.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{file}"),
        None => file,
    })
}

/// The name a rename field is prefilled with: `source_rel`'s file stem.
fn rename_seed(source_rel: &str) -> String {
    let file = source_rel.rsplit('/').next().unwrap_or(source_rel);
    file.strip_suffix(&format!(".{SPRITE_EXT}"))
        .unwrap_or(file)
        .to_string()
}

fn bind_panel_keys(cx: &mut App) {
    // All scoped to the panel's key context. Space additionally requires
    // `not_editing` (the panel's dispatch context stamps `editing` while
    // any clip/duration field editor is focused, project_panel's
    // `not_editing` pattern) -- otherwise a panel-depth space binding
    // would win over a focused editor's plain-character input, since no
    // deeper binding shadows it. Ctrl-z/ctrl-s DON'T need the guard: the
    // default keymap binds them at the deeper `Editor` context, which
    // takes precedence while a field editor is focused. Enter is bound at
    // `KEY_CONTEXT > Editor` (single-line editors leave Enter unbound --
    // `editor::Newline` is `mode == full` only), committing the focused
    // field. `ToggleFocus` stays unbound, dispatched via
    // `Panel::toggle_action` / the command palette.
    cx.bind_keys([
        KeyBinding::new(
            "space",
            PlayPause,
            Some(&format!("{KEY_CONTEXT} && not_editing")),
        ),
        KeyBinding::new("ctrl-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-s", Save, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-s", Save, Some(KEY_CONTEXT)),
        KeyBinding::new(
            "enter",
            CommitField,
            Some(&format!("{KEY_CONTEXT} > Editor")),
        ),
        // Escape drops the active tile selection (the click-doesn't-
        // mutate affordance). No `not_editing` guard needed: while a
        // field editor is focused, the default keymap's deeper
        // `Editor`-context escape binding wins.
        KeyBinding::new("escape", DeselectTile, Some(KEY_CONTEXT)),
    ]);
}

// ------------------------------------------------------------- view state

/// What one panel text input edits. `Duration` deliberately carries no
/// frame index -- there is ONE duration editor, always bound to the
/// currently selected frame, so a selection change re-syncs its text
/// instead of rebuilding the editor set. The five transform fields
/// (rotation in degrees, 8.8 scales and shears as decimals) follow the
/// same single-editor rule.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditTarget {
    ClipName(usize),
    ClipFrom(usize),
    ClipTo(usize),
    Duration,
    Rot,
    ScaleX,
    ScaleY,
    ShearX,
    ShearY,
}

/// One panel text input: the target it edits and the single-line editor
/// entity backing it. Commit on blur (subscription) or Enter
/// ([`CommitField`]); the focus subscription re-renders the panel so the
/// key context's `editing` flag tracks reality.
struct EditorEntry {
    target: EditTarget,
    editor: Entity<Editor>,
    _subscriptions: [Subscription; 2],
}

/// An in-flight playback: wall-clock anchored (the shown frame is
/// recomputed from `started.elapsed()` every tick, never incremented), so
/// tick jitter can't drift the transport.
struct Playing {
    started: Instant,
    /// Elapsed-ms seed so playback starts ON the selected frame
    /// (`playback::start_offset_ms`).
    start_offset_ms: i64,
    /// The frame the transport is currently showing.
    frame: usize,
}

/// A loaded sprite: its doc store, per-frame image cache, transport
/// state, and field editors.
struct OpenSprite {
    /// The doc's path relative to [`Self::root`] -- the frame `save_sprite`
    /// and the sidecar rels all live in.
    rel_path: String,
    /// The path the user actually clicked, worktree-relative. Kept apart
    /// from [`Self::rel_path`] because they differ whenever the asset root
    /// isn't the worktree root (`assets/hh.spr` vs `hh.spr`); this is the
    /// one shown in the title and the dirty-close prompt, and the one the
    /// "already open?" guard compares.
    source_rel: String,
    /// The asset root this doc was READ from, captured at open time so a
    /// save can't land somewhere else if the worktree is repointed
    /// meanwhile (world_panel's `OpenWorld::root` idiom).
    root: PathBuf,
    /// Sidecar rels resolved at open time -- `save_sprite` writes the
    /// same trio back.
    til_path: String,
    pal_path: String,
    store: SpriteDocStore,
    /// One composed BGRA image per frame index; rebuilt wholesale after
    /// every doc mutation (see `loader::LoadedSprite::frames`).
    frames: Vec<Arc<RenderImage>>,
    /// Onion-skin ghost images, composed and tinted lazily on first paint
    /// and kept keyed by `(dist, frame idx)` -- `loader::compose_ghost`'s
    /// output is pure in both, so the same key always produces the same
    /// image and there is no reason to recompose it on every render.
    /// `RefCell` because [`Self::render_preview`] (and `ghosts`, which it
    /// calls) only ever sees `&self` -- same interior-mutability idiom as
    /// [`Self::preview_bounds`]/[`Self::picker_bounds`], just caching
    /// pixels instead of layout. Cleared alongside `frames` in
    /// `refresh_after_doc_change`: a doc mutation can change any frame's
    /// pixels, and the key doesn't carry a generation to invalidate by.
    ghost_cache: RefCell<HashMap<(i32, usize), Arc<RenderImage>>>,
    /// Transformed composes keyed by frame index, shared by the big
    /// preview and the clip sequence thumbnails
    /// (`loader::compose_transformed_frame` is pure in the doc state,
    /// so an entry is stale only after a doc mutation, which clears the
    /// map in `refresh_after_doc_change` alongside `frames`). `RefCell`
    /// for the same reason as [`Self::ghost_cache`]: filled from
    /// [`Self::frame_image`] under `&self` during render.
    transformed_frames: RefCell<HashMap<usize, Arc<RenderImage>>>,
    /// The bound tileset composed as the tile picker's sheet; same
    /// invalidation as `frames`.
    pool_strip: Option<loader::PoolStrip>,
    /// The tile picker's wrap width in tiles -- session-only (the `.til`
    /// format has no layout field), so a user can match the picker's
    /// wraparound to the source sheet's. Every recompose of `pool_strip`
    /// honors it.
    picker_cols: usize,
    /// Editor-only frame names, index-parallel to the doc's frames --
    /// persisted in the sidecar, never in the `.spr`. Kept in sync by the
    /// panel's frame ops; `refresh_after_doc_change` re-pads it to the
    /// frame count as a safety net (undo/redo of adds and deletes can
    /// shift alignment -- names are best-effort metadata, not doc state).
    frame_names: Vec<String>,
    /// The tile picker sheet's on-screen bounds, recorded at prepaint --
    /// same overlay-canvas idiom as [`Self::preview_bounds`].
    picker_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// The active selection: a 1x1 block from a plain picker click, or a
    /// larger block from a marquee drag. While `Some`, a preview-cell
    /// click stamps the block anchored there (`FrameTilesSet`); `None`
    /// means clicks don't mutate. Cleared by re-clicking a selected
    /// single tile, Escape, or a selected tile vanishing under an
    /// undo/fold-back.
    selection: Option<tiles::TileBlock>,
    /// An in-flight picker marquee: `(anchor, far)` in sheet cells,
    /// armed on mouse-down, updated while dragging, resolved into
    /// `selection` on mouse-up.
    picker_drag: Option<((usize, usize), (usize, usize))>,
    /// The Eraser tool: while on, preview clicks blank the cell instead
    /// of stamping the selection. Cleared by Escape alongside the
    /// selection.
    eraser: bool,
    /// An open per-frame settings popup: `(owning clip, anchor position)`.
    /// The frame it edits is `selected_frame` (opening it selects the
    /// frame, so the preview and the popup agree). Dismissed by
    /// click-away or Escape.
    frame_settings: Option<(usize, gpui::Point<Pixels>)>,
    /// The preview image's on-screen bounds, recorded at prepaint by the
    /// overlay canvas so the click handler can map window coords to cell
    /// hits (world_panel's `last_bounds` idiom). `None` until the first
    /// Ready-state paint.
    preview_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    selected_frame: usize,
    /// Index into the doc's clips; `None` = whole-sprite range.
    active_clip: Option<usize>,
    /// Onion-skin controls (off by default) -- see [`onion`].
    onion: onion::OnionState,
    playing: Option<Playing>,
    /// The transport's timer loop -- dropping it (new sprite selected,
    /// panel dropped) cancels playback; a finished loop leaves a spent
    /// task behind, which the next play simply replaces.
    _tick_task: Option<Task<()>>,
    /// Clip/duration field editors, rebuilt by `ensure_editors` when the
    /// target set changes.
    editors: Vec<EditorEntry>,
    /// Inline rejection from a clip-range edit: `(clip index, message)`.
    /// Cleared on the next applied op.
    clip_error: Option<(usize, String)>,
    /// A store-level op rejection (shouldn't happen -- ops are
    /// bounds-guarded before apply -- but surfaced instead of swallowed).
    op_error: Option<String>,
    save_error: Option<String>,
}

impl OpenSprite {
    fn new(
        rel_path: String,
        source_rel: String,
        root: PathBuf,
        loaded: loader::LoadedSprite,
    ) -> Self {
        OpenSprite {
            rel_path,
            source_rel,
            root,
            til_path: loaded.til_path,
            pal_path: loaded.pal_path,
            store: SpriteDocStore::new(loaded.state),
            frames: loaded.frames,
            ghost_cache: RefCell::new(HashMap::new()),
            transformed_frames: RefCell::new(HashMap::new()),
            pool_strip: loaded.pool_strip,
            picker_cols: loaded
                .meta
                .picker_cols
                .unwrap_or(loader::PICKER_COLS)
                .clamp(1, MAX_PICKER_COLS),
            frame_names: loaded.meta.frame_names,
            picker_bounds: Rc::new(RefCell::new(None)),
            selection: None,
            picker_drag: None,
            eraser: false,
            frame_settings: None,
            preview_bounds: Rc::new(RefCell::new(None)),
            selected_frame: 0,
            active_clip: None,
            onion: onion::OnionState::default(),
            playing: None,
            _tick_task: None,
            editors: Vec::new(),
            clip_error: None,
            op_error: None,
            save_error: None,
        }
    }

    /// The frame the big preview shows: the transport's while playing,
    /// the selected frame otherwise (so stopping "resets" the preview to
    /// the selection).
    fn shown_frame(&self) -> usize {
        self.playing
            .as_ref()
            .map_or(self.selected_frame, |p| p.frame)
    }

    /// The image the big preview draws: the shown frame's LEGACY compose
    /// (the same Arc the strip thumbnail uses) while its transform is
    /// identity, else the transformed compose on the doubled canvas,
    /// cached per shown frame until the next doc mutation.
    fn preview_image(&self) -> Option<Arc<RenderImage>> {
        self.frame_image(self.shown_frame())
    }

    /// The image any frame draws with (big preview and sequence thumbs
    /// alike): the LEGACY compose (the same Arc the strip thumbnail
    /// uses) while the frame's transform is identity, else the
    /// transformed compose on the doubled canvas, cached per frame
    /// until the next doc mutation.
    fn frame_image(&self, idx: usize) -> Option<Arc<RenderImage>> {
        let state = self.store.state();
        if state
            .frames
            .get(idx)
            .is_none_or(|f| f.transform.is_identity())
        {
            return self.frames.get(idx).cloned();
        }
        if let Some(image) = self.transformed_frames.borrow().get(&idx) {
            return Some(image.clone());
        }
        let image = loader::compose_transformed_frame(state, idx)?;
        self.transformed_frames
            .borrow_mut()
            .insert(idx, image.clone());
        Some(image)
    }

    /// Whether the shown frame renders through the legacy identity path
    /// -- the only case the preview's tile grid and cell-click editing
    /// are geometrically meaningful in (the transformed canvas is
    /// doubled and rotated, so cell math over it would stamp the wrong
    /// tiles).
    fn shown_frame_is_identity(&self) -> bool {
        self.store
            .state()
            .frames
            .get(self.shown_frame())
            .is_none_or(|f| f.transform.is_identity())
    }

    fn durations(&self) -> Vec<u16> {
        self.store
            .state()
            .frames
            .iter()
            .map(|f| f.duration_ms)
            .collect()
    }

    /// The onion-skin ghosts for the frame currently on screen, farthest
    /// first. Empty while the toggle is off -- and while PLAYING, which
    /// ggo-ide does not special-case but which is meaningless here: the
    /// transport already shows the neighbouring frames in sequence, and
    /// stacking ghosts under a moving image only smears it.
    fn ghosts(&self) -> Vec<onion::Ghost> {
        if self.playing.is_some() {
            return Vec::new();
        }
        let state = self.store.state();
        let clip = self.active_clip.and_then(|i| state.clips.get(i));
        self.onion
            .ghosts(self.shown_frame(), state.frames.len(), clip)
    }

    /// The tinted image for one onion-skin ghost, from
    /// [`Self::ghost_cache`] if this `(dist, idx)` was composed before,
    /// else composed via [`loader::compose_ghost`] and cached for next
    /// time. `&self` (not `&mut self`): called from [`render_preview`],
    /// which only borrows the Ready state immutably -- the cache's
    /// `RefCell` is what makes filling it in from there sound.
    fn ghost_image(&self, dist: i32, idx: usize) -> Option<Arc<RenderImage>> {
        if let Some(image) = self.ghost_cache.borrow().get(&(dist, idx)) {
            return Some(image.clone());
        }
        let image = loader::compose_ghost(self.store.state(), idx, dist)?;
        self.ghost_cache
            .borrow_mut()
            .insert((dist, idx), image.clone());
        Some(image)
    }
}

enum ViewerState {
    /// Nothing selected yet.
    Empty,
    Loading {
        rel_path: String,
    },
    Ready(Box<OpenSprite>),
    Error(String),
}

/// A modal-ish form rendered as a bar above the viewer. Both entries the
/// spec routes here need typed or picked input that a `window.prompt`
/// (button-choice only) cannot collect, which is why they are panel forms
/// rather than menu prompts.
///
/// Only one can be open at a time: they are both started from the project
/// panel's context menu, one click at a time, and a second start replaces
/// the first rather than stacking.
enum PanelForm {
    /// "New Sprite…"/"New Metasprite…": pick the tileset to bind, then
    /// create. Nothing is on disk until [`SpritePanel::confirm_new`] runs
    /// -- see [`create_sprite`] for why a binding must be chosen at all.
    New {
        kind: NewKind,
        /// The clicked directory, worktree-relative.
        dir_rel: String,
        /// The stem typed into the project panel's inline editor -- the
        /// new document's name, already validated by the editor's gate.
        name: String,
        /// Every `.til` under the asset root with its existing sharers
        /// ([`tileset_choices`]). Empty means the project has no tileset
        /// yet and the form can only be cancelled.
        tilesets: Vec<TilesetChoice>,
        selected: usize,
        error: Option<String>,
    },
    /// "Rename Sprite…": the typed name, committed by the button or
    /// Enter.
    Rename {
        /// The sprite being renamed, worktree-relative.
        source_rel: String,
        editor: Entity<Editor>,
        error: Option<String>,
    },
    /// Name a frame (editor-only metadata; double-click a strip cell).
    NameFrame {
        index: usize,
        editor: Entity<Editor>,
    },
}

pub struct SpritePanel {
    focus_handle: FocusHandle,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    state: ViewerState,
    /// The open "New …"/"Rename …" form, if any.
    form: Option<PanelForm>,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl SpritePanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            root_override: None,
            project_root: None,
            state: ViewerState::Empty,
            form: None,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree). MUST NOT run while the workspace itself is mid-update
    /// (it reads the workspace entity) -- see the deferral in `set_active`
    /// and in [`Self::open_rel_path`].
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        cx.notify();
    }

    /// Load the project-relative `.spr` path `rel`, prompting FIRST if the
    /// open sprite has unsaved edits -- Cancel leaves the current document
    /// loaded and dirty and abandons the open. This is the panel's entry
    /// point from the file explorer ([`intercept_sprite_open`]); there is no
    /// in-panel picker.
    ///
    /// Everything after the guard runs on a spawned task, deliberately: the
    /// interceptor calls this from INSIDE the workspace's own update, and
    /// [`Self::refresh_root`] has to read that same workspace entity.
    pub fn open_rel_path(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Clicking the file that is ALREADY open is how you bring the panel
        // back into focus, and upstream's semantics for that click on a tab
        // are "activate the existing item", not "reload it". The interceptor
        // has already revealed and focused the dock by the time we get here,
        // so there is nothing left to do -- and doing anything would either
        // prompt (offering a "Don't Save" the user never asked for) or drop
        // the undo stack, selection, tile choice and transport state on the
        // floor.
        if let ViewerState::Ready(open) = &self.state
            && open.source_rel == rel
        {
            return;
        }
        let rel = rel.to_string();
        let proceed = ggo_common::prepare_to_close_dirty(
            self.dirty_sprite_name(),
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
                this.load_rel_path(&rel, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Copy the sprite at worktree-relative `rel` to the first free
    /// `-copy` name, returning that copy's asset-root-relative rel (`None`
    /// if anything failed). The body of the project panel's "Duplicate
    /// Sprite" entry ([`contribute_sprite_menu`]).
    ///
    /// Goes through worldlib's own `open_sprite` -> `save_sprite` rather
    /// than `fs::copy`, because a `.spr` stores the rels of its `.til` and
    /// `.pal`: a byte copy would point the duplicate at the ORIGINAL's
    /// sidecars, which (a) flips the original into `pool_shared` the next
    /// time it is opened, changing how tile edits behave in a file the user
    /// only asked to copy FROM, and (b) makes a save of either sprite
    /// rewrite the other's tileset. Re-saving instead gives the copy its
    /// own `-copy.til`/`-copy.pal`, so the original is untouched in every
    /// observable way, and the copy is a real `.spr` by construction (it
    /// came out of the same encoder every save uses).
    ///
    /// Copies what the panel SHOWS, not what is on disk, when `rel` is the
    /// open document: duplicating a dirty sprite off its last-saved bytes
    /// would silently drop the edits the user is looking at. ggo-ide put a
    /// "Discard unsaved changes?" prompt in front of exactly this case; the
    /// live-state copy answers it without asking, and skips a read besides.
    ///
    /// No prompt: no existing file is written and no state is lost.
    /// Synchronous: at most one small read and three small writes, without
    /// the frame composition that makes an actual open worth backgrounding.
    fn duplicate_sprite(&mut self, rel: &str, cx: &mut Context<Self>) -> Option<String> {
        // `project_root` is only re-discovered on panel activation, and a
        // right-click can reach a panel that was never activated.
        self.refresh_root(cx);
        let project_root = self.project_root.clone()?;
        let (root, rel_in_root) = split_sprite_path(&project_root, rel);
        let state = match &self.state {
            ViewerState::Ready(open) if open.source_rel == rel => open.store.state().clone(),
            _ => open_sprite(&root, &rel_in_root).ok()?.state,
        };
        let base = rel_in_root
            .rsplit_once('.')
            .map_or(rel_in_root.as_str(), |(base, _)| base);
        let copy_base = free_copy_base(base, |candidate| {
            SPRITE_TRIO_EXTS
                .iter()
                .any(|ext| root.join(format!("{candidate}.{ext}")).exists())
        });
        let copy_rel = format!("{copy_base}.{SPRITE_EXT}");
        save_sprite(
            &root,
            &copy_rel,
            &state,
            &format!("{copy_base}.til"),
            &format!("{copy_base}.pal"),
        )
        .ok()?;
        Some(copy_rel)
    }

    /// Confirm, then delete the sprite at worktree-relative `rel` -- the
    /// body of the project panel's "Delete Sprite" entry
    /// ([`contribute_sprite_menu`]).
    ///
    /// Deletes the `.spr` ONLY. Its `.til`/`.pal` are shareable by design
    /// (worldlib's `scan_til_sharers`/`pool_shared` exist for exactly
    /// that), so removing them on the strength of one sprite's deletion
    /// could break sprites the user never touched; an orphaned sidecar is
    /// the recoverable half of that trade. A failed unlink leaves the panel
    /// exactly as it was rather than half-clearing it.
    ///
    /// Returns the `Task` so tests can await the whole prompt->delete round
    /// trip; the menu handler detaches it.
    fn delete_sprite(
        &mut self,
        rel: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        self.refresh_root(cx);
        let Some(project_root) = self.project_root.clone() else {
            return Task::ready(());
        };
        // Named, not offered a save: deleting the file makes an unsaved edit
        // to it moot, so this warns instead of routing through
        // `prepare_to_close_dirty` (which would offer to write bytes that
        // are about to be unlinked). ggo-ide's delete made the same call.
        let unsaved = self.dirty_sprite_name().is_some_and(|name| name == rel);
        let confirm = ggo_common::confirm_destructive(
            &format!("Delete the sprite {rel}?"),
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
                log::error!("GGO: failed to delete sprite {rel}: {e}");
                return;
            }
            this.update(cx, |this, cx| {
                // The open document's file is gone: keeping it on screen
                // would offer edits, undo and a save that target nothing.
                if matches!(&this.state, ViewerState::Ready(open) if open.source_rel == rel) {
                    this.state = ViewerState::Empty;
                    cx.notify();
                }
            })
            .ok();
        })
    }

    // ------------------------------------------------------- new / rename

    /// Open the "New Sprite…"/"New Metasprite…" form for the
    /// worktree-relative directory `dir_rel` -- the body of those two
    /// project-panel entries.
    ///
    /// **The unsaved-edits guard runs BEFORE the form opens, and the form
    /// is the only thing that writes** -- so a Cancel at either step
    /// leaves the disk untouched. `ggo_map_panel`'s "New Map…" originally
    /// created the file first and prompted afterwards, which orphaned a
    /// `map.map` on Cancel and pushed the next attempt onto `map-2.map`
    /// (M2 fix round 1, BLOCKING 2); guarding first is that lesson, and
    /// deferring the write to [`Self::confirm_new`] makes it structural
    /// here rather than a matter of statement order.
    ///
    /// Refreshes the root first for the same reason `duplicate_sprite`
    /// does: a right-click can reach a panel that was never activated.
    fn new_sprite_named(
        &mut self,
        kind: NewKind,
        dir_rel: &str,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_root(cx);
        if self.project_root.is_none() {
            return;
        }
        let dir_rel = dir_rel.to_string();
        let proceed = ggo_common::prepare_to_close_dirty(
            self.dirty_sprite_name(),
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
                // The tileset list is asset-root-relative because that is
                // the frame a `.spr` stores its `til_path` in.
                let root = asset_root_of_dir(&project_root.join(&dir_rel))
                    .unwrap_or_else(|| project_root.clone());
                let tilesets = tileset_choices(&root);
                this.form = Some(PanelForm::New {
                    kind,
                    dir_rel,
                    name,
                    selected: default_tileset_choice(&tilesets),
                    tilesets,
                    error: None,
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Pick which tileset the pending new sprite binds to.
    fn select_new_tileset(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(PanelForm::New {
            tilesets, selected, ..
        }) = &mut self.form
            && ix < tilesets.len()
        {
            *selected = ix;
            cx.notify();
        }
    }

    /// Create the pending new sprite and open it.
    ///
    /// **The unsaved-edits guard is RE-RUN here** (fix round 1, BLOCKING
    /// 2), not just at [`Self::new_sprite`]. The form bar renders ABOVE a
    /// fully live viewer, so the open document stays editable the whole
    /// time it is up -- an edit made after the form opened sat between the
    /// old guard and this replacement and was discarded without a prompt.
    /// Deferring the write out of the menu handler bought structure but
    /// also separated the guard from the thing it guards; the guard has to
    /// sit where the document is actually replaced, which is here.
    ///
    /// It costs a second prompt only when the answer to the first one was
    /// "Don't Save" -- that leaves the document dirty by design, so
    /// `dirty_sprite_name` is still `Some` and the question is genuinely
    /// live again. A "Save" answer leaves it clean and
    /// `prepare_to_close_dirty` returns ready-true without prompting.
    ///
    /// Cancel leaves the form open (nothing was written, so the user can
    /// still pick a tileset or dismiss it).
    fn confirm_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PanelForm::New {
            kind,
            dir_rel,
            name,
            tilesets,
            selected,
            ..
        }) = &self.form
        else {
            return;
        };
        let (kind, dir_rel, name) = (*kind, dir_rel.clone(), name.clone());
        let Some(til_rel) = tilesets.get(*selected).map(|choice| choice.rel.clone()) else {
            return; // no tileset in the project: the form only cancels
        };
        let proceed = ggo_common::prepare_to_close_dirty(
            self.dirty_sprite_name(),
            window,
            cx,
            Self::save_for_close,
        );
        cx.spawn(async move |this, cx| {
            if !proceed.await {
                return;
            }
            this.update(cx, |this, cx| {
                this.create_and_open(kind, &dir_rel, &til_rel, &name, cx)
            })
            .ok();
        })
        .detach();
    }

    /// [`Self::confirm_new`]'s body, once the unsaved-edits guard has
    /// resolved: the one place a new `.spr` is written.
    fn create_and_open(
        &mut self,
        kind: NewKind,
        dir_rel: &str,
        til_rel: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        match create_sprite(&project_root, dir_rel, kind, til_rel, name) {
            Ok(source_rel) => {
                self.form = None;
                self.load_rel_path(&source_rel, cx);
            }
            Err(message) => {
                log::error!("GGO: failed to create sprite in {dir_rel}: {message}");
                if let Some(PanelForm::New { error, .. }) = &mut self.form {
                    *error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// Open the rename form for the sprite at worktree-relative `rel` --
    /// the body of the project panel's "Rename Sprite…" entry, prefilled
    /// with the current stem and focused so it can be typed into
    /// immediately.
    fn begin_rename(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_root(cx);
        let seed = rename_seed(&rel);
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(seed, window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.form = Some(PanelForm::Rename {
            source_rel: rel,
            editor,
            error: None,
        });
        cx.notify();
    }

    /// Rename the `.spr` to the typed name.
    ///
    /// **Only the `.spr` moves.** Its `.til`/`.pal` keep both their names
    /// and their stored rels, which is what keeps a renamed sprite
    /// resolvable: those rels are asset-root-relative (the `ggo-sprfix`
    /// contract F4 exists to repair), so they are not affected by the
    /// `.spr`'s own name at all, and the bare-sibling fallback form is
    /// safe too because [`rename_target`] keeps the file in its
    /// directory. Renaming the sidecars WOULD require rewriting the
    /// `.spr`'s stored rels, and would break every OTHER sprite sharing
    /// that tileset -- worldlib's `scan_til_sharers`/`pool_shared` exist
    /// precisely because sharing is expected. The same call `delete_sprite`
    /// makes, for the same reason.
    ///
    /// No unsaved-edits guard: nothing is lost. The open document, if it
    /// is this file, simply follows the rename -- `source_rel` (what the
    /// title and the "already open?" check compare) and `rel_path` (what
    /// a save writes to) are both repointed, so a dirty document stays
    /// dirty and saves to the new name.
    fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(PanelForm::Rename {
            source_rel, editor, ..
        }) = &self.form
        else {
            return;
        };
        let source_rel = source_rel.clone();
        let text = editor.read(cx).text(cx);
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let target = rename_target(&source_rel, &text).and_then(|target| {
            if target == source_rel {
                // Committing the name it already has is a dismissal, not
                // an error -- and must not trip the exists check below.
                return Ok(target);
            }
            if project_root.join(&target).exists() {
                return Err(format!("{target} already exists"));
            }
            std::fs::rename(project_root.join(&source_rel), project_root.join(&target))
                .map_err(|e| e.to_string())
                .map(|()| target)
        });
        match target {
            Ok(target) => {
                if let ViewerState::Ready(open) = &mut self.state
                    && open.source_rel == source_rel
                {
                    let (_, rel_in_root) = split_sprite_path(&project_root, &target);
                    open.source_rel = target;
                    open.rel_path = rel_in_root;
                }
                self.form = None;
            }
            Err(message) => {
                log::error!("GGO: failed to rename sprite {source_rel}: {message}");
                if let Some(PanelForm::Rename { error, .. }) = &mut self.form {
                    *error = Some(message);
                }
            }
        }
        cx.notify();
    }

    /// Open the name-frame form for strip cell `index`, prefilled with
    /// the stored name (empty when unnamed -- seeding the "Frame N"
    /// fallback would commit it as a literal name).
    fn begin_name_frame(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        if index >= open.store.state().frames.len() {
            return;
        }
        let seed = open.frame_names.get(index).cloned().unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(seed, window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.form = Some(PanelForm::NameFrame { index, editor });
        cx.notify();
    }

    /// Commit the name-frame form into the sidecar-backed name list.
    fn confirm_name_frame(&mut self, cx: &mut Context<Self>) {
        let Some(PanelForm::NameFrame { index, editor }) = &self.form else {
            return;
        };
        let index = *index;
        let name = editor.read(cx).text(cx).trim().to_string();
        self.form = None;
        self.set_frame_name(index, name, cx);
        cx.notify();
    }

    /// Dismiss whichever form is open, writing nothing.
    fn cancel_form(&mut self, cx: &mut Context<Self>) -> bool {
        let had = self.form.take().is_some();
        if had {
            cx.notify();
        }
        had
    }

    /// Kick off the off-thread load of `rel`. A stale result (superseded by
    /// a later open) is dropped by generation check.
    fn load_rel_path(&mut self, rel: &str, cx: &mut Context<Self>) {
        let source_rel = rel.to_string();
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        // Resolve the asset root the sidecars inside this `.spr` are written
        // against BEFORE loading, and carry it on the open doc -- the save
        // path must use this root, not the worktree root.
        let (root, rel) = split_sprite_path(&project_root, &source_rel);
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            rel_path: source_rel.clone(),
        };
        cx.notify();

        let load = {
            let rel = rel.clone();
            let root = root.clone();
            cx.background_spawn(async move { loader::load_sprite(&root, &rel) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => {
                        ViewerState::Ready(Box::new(OpenSprite::new(rel, source_rel, root, loaded)))
                    }
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Click a strip thumbnail: select that frame (the transport, if
    /// running, keeps playing -- its own frame wins in the preview until
    /// it stops).
    fn select_frame(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && ix < open.store.state().frames.len()
        {
            open.selected_frame = ix;
            cx.notify();
        }
    }

    /// Pick the active clip (`None` = whole sprite). A running transport
    /// picks the new range up on its next tick (range/loop are re-read
    /// per tick, ggo-ide's mid-playback-edit rule).
    fn select_clip(&mut self, clip: Option<usize>, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.active_clip = clip;
            cx.notify();
        }
    }

    /// Mutate the onion-skin controls and repaint. One entry point for all
    /// four (toggle, back, forward, opacity) so every control shares the
    /// Ready guard and the notify.
    fn update_onion(&mut self, edit: impl FnOnce(&mut onion::OnionState), cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            edit(&mut open.onion);
            cx.notify();
        }
    }

    /// Play/pause toggle. Play anchors a wall clock, seeds the elapsed
    /// offset so playback starts on the selected frame (clamped into the
    /// active range), and spawns the tick loop; pause drops the loop and
    /// the transport state (the preview falls back to the selected
    /// frame).
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open.playing.is_some() {
            open.playing = None;
            open._tick_task = None;
            cx.notify();
            return;
        }
        if open.store.state().frames.is_empty() {
            return;
        }
        let durations = open.durations();
        let state = open.store.state();
        let range = playback::play_range(&state.clips, open.active_clip, state.frames.len());
        let start_offset_ms = playback::start_offset_ms(&durations, range, open.selected_frame);
        open.playing = Some(Playing {
            started: Instant::now(),
            start_offset_ms,
            frame: open
                .selected_frame
                .clamp(range.0.min(range.1), range.0.max(range.1)),
        });
        // The timer loop: sleep a tick, recompute the shown frame from
        // wall-clock elapsed, stop when a non-looping range finishes (or
        // the panel/sprite goes away). `cx.spawn` + a
        // `background_executor().timer` await is this checkout's idiom
        // for periodic UI work (gpui has no retained per-entity
        // animation-frame callback; element-level `with_animation` is a
        // fixed-duration easing wrapper, wrong shape for a
        // duration-table-driven transport).
        open._tick_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let finished = this.update(cx, |this, cx| this.advance_playback(cx));
                match finished {
                    Ok(false) => {}
                    Ok(true) | Err(_) => break,
                }
            }
        }));
        cx.notify();
    }

    /// One transport tick: re-read durations/range/loop from the doc
    /// (mid-playback edits get picked up immediately, ggo-ide's `tick`
    /// rule -- a deleted clip falls back to the whole strip via
    /// `play_range`'s stale-index rule) and recompute the shown frame.
    /// Notifies only when the shown frame actually changed (or playback
    /// stopped) -- at 16ms ticks over >= 16ms frames most ticks change
    /// nothing, and a no-op notify would re-render the whole panel at
    /// 60Hz for the duration of playback (M4 review fix). Returns true
    /// when the loop should stop: playback was cancelled, or a
    /// non-looping range ran past its total (which also resets the
    /// preview to the selected frame).
    fn advance_playback(&mut self, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
            return true;
        };
        if open.playing.is_none() {
            return true;
        }
        let state = open.store.state();
        let durations: Vec<u16> = state.frames.iter().map(|f| f.duration_ms).collect();
        let range = playback::play_range(&state.clips, open.active_clip, state.frames.len());
        let loop_ = playback::play_loop(&state.clips, open.active_clip);
        let playing = open
            .playing
            .as_mut()
            .expect("checked playing.is_some() above");
        let t_ms = playing.start_offset_ms + playing.started.elapsed().as_millis() as i64;
        if !loop_ && t_ms >= playback_total_ms(&durations, range) {
            open.playing = None;
            cx.notify();
            return true;
        }
        let frame = playback_frame_at(&durations, range, t_ms, loop_);
        if playing.frame != frame {
            playing.frame = frame;
            cx.notify();
        }
        false
    }

    // ------------------------------------------------------------ doc ops

    /// Apply one op to the store, then refresh everything derived from the
    /// doc. All callers bounds-check their indices FIRST: the store
    /// validates every index itself now (worldlib DocOp hardening, ggo
    /// PR #73 -- a bad index comes back as a `DocError`, not a panic),
    /// but a stale index from a click racing an undo is a UI no-op, not
    /// an error the user should see. A genuine store-level rejection
    /// (e.g. an over-long clip name, pre-clamped here so it shouldn't
    /// fire) is surfaced inline rather than swallowed.
    fn apply_doc(&mut self, op: DocOp, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
            return false;
        };
        match open.store.apply(op) {
            Ok(()) => {
                self.refresh_after_doc_change(cx);
                true
            }
            Err(e) => {
                open.op_error = Some(e.to_string());
                cx.notify();
                false
            }
        }
    }

    /// After any doc mutation (op, undo, redo, save fold-back): rebuild
    /// the composed per-frame images (the whole Vec -- M4's documented
    /// invalidation point; wholesale recompose keeps every thumbnail *and*
    /// the frame-index -> image mapping trivially correct after
    /// adds/deletes/moves, at O(frames x sprite px) per edit, cheap at
    /// sprite scale), clamp the frame/clip selections into the new
    /// bounds, and clear stale inline errors. A running transport needs no
    /// help: range/loop/durations are re-read from the doc every tick.
    fn refresh_after_doc_change(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let frames = loader::compose_frames(open.store.state()).unwrap_or_default();
        let pool_strip = loader::compose_pool_strip(open.store.state(), open.picker_cols);
        let frame_count = open.store.state().frames.len();
        let clip_count = open.store.state().clips.len();
        let tile_count = open.store.state().tile_count;
        open.frames = frames;
        open.ghost_cache.borrow_mut().clear();
        open.transformed_frames.borrow_mut().clear();
        open.pool_strip = pool_strip;
        open.selected_frame = open.selected_frame.min(frame_count.saturating_sub(1));
        // Undo/redo of frame adds/deletes can leave the editor-only name
        // list misaligned; re-pad to the frame count so indexing stays
        // safe (names are best-effort metadata).
        open.frame_names.resize(frame_count, String::new());
        if open.active_clip.is_some_and(|c| c >= clip_count) {
            open.active_clip = None;
        }
        if open.frame_settings.is_some_and(|(clip, _)| clip >= clip_count) {
            // The popup's owning clip vanished under an undo/delete.
            open.frame_settings = None;
        }
        if open.selection.as_ref().is_some_and(|block| {
            block
                .tiles
                .iter()
                .flatten()
                .any(|&t| t as usize >= tile_count)
        }) {
            // A selected tile went away (undo past a COW clone, dedup
            // fold-back on save) -- drop the selection rather than
            // stamping whatever tile inherits the index.
            open.selection = None;
        }
        open.clip_error = None;
        open.op_error = None;
        cx.notify();
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        let undone = match &mut self.state {
            ViewerState::Ready(open) => open.store.undo(),
            _ => false,
        };
        if undone {
            self.refresh_after_doc_change(cx);
        }
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        let redone = match &mut self.state {
            ViewerState::Ready(open) => open.store.redo(),
            _ => false,
        };
        if redone {
            self.refresh_after_doc_change(cx);
        }
    }

    /// The open sprite's display path when it has unsaved edits, else
    /// `None`.
    fn dirty_sprite_name(&self) -> Option<String> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        open.store.dirty().then(|| open.source_rel.clone())
    }

    /// Save on behalf of a "Save" answer to the unsaved-edits prompt,
    /// reporting whether the write actually landed (a failed write must not
    /// let the caller discard the document). Shared by
    /// [`Panel::prepare_to_close`] and [`Self::open_rel_path`].
    fn save_for_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_impl(cx);
        match &self.state {
            ViewerState::Ready(open) => open.save_error.is_none(),
            _ => true,
        }
    }

    /// `save_sprite` -> `set_saved_state` with its fold-back result --
    /// worldlib's own save flow (ggo-ide `editor.rs` parity: the folded
    /// state silently replaces `current` WITHOUT an undo entry, so Ctrl+Z
    /// after saving steps through the user's edits, not the fold-back).
    /// Synchronous by choice, same reasoning as `ggo_world_panel`'s save.
    fn save_impl(&mut self, cx: &mut Context<Self>) {
        let saved = {
            let ViewerState::Ready(open) = &mut self.state else {
                return;
            };
            // `open.root`, NOT `self.project_root`: the doc must be written
            // back against the asset root it was READ from, or the sidecar
            // rels get an extra `assets/` segment (see `split_sprite_path`).
            let root = open.root.clone();
            match save_sprite(
                &root,
                &open.rel_path,
                open.store.state(),
                &open.til_path,
                &open.pal_path,
            ) {
                Ok(folded) => {
                    open.store.set_saved_state(folded);
                    open.save_error = None;
                    true
                }
                Err(e) => {
                    open.save_error = Some(e.to_string());
                    false
                }
            }
        };
        if saved {
            self.write_sidecar();
            // Fold-back may have re-identified pool tiles; recompose from
            // the state the store now actually holds.
            self.refresh_after_doc_change(cx);
        } else {
            cx.notify();
        }
    }

    // --------------------------------------------------------- frame ops

    /// Append a blank frame -- ggo-ide `Msg::AddFrame` (insert at the end,
    /// no `copy_of`; the store finds or appends an all-zero tile).
    fn add_blank_frame(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let at = open.store.state().frames.len();
        // No explicit name sync: the new frame appends at the end, which
        // is exactly what `refresh_after_doc_change`'s name pad produces.
        self.apply_doc(
            DocOp::FrameAdd {
                at,
                copy_of: None,
                map: None,
            },
            cx,
        );
    }

    /// Duplicate the selected frame right after itself and select the
    /// copy -- ggo-ide `Msg::DupFrame`.
    fn duplicate_selected_frame(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let i = open.selected_frame;
        let pre_len = open.store.state().frames.len();
        if i >= pre_len {
            return;
        }
        // Snapshot BEFORE the op: `apply_doc`'s refresh re-pads the live
        // list at the tail, which is the wrong position for an insert.
        let mut names = open.frame_names.clone();
        names.resize(pre_len, String::new());
        if self.apply_doc(
            DocOp::FrameAdd {
                at: i + 1,
                copy_of: Some(i),
                map: None,
            },
            cx,
        ) && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = i + 1;
            names.insert(i + 1, names[i].clone());
            open.frame_names = names;
        }
    }

    /// Delete the selected frame -- ggo-ide `Msg::DeleteFrame`: refuses to
    /// drop the last frame; the store adjusts clip ranges in the same op;
    /// the selection follows `edits::selection_after_frame_delete`.
    fn delete_selected_frame(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let i = open.selected_frame;
        let len = open.store.state().frames.len();
        if len <= 1 || i >= len {
            return; // always keep at least one frame
        }
        let next = edits::selection_after_frame_delete(i, i, len);
        let mut names = open.frame_names.clone();
        names.resize(len, String::new());
        if self.apply_doc(DocOp::FrameDelete { at: i }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = next;
            names.remove(i);
            open.frame_names = names;
        }
    }

    /// Move the selected frame one slot left/right -- ggo-ide
    /// `Msg::MoveFrame`: for an ADJACENT move, `to` is already the correct
    /// post-removal splice index (a neighbor swap), so no drop-target
    /// conversion is needed; the selection follows the moved frame.
    fn move_selected_frame(&mut self, delta: i32, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let i = open.selected_frame;
        let len = open.store.state().frames.len();
        let to = if delta < 0 {
            i.checked_sub(1)
        } else {
            i.checked_add(1)
        };
        let Some(to) = to else {
            return;
        };
        if i >= len || to >= len {
            return;
        }
        if self.apply_doc(DocOp::FrameMove { from: i, to }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = to;
            move_name(&mut open.frame_names, i, to);
        }
    }

    /// Drop a dragged frame thumbnail onto strip position `to` --
    /// `DocOp::FrameMove`'s splice semantics land the frame AT `to` for
    /// drops in either direction, so the drop index needs no conversion.
    /// The selection and the frame's editor-only name travel with it.
    fn move_frame_to(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let len = open.store.state().frames.len();
        if from == to || from >= len || to >= len {
            return;
        }
        if self.apply_doc(DocOp::FrameMove { from, to }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = to;
            move_name(&mut open.frame_names, from, to);
        }
    }

    /// Drop a dragged library frame onto clip `clip_ix`'s END slot:
    /// [`Self::insert_frame_in_clip`] at the sequence's tail.
    fn drop_frame_on_clip(&mut self, clip_ix: usize, frame_ix: usize, cx: &mut Context<Self>) {
        let seq_len = match &self.state {
            ViewerState::Ready(open) => open
                .store
                .state()
                .clips
                .get(clip_ix)
                .map(|clip| clip.to.saturating_sub(clip.from) + 1),
            _ => None,
        };
        if let Some(seq_len) = seq_len {
            self.insert_frame_in_clip(clip_ix, seq_len, frame_ix, cx);
        }
    }

    /// Insert a COPY of library frame `frame_ix` at sequence position
    /// `seq_pos` (0 = before the clip's first frame, len = append) inside
    /// clip `clip_ix`, extending the range by one. Two doc ops (add,
    /// range) -- two undo steps; a composite op is the known upgrade if
    /// that grates. The copy's editor-only name travels; stale indices
    /// vanish as no-ops.
    fn insert_frame_in_clip(
        &mut self,
        clip_ix: usize,
        seq_pos: usize,
        frame_ix: usize,
        cx: &mut Context<Self>,
    ) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let state = open.store.state();
        let Some(clip) = state.clips.get(clip_ix).cloned() else {
            return;
        };
        let seq_len = clip.to.saturating_sub(clip.from) + 1;
        if frame_ix >= state.frames.len() || seq_pos > seq_len {
            return;
        }
        let at = clip.from + seq_pos;
        let mut names = open.frame_names.clone();
        names.resize(state.frames.len(), String::new());
        if !self.apply_doc(
            DocOp::FrameAdd {
                at,
                copy_of: Some(frame_ix),
                map: None,
            },
            cx,
        ) {
            return;
        }
        if let ViewerState::Ready(open) = &mut self.state {
            names.insert(at, names.get(frame_ix).cloned().unwrap_or_default());
            open.frame_names = names;
        }
        self.apply_doc(
            DocOp::ClipSet {
                at: clip_ix,
                clip: Some(ClipEdit {
                    to: clip.to + 1,
                    ..clip
                }),
            },
            cx,
        );
    }

    /// Duplicate the frame at sequence position `seq_pos` of clip
    /// `clip_ix` in place: a copy lands right after it and the clip
    /// range grows by one (the sequence cell's duplicate button).
    fn duplicate_frame_in_clip(&mut self, clip_ix: usize, seq_pos: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(clip) = open.store.state().clips.get(clip_ix) else {
            return;
        };
        let seq_len = clip.to.saturating_sub(clip.from) + 1;
        if seq_pos >= seq_len {
            return;
        }
        let frame_ix = clip.from + seq_pos;
        self.insert_frame_in_clip(clip_ix, seq_pos + 1, frame_ix, cx);
    }

    /// Delete the frame at sequence position `seq_pos` of clip `clip_ix`
    /// (the sequence cell's delete button). One doc op: the store remaps
    /// every clip's range itself, dropping a clip whose sole frame died.
    /// Refuses to drop the document's last frame, like
    /// [`Self::delete_selected_frame`].
    fn delete_frame_in_clip(&mut self, clip_ix: usize, seq_pos: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let state = open.store.state();
        let len = state.frames.len();
        let Some(clip) = state.clips.get(clip_ix) else {
            return;
        };
        let seq_len = clip.to.saturating_sub(clip.from) + 1;
        if seq_pos >= seq_len || len <= 1 {
            return;
        }
        let at = clip.from + seq_pos;
        if at >= len {
            return;
        }
        let next = edits::selection_after_frame_delete(open.selected_frame, at, len);
        let mut names = open.frame_names.clone();
        names.resize(len, String::new());
        if self.apply_doc(DocOp::FrameDelete { at }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = next;
            names.remove(at);
            open.frame_names = names;
        }
    }

    /// Set frame `ix`'s editor-only name and persist the sidecar right
    /// away -- names aren't doc state, so there is no dirty flag to defer
    /// the write behind.
    fn set_frame_name(&mut self, ix: usize, name: String, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if ix >= open.store.state().frames.len() {
            return;
        }
        if open.frame_names.len() <= ix {
            open.frame_names.resize(ix + 1, String::new());
        }
        open.frame_names[ix] = name;
        self.write_sidecar();
        cx.notify();
    }

    /// Write the editor-settings sidecar for the open sprite. Failures
    /// are logged, not surfaced -- losing a wrap preference must never
    /// block or noise up a save.
    fn write_sidecar(&self) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let meta = editor_meta::EditorMeta {
            picker_cols: Some(open.picker_cols),
            frame_names: open.frame_names.clone(),
        };
        if let Err(e) = editor_meta::save(&open.root, &open.rel_path, &meta) {
            log::error!("GGO: failed to write editor sidecar for {}: {e}", open.rel_path);
        }
    }

    // ----------------------------------------------------------- tile ops

    /// Select `tile` as a 1x1 block, or deselect on a re-select of the
    /// already-active single tile (the brief's "clicks don't always
    /// mutate" affordance, alongside Escape).
    fn select_tile(&mut self, tile: u16, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if (tile as usize) >= open.store.state().tile_count {
            return; // stale click racing an undo
        }
        // Position the 1x1 block at the tile's DISPLAY cell -- sheet
        // order excludes blanks, so pool index != sheet index.
        let Some(strip) = open.pool_strip.as_ref() else {
            return;
        };
        let Some(display_ix) = strip.tiles.iter().position(|&t| t == tile) else {
            return; // blank or vanished tile -- not pickable
        };
        let rc = (display_ix % strip.cols, display_ix / strip.cols);
        open.selection = if open
            .selection
            .as_ref()
            .and_then(tiles::TileBlock::single)
            == Some(tile)
        {
            None
        } else {
            Some(tiles::marquee_block(rc, rc, strip.cols, &strip.tiles))
        };
        cx.notify();
    }

    /// The picker sheet cell under `position`, clamped into the sheet's
    /// grid (pad cells included -- the marquee resolves those to `None`
    /// tiles itself).
    fn picker_cell_rc(&self, position: gpui::Point<Pixels>) -> Option<(usize, usize)> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let strip = open.pool_strip.as_ref()?;
        let bounds = (*open.picker_bounds.borrow())?;
        let local_x = f32::from(position.x - bounds.origin.x);
        let local_y = f32::from(position.y - bounds.origin.y);
        let col = ((local_x / PICKER_CELL_PX).floor().max(0.) as usize).min(strip.cols - 1);
        let row = ((local_y / PICKER_CELL_PX).floor().max(0.) as usize)
            .min(strip.rows.saturating_sub(1));
        Some((col, row))
    }

    /// Arm a marquee at the pressed cell. Strict hit test: a press
    /// OUTSIDE the sheet arms nothing (matching the old click contract),
    /// unlike move/up which clamp so a drag can leave the sheet.
    fn on_picker_down(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        {
            let ViewerState::Ready(open) = &self.state else {
                return;
            };
            let Some(bounds) = *open.picker_bounds.borrow() else {
                return;
            };
            if !bounds.contains(&position) {
                return;
            }
        }
        let Some(rc) = self.picker_cell_rc(position) else {
            return;
        };
        if let ViewerState::Ready(open) = &mut self.state {
            open.picker_drag = Some((rc, rc));
            cx.notify();
        }
    }

    /// Widen the in-flight marquee to the hovered cell.
    fn on_picker_move(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(rc) = self.picker_cell_rc(position) else {
            return;
        };
        if let ViewerState::Ready(open) = &mut self.state
            && let Some((_, far)) = &mut open.picker_drag
            && *far != rc
        {
            *far = rc;
            cx.notify();
        }
    }

    /// Resolve the marquee: a no-drag press-release toggles a single
    /// tile (pad cells select nothing); a dragged rect becomes the block
    /// selection (dropped entirely when it covers only pad cells).
    fn on_picker_up(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let rc = self.picker_cell_rc(position);
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some((anchor, far)) = open.picker_drag.take() else {
            return;
        };
        let far = rc.unwrap_or(far);
        let Some(strip) = open.pool_strip.as_ref() else {
            cx.notify();
            return;
        };
        let (cols, display) = (strip.cols, strip.tiles.clone());
        if anchor == far {
            match display.get(anchor.1 * cols + anchor.0).copied() {
                Some(tile) => self.select_tile(tile, cx),
                None => cx.notify(),
            }
            return;
        }
        let block = tiles::marquee_block(anchor, far, cols, &display);
        open.selection = block.tiles.iter().any(Option::is_some).then_some(block);
        cx.notify();
    }

    /// The W/H steppers' body: one tile grid step per click, clamped to
    /// worldlib's sprite range -- an at-bound step drops without an op
    /// (M5's undo-stack hygiene rule), an in-range one goes through
    /// `apply_doc` as `DocOp::Resize` with a top-left anchor, so undo,
    /// recompose, and error surfacing come with it.
    fn step_size(&mut self, dw: i32, dh: i32, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let state = open.store.state();
        let range = MIN_SPRITE_TILES as i32..=MAX_SPRITE_TILES as i32;
        let w = (state.w_tiles as i32 + dw).clamp(*range.start(), *range.end()) as u8;
        let h = (state.h_tiles as i32 + dh).clamp(*range.start(), *range.end()) as u8;
        if (w, h) == (state.w_tiles, state.h_tiles) {
            return;
        }
        self.apply_doc(
            DocOp::Resize {
                w_tiles: w,
                h_tiles: h,
                anchor: Anchor::TopLeft,
            },
            cx,
        );
    }

    /// The picker-cols stepper's body: adjust the session wrap width
    /// (clamped to 1..=[`MAX_PICKER_COLS`]) and recompose the sheet at
    /// the new wrap. Not a doc op -- the `.til` has no layout field, so
    /// this never dirties the store or lands on the undo stack.
    fn step_picker_cols(&mut self, delta: i32, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let next = (open.picker_cols as i32 + delta).clamp(1, MAX_PICKER_COLS as i32) as usize;
        if next == open.picker_cols {
            return;
        }
        open.picker_cols = next;
        open.pool_strip = loader::compose_pool_strip(open.store.state(), next);
        cx.notify();
    }

    fn deselect_tile(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && (open.selection.is_some()
                || open.picker_drag.is_some()
                || open.eraser
                || open.frame_settings.is_some())
        {
            open.selection = None;
            open.picker_drag = None;
            open.eraser = false;
            open.frame_settings = None;
            cx.notify();
        }
    }

    /// Open the per-frame settings popup for sequence frame `frame_ix`
    /// of clip `clip_ix`, anchored at `position` (the "..." click).
    /// Selects the frame so the preview and the popup's editors (which
    /// target the selected frame) both point at it.
    fn open_frame_settings(
        &mut self,
        clip_ix: usize,
        frame_ix: usize,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.select_frame(frame_ix, cx);
        if let ViewerState::Ready(open) = &mut self.state {
            open.frame_settings = Some((clip_ix, position));
            cx.notify();
        }
    }

    fn close_frame_settings(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.frame_settings.is_some()
        {
            open.frame_settings = None;
            cx.notify();
        }
    }

    /// The Eraser toggle: while on, preview clicks blank cells.
    fn toggle_eraser(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.eraser = !open.eraser;
            cx.notify();
        }
    }

    /// A preview click DURING playback pauses the transport and adopts
    /// its shown frame as the selection FIRST, so the click edits the
    /// frame the user actually saw -- not the stale pre-playback
    /// selection (M6 review fix-forward). A no-op when not playing.
    fn pause_playback_on_edit(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(playing) = open.playing.take() else {
            return;
        };
        open._tick_task = None;
        open.selected_frame = playing.frame;
        cx.notify();
    }

    /// The preview's left-click body (the mouse listener adds only the
    /// focus grab): pause-and-sync if playing, THEN map the click to a
    /// cell edit -- reviewer-specified order, M6 fix-forward.
    fn on_preview_click(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        self.pause_playback_on_edit(cx);
        if let Some(cell) = self.preview_cell_at(position) {
            self.set_tile_on_cell(cell, cx);
        }
    }

    /// Click a cell of the selected-frame preview with a selection
    /// active: stamp the block anchored at that cell, clipped at the
    /// frame's edges, as ONE `DocOp::FrameTilesSet` through the same
    /// `apply_doc` path as every other edit -- undo, thumbnail recompose,
    /// and error surfacing come with it. Targets the SELECTED frame --
    /// which, on a click during playback, was just synced to the
    /// transport's shown frame by [`Self::pause_playback_on_edit`].
    /// No-selection and all-unchanged clicks are dropped without an op
    /// (M5's undo-stack hygiene rule).
    fn set_tile_on_cell(&mut self, cell: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        if open.eraser {
            let frame = open.selected_frame;
            let state = open.store.state();
            let Some(&current) = state.frames.get(frame).and_then(|f| f.map.get(cell)) else {
                return;
            };
            let off = current as usize * ggo_worldlib::sprites::hw::TILE_BYTES;
            let already_blank = state
                .pool
                .get(off..off + ggo_worldlib::sprites::hw::TILE_BYTES)
                .is_some_and(|tile| tile.iter().all(|&b| b == 0));
            if already_blank {
                return; // erasing a blank cell -- no op to push
            }
            self.apply_doc(
                DocOp::FrameCellsErase {
                    frame,
                    cells: vec![cell],
                },
                cx,
            );
            return;
        }
        let Some(block) = open.selection.as_ref() else {
            return;
        };
        let frame = open.selected_frame;
        let state = open.store.state();
        let Some(f) = state.frames.get(frame) else {
            return;
        };
        if cell >= f.map.len()
            || block
                .tiles
                .iter()
                .flatten()
                .any(|&t| t as usize >= state.tile_count)
        {
            return; // stale geometry/selection racing an undo
        }
        let sets = tiles::stamp_sets(
            block,
            cell,
            state.w_tiles as usize,
            state.h_tiles as usize,
            &f.map,
        );
        if sets.is_empty() {
            return; // nothing would change -- don't push a no-op undo entry
        }
        self.apply_doc(DocOp::FrameTilesSet { frame, sets }, cx);
    }

    /// The preview click handler's window->cell mapping: local coords
    /// against the recorded preview bounds, then `tiles::cell_at` over
    /// the selected frame's tile grid.
    fn preview_cell_at(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        // A transformed frame's preview is the doubled, rotated canvas:
        // the cell math below would land on the wrong tiles, so clicks
        // there edit nothing (the click still pauses playback/focuses).
        if !open.shown_frame_is_identity() {
            return None;
        }
        let bounds = (*open.preview_bounds.borrow())?;
        let state = open.store.state();
        tiles::cell_at(
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            state.w_tiles as usize,
            state.h_tiles as usize,
        )
    }

    // ---------------------------------------------------------- clip ops

    /// Add a clip with ggo-ide `Msg::AddClip`'s defaults (see
    /// `edits::default_new_clip`).
    fn add_clip(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let state = open.store.state();
        let clip =
            edits::default_new_clip(state.clips.len(), open.selected_frame, state.frames.len());
        self.apply_doc(DocOp::ClipAdd { clip }, cx);
    }

    /// Delete clip `i` -- ggo-ide `Msg::DeleteClip`, including its
    /// active-selection shift rule. Bounds-checked: a stale index (clip
    /// removed by an undo between render and click) should vanish as a
    /// no-op, not surface as the store's `ClipOutOfRange` error.
    fn delete_clip(&mut self, i: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        if i >= open.store.state().clips.len() {
            return;
        }
        let next_active = edits::active_clip_after_clip_delete(open.active_clip, i);
        if self.apply_doc(DocOp::ClipSet { at: i, clip: None }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.active_clip = next_active;
        }
    }

    /// Toggle clip `i`'s loop flag -- ggo-ide `Msg::ClipLoop` (whole-clip
    /// `ClipSet` with just the flag changed).
    fn set_clip_loop(&mut self, i: usize, loop_: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(clip) = open.store.state().clips.get(i).cloned() else {
            return;
        };
        if clip.loop_ == loop_ {
            return;
        }
        self.apply_doc(
            DocOp::ClipSet {
                at: i,
                clip: Some(ClipEdit { loop_, ..clip }),
            },
            cx,
        );
    }

    // ------------------------------------------------------ field editors

    /// A completed edit's buffer -> the op, mirroring ggo-ide's guard
    /// order per field:
    ///
    /// - clip name (`Msg::ClipNameSubmit`): trim, byte-clamp
    ///   (`clamp_clip_name_bytes`), empty -> dropped; then `ClipSet`.
    /// - clip from/to: parse `usize` (unparsable -> dropped, world_panel's
    ///   commit rule); validate the CANDIDATE range via
    ///   `edits::clip_range_error` BEFORE building the op -- a failure is
    ///   shown inline on that clip's row and nothing is applied. (ggo-ide
    ///   edits ranges with clamped sliders, so its equivalent guard is the
    ///   clamp itself; typed input needs the explicit check.)
    /// - duration (`Msg::DurationSubmit`): parse-or-0 then floor to
    ///   `MIN_FRAME_MS` (`edits::parse_duration_ms`), applied to the
    ///   selected frame.
    ///
    /// Unchanged values are dropped without an op: blur commits every
    /// focus exit, and a no-change `ClipSet`/`FrameDuration` would still
    /// push an undo entry.
    fn commit_edit(&mut self, target: EditTarget, text: String, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        match target {
            EditTarget::ClipName(i) => {
                let Some(clip) = open.store.state().clips.get(i).cloned() else {
                    return;
                };
                let name = clamp_clip_name_bytes(text.trim());
                if name.is_empty() || name == clip.name {
                    cx.notify(); // re-sync the editor's text from the doc
                    return;
                }
                self.apply_doc(
                    DocOp::ClipSet {
                        at: i,
                        clip: Some(ClipEdit { name, ..clip }),
                    },
                    cx,
                );
            }
            EditTarget::ClipFrom(i) | EditTarget::ClipTo(i) => {
                let Some(clip) = open.store.state().clips.get(i).cloned() else {
                    return;
                };
                let Ok(value) = text.trim().parse::<usize>() else {
                    cx.notify(); // dropped, not committed -- editor re-syncs
                    return;
                };
                let (from, to) = match target {
                    EditTarget::ClipFrom(_) => (value, clip.to),
                    _ => (clip.from, value),
                };
                if (from, to) == (clip.from, clip.to) {
                    cx.notify();
                    return;
                }
                let frame_count = open.store.state().frames.len();
                if let Some(err) = edits::clip_range_error(from, to, frame_count) {
                    open.clip_error = Some((i, err));
                    cx.notify();
                    return;
                }
                open.clip_error = None;
                self.apply_doc(
                    DocOp::ClipSet {
                        at: i,
                        clip: Some(ClipEdit { from, to, ..clip }),
                    },
                    cx,
                );
            }
            EditTarget::Duration => {
                let ms = edits::parse_duration_ms(&text);
                let at = open.selected_frame;
                let Some(frame) = open.store.state().frames.get(at) else {
                    return;
                };
                if frame.duration_ms == ms {
                    cx.notify();
                    return;
                }
                self.apply_doc(DocOp::FrameDuration { at, ms }, cx);
            }
            EditTarget::Rot
            | EditTarget::ScaleX
            | EditTarget::ScaleY
            | EditTarget::ShearX
            | EditTarget::ShearY => {
                let frame = open.selected_frame;
                let Some(current) = open.store.state().frames.get(frame).map(|f| f.transform)
                else {
                    return;
                };
                // One field parsed and merged into the frame's CURRENT
                // transform; unparsable input is dropped and the editor
                // re-syncs from the doc (the duration editor's revert).
                let merged = match target {
                    EditTarget::Rot => edits::parse_angle_deg(&text).map(|angle256| {
                        FrameTransform {
                            angle256,
                            ..current
                        }
                    }),
                    EditTarget::ScaleX => edits::parse_fixed88(&text)
                        .map(|sx| FrameTransform { sx, ..current }),
                    EditTarget::ScaleY => edits::parse_fixed88(&text)
                        .map(|sy| FrameTransform { sy, ..current }),
                    EditTarget::ShearX => edits::parse_fixed88(&text)
                        .map(|shear_x| FrameTransform { shear_x, ..current }),
                    _ => edits::parse_fixed88(&text)
                        .map(|shear_y| FrameTransform { shear_y, ..current }),
                };
                let Some(transform) = merged else {
                    cx.notify(); // dropped, not committed -- editor re-syncs
                    return;
                };
                if transform == current {
                    cx.notify();
                    return;
                }
                self.apply_doc(DocOp::FrameTransformSet { frame, transform }, cx);
            }
        }
    }

    /// The display string an unfocused input shows for `target`.
    fn edit_display_text(
        target: &EditTarget,
        state: &SpriteState,
        selected_frame: usize,
    ) -> String {
        match target {
            EditTarget::ClipName(i) => state
                .clips
                .get(*i)
                .map_or_else(String::new, |c| c.name.clone()),
            EditTarget::ClipFrom(i) => state
                .clips
                .get(*i)
                .map_or_else(String::new, |c| c.from.to_string()),
            EditTarget::ClipTo(i) => state
                .clips
                .get(*i)
                .map_or_else(String::new, |c| c.to.to_string()),
            EditTarget::Duration => state
                .frames
                .get(selected_frame)
                .map_or_else(String::new, |f| f.duration_ms.to_string()),
            EditTarget::Rot => state
                .frames
                .get(selected_frame)
                .map_or_else(String::new, |f| {
                    edits::format_angle_deg(f.transform.angle256)
                }),
            EditTarget::ScaleX | EditTarget::ScaleY | EditTarget::ShearX | EditTarget::ShearY => {
                state
                    .frames
                    .get(selected_frame)
                    .map_or_else(String::new, |f| {
                        let value = match target {
                            EditTarget::ScaleX => f.transform.sx,
                            EditTarget::ScaleY => f.transform.sy,
                            EditTarget::ShearX => f.transform.shear_x,
                            _ => f.transform.shear_y,
                        };
                        edits::format_fixed88(value)
                    })
            }
        }
    }

    /// Keep the field-editor entities in sync with the doc: rebuild when
    /// the target set changed (clip added/deleted, undo/redo
    /// restructure), otherwise refresh unfocused editors' text from the
    /// doc -- a focused editor keeps the user's in-progress buffer
    /// (`ggo_world_panel::ensure_inspector`'s rule, itself ggo-ide's
    /// draft pattern).
    fn ensure_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let mut targets = Vec::with_capacity(open.store.state().clips.len() * 3 + 6);
        for i in 0..open.store.state().clips.len() {
            targets.push(EditTarget::ClipName(i));
            targets.push(EditTarget::ClipFrom(i));
            targets.push(EditTarget::ClipTo(i));
        }
        targets.push(EditTarget::Duration);
        targets.push(EditTarget::Rot);
        targets.push(EditTarget::ScaleX);
        targets.push(EditTarget::ScaleY);
        targets.push(EditTarget::ShearX);
        targets.push(EditTarget::ShearY);

        let same_targets = open.editors.len() == targets.len()
            && open
                .editors
                .iter()
                .zip(&targets)
                .all(|(entry, target)| entry.target == *target);

        if same_targets {
            let state = open.store.state();
            for entry in &open.editors {
                if entry.editor.focus_handle(cx).is_focused(window) {
                    continue;
                }
                let text = Self::edit_display_text(&entry.target, state, open.selected_frame);
                if entry.editor.read(cx).text(cx) != text {
                    entry
                        .editor
                        .update(cx, |editor, cx| editor.set_text(text, window, cx));
                }
            }
            return;
        }

        let mut entries = Vec::with_capacity(targets.len());
        for target in targets {
            let text = Self::edit_display_text(&target, open.store.state(), open.selected_frame);
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(text, window, cx);
                editor
            });
            let commit = cx.subscribe(&editor, Self::handle_editor_event);
            // Focus entering a field flips the key context's flag to
            // `editing` (space types instead of toggling playback) -- but
            // only a re-render re-reads `dispatch_context`, so nudge one.
            let focus = cx.on_focus(&editor.focus_handle(cx), window, |_, _, cx| cx.notify());
            entries.push(EditorEntry {
                target,
                editor,
                _subscriptions: [commit, focus],
            });
        }
        open.editors = entries;
    }

    /// Blur commits the field (world_panel's rule; ggo-ide commits on
    /// Enter only because iced has no blur event -- see its
    /// `timeline.rs` module doc). The notify re-evaluates the key
    /// context's `editing` flag either way.
    fn handle_editor_event(
        &mut self,
        editor: Entity<Editor>,
        event: &EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, EditorEvent::Blurred) {
            self.commit_editor(editor.entity_id(), cx);
            cx.notify();
        }
    }

    fn commit_editor(&mut self, editor_id: EntityId, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some((target, text)) = open
            .editors
            .iter()
            .find(|e| e.editor.entity_id() == editor_id)
            .map(|e| (e.target.clone(), e.editor.read(cx).text(cx)))
        else {
            return;
        };
        self.commit_edit(target, text, cx);
    }

    fn on_commit_field(&mut self, _: &CommitField, window: &mut Window, cx: &mut Context<Self>) {
        // Enter in the rename field commits the rename -- the form's own
        // editor is not one of the doc's `EditTarget` editors, so it has
        // to be checked before the loop below.
        if let Some(PanelForm::Rename { editor, .. }) = &self.form
            && editor.focus_handle(cx).is_focused(window)
        {
            self.confirm_rename(cx);
            return;
        }
        if let Some(PanelForm::NameFrame { editor, .. }) = &self.form
            && editor.focus_handle(cx).is_focused(window)
        {
            self.confirm_name_frame(cx);
            return;
        }
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let focused = open
            .editors
            .iter()
            .find(|e| e.editor.focus_handle(cx).is_focused(window))
            .map(|e| e.editor.entity_id());
        if let Some(id) = focused {
            self.commit_editor(id, cx);
        }
    }

    /// The panel's key context: the panel identifier plus an
    /// `editing`/`not_editing` flag from live editor focus
    /// (project_panel's `dispatch_context` pattern) -- see
    /// [`bind_panel_keys`] for why space needs it.
    fn dispatch_context(&self, window: &Window, cx: &Context<Self>) -> KeyContext {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add(KEY_CONTEXT);
        let form_editing = matches!(
            &self.form,
            Some(PanelForm::Rename { editor, .. } | PanelForm::NameFrame { editor, .. })
                if editor.focus_handle(cx).is_focused(window)
        );
        let editing = form_editing
            || match &self.state {
                ViewerState::Ready(open) => open
                    .editors
                    .iter()
                    .any(|e| e.editor.focus_handle(cx).is_focused(window)),
                _ => false,
            };
        key_context.add(if editing { "editing" } else { "not_editing" });
        key_context
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

    /// Transport row: play/pause, the clip selector, undo/redo/save, and
    /// the sprite name with world_panel's dirty dot.
    fn render_transport(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_transport is only called in the Ready state");
        };
        let playing = open.playing.is_some();
        let dirty = open.store.dirty();
        let state = open.store.state();
        let clip_label: SharedString = match open.active_clip.and_then(|i| state.clips.get(i)) {
            Some(c) => c.name.clone().into(),
            None => "All frames".into(),
        };
        let clip_names: Vec<String> = state.clips.iter().map(|c| c.name.clone()).collect();
        let title = format!("{}{}", open.source_rel, if dirty { " ●" } else { "" });
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            {
                let weak = weak.clone();
                menu = menu.entry("All frames", None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_clip(None, cx)).ok();
                });
            }
            for (ix, name) in clip_names.into_iter().enumerate() {
                let weak = weak.clone();
                menu = menu.entry(SharedString::from(name), None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_clip(Some(ix), cx))
                        .ok();
                });
            }
            menu
        });
        h_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Button::new("ggo-sprite-play", if playing { "Pause" } else { "Play" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx))),
            )
            .child(DropdownMenu::new("ggo-sprite-clip", clip_label, menu))
            .child(Self::stepper(
                "ggo-sprite-w",
                format!("W {}", state.w_tiles),
                state.w_tiles > MIN_SPRITE_TILES,
                state.w_tiles < MAX_SPRITE_TILES,
                "Sprite width in tiles",
                |this, d, cx| this.step_size(d, 0, cx),
                cx,
            ))
            .child(Self::stepper(
                "ggo-sprite-h",
                format!("H {}", state.h_tiles),
                state.h_tiles > MIN_SPRITE_TILES,
                state.h_tiles < MAX_SPRITE_TILES,
                "Sprite height in tiles",
                |this, d, cx| this.step_size(0, d, cx),
                cx,
            ))
            .child(div().flex_1())
            .child(
                Label::new(SharedString::from(title))
                    .size(LabelSize::Small)
                    .color(if dirty { Color::Modified } else { Color::Muted }),
            )
            .child(
                IconButton::new("ggo-sprite-undo", IconName::Undo)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Undo"))
                    .on_click(cx.listener(|this, _, _, cx| this.undo_impl(cx))),
            )
            .child(
                IconButton::new("ggo-sprite-redo", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Redo"))
                    .on_click(cx.listener(|this, _, _, cx| this.redo_impl(cx))),
            )
            .child(
                Button::new("ggo-sprite-save", "Save")
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

    /// A `-`/`+` stepper with a small label between the buttons, disabled
    /// at its clamps so the row shows its own limits (the import panel's
    /// zoom-stepper idiom). Shared by the onion row, the transport's W/H
    /// size steppers, and the tile picker's cols stepper.
    fn stepper(
        id: &'static str,
        label: String,
        can_dec: bool,
        can_inc: bool,
        tooltip: &'static str,
        step: fn(&mut SpritePanel, i32, &mut Context<SpritePanel>),
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        h_flex()
            .gap_0p5()
            .child(
                IconButton::new(SharedString::from(format!("{id}-minus")), IconName::Dash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(ui::Tooltip::text(tooltip))
                    .disabled(!can_dec)
                    .on_click(cx.listener(move |this, _, _, cx| step(this, -1, cx))),
            )
            .child(
                Label::new(SharedString::from(label))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new(SharedString::from(format!("{id}-plus")), IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .tooltip(ui::Tooltip::text(tooltip))
                    .disabled(!can_inc)
                    .on_click(cx.listener(move |this, _, _, cx| step(this, 1, cx))),
            )
    }

    /// The onion-skin control row: the toggle, a `-`/`+` stepper for the
    /// back and forward ghost counts, and one for the opacity -- ggo-ide's
    /// `timeline::State::transport_row` onion group, minus its slider (see
    /// [`onion`]'s module doc). The steppers are disabled once the counts
    /// hit their clamp so the row shows its own limits, matching the
    /// import panel's zoom stepper.
    fn render_onion(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_onion is only called in the Ready state");
        };
        let o = open.onion;
        h_flex()
            .gap_1()
            .px_1()
            .pb_1()
            .child(
                Checkbox::new("ggo-sprite-eraser", ToggleState::from(open.eraser))
                    .label("Eraser")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_eraser(cx))),
            )
            .child(
                Checkbox::new("ggo-sprite-onion", ToggleState::from(o.on))
                    .label("Onion")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_onion(onion::OnionState::toggle, cx)
                    })),
            )
            .child(Self::stepper(
                "ggo-sprite-onion-back",
                format!("back {}", o.back),
                o.can_step_back(-1),
                o.can_step_back(1),
                "Ghost frames behind",
                |this, d, cx| this.update_onion(|s| s.step_back(d), cx),
                cx,
            ))
            .child(Self::stepper(
                "ggo-sprite-onion-fwd",
                format!("fwd {}", o.fwd),
                o.can_step_fwd(-1),
                o.can_step_fwd(1),
                "Ghost frames ahead",
                |this, d, cx| this.update_onion(|s| s.step_fwd(d), cx),
                cx,
            ))
            .child(Self::stepper(
                "ggo-sprite-onion-opacity",
                format!("{}%", (o.opacity * 100.0).round() as i32),
                o.can_step_opacity(-1),
                o.can_step_opacity(1),
                "Ghost opacity",
                |this, d, cx| this.update_onion(|s| s.step_opacity(d), cx),
                cx,
            ))
            .into_any_element()
    }

    /// The big center preview: the shown frame fit into a [`PREVIEW_PX`]
    /// box. An invisible overlay canvas records the image's on-screen
    /// bounds at prepaint (world_panel's `last_bounds` idiom -- gpui
    /// mouse listeners only get window coords), and a left click goes
    /// through [`Self::on_preview_click`]: pause-and-sync any running
    /// transport, then (with an active tile) map through
    /// `tiles::cell_at` to a `FrameTileSet` on the selected frame.
    fn render_preview(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_preview is only called in the Ready state");
        };
        let mut preview = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(cx.theme().colors().editor_background);
        // The shown frame with its transform applied (identity = the
        // strip's legacy image); a transformed frame's grid overlay is
        // suppressed -- the doubled, rotated canvas has no meaningful
        // cell geometry (clicks are guarded the same way in
        // `preview_cell_at`).
        let show_grid = open.shown_frame_is_identity();
        if let Some(image) = open.preview_image() {
            let (w, h) = image_px_size(&image);
            let state = open.store.state();
            let (fit_w, fit_h) = playback::preview_display_size(
                w,
                h,
                state.w_tiles as u32 * ggo_worldlib::sprites::hw::TILE_PX as u32,
                state.h_tiles as u32 * ggo_worldlib::sprites::hw::TILE_PX as u32,
                PREVIEW_PX,
            );
            let bounds_cell = open.preview_bounds.clone();
            let state = open.store.state();
            let (grid_cols, grid_rows) = (state.w_tiles as usize, state.h_tiles as usize);
            let grid_color = cx.theme().colors().border;
            // `.top_0().left_0()` matters: an absolute child with auto
            // insets sits at its STATIC position -- after the in-flow img
            // sibling, one image-height below the box -- so the recorded
            // bounds (and every click mapped against them) were shifted by
            // exactly the image and silently missed.
            let overlay = gpui::canvas(
                move |bounds, _window, _cx| {
                    *bounds_cell.borrow_mut() = Some(bounds);
                },
                move |bounds, (), window, _cx| {
                    if show_grid {
                        paint_tile_grid(bounds, grid_cols, grid_rows, grid_color, window);
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full();
            // Onion ghosts, farthest first, each absolutely positioned over
            // the same box as the real frame and drawn BEFORE it (so the
            // current frame is on top). Frames all share the sprite's
            // dimensions, so one fit box serves them all. Each ghost is
            // its OWN tinted image (red behind, blue ahead --
            // `loader::compose_ghost`), not the plain `open.frames` entry:
            // `ghost.alpha` still governs the layer's overall opacity here,
            // exactly as before this fast-follow -- the tint is baked into
            // the pixels underneath it, not a replacement for it.
            let ghosts: Vec<gpui::AnyElement> = open
                .ghosts()
                .into_iter()
                .filter_map(|ghost| {
                    let image = open.ghost_image(ghost.dist, ghost.idx)?;
                    Some(
                        img(image).nearest(true)
                            .absolute()
                            .w(px(fit_w))
                            .h(px(fit_h))
                            .opacity(ghost.alpha)
                            .into_any_element(),
                    )
                })
                .collect();
            preview = preview.child(
                div()
                    .relative()
                    .w(px(fit_w))
                    .h(px(fit_h))
                    .children(ghosts)
                    .child(img(image).nearest(true).w(px(fit_w)).h(px(fit_h)))
                    .child(overlay)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            // Take focus so Escape/undo bindings apply
                            // (and any in-flight field edit blur-commits).
                            window.focus(&this.focus_handle, cx);
                            this.on_preview_click(event.position, cx);
                        }),
                    ),
            );
        }
        preview.into_any_element()
    }

    /// The tile picker, beside the frame grid: the BOUND TILESET's tiles
    /// as one composed sheet (`loader::compose_pool_strip` -- the sprite's
    /// pool is that `.til`, byte for byte), laid out
    /// [`loader::PICKER_COLS`] wide at [`PICKER_CELL_PX`] per tile. Click
    /// a tile to make it active (re-click to deselect), then click a frame
    /// cell in the preview to place it -- the missing SOURCE half of
    /// F2/M6's already-shipped `FrameTileSet` placement.
    ///
    /// One image plus an absolutely-positioned selection outline, rather
    /// than one element per tile: a `.til` runs to hundreds of tiles, and
    /// the same overlay-canvas + hit-math shape the preview already uses
    /// ([`tiles::picker_tile_at`]) costs one element either way.
    fn render_tile_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_tile_picker is only called in the Ready state");
        };
        let border = cx.theme().colors().border;
        let accent = cx.theme().colors().border_focused;
        // Wide enough for the sheet at the chosen wrap, never narrower
        // than the default column (the header row needs the room).
        let sheet_cols = open
            .pool_strip
            .as_ref()
            .map_or(loader::PICKER_COLS, |strip| strip.cols);
        let width = px(
            (PICKER_CELL_PX * sheet_cols as f32 + 12.).max(f32::from(PICKER_WIDTH)),
        );
        let picker_cols = open.picker_cols;
        let mut column = v_flex()
            .flex_none()
            .w(width)
            .h_full()
            .border_l_1()
            .border_color(border)
            .child(
                h_flex()
                    .px_1()
                    .pt_1()
                    .justify_between()
                    .child(
                        Label::new("Tiles")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Self::stepper(
                        "ggo-sprite-picker-cols",
                        format!("{picker_cols} col"),
                        picker_cols > 1,
                        picker_cols < MAX_PICKER_COLS,
                        "Tile picker columns",
                        |this, d, cx| this.step_picker_cols(d, cx),
                        cx,
                    )),
            );
        if let Some(strip) = open.pool_strip.as_ref() {
            let sheet_w = px(PICKER_CELL_PX * strip.cols as f32);
            let sheet_h = px(PICKER_CELL_PX * strip.rows as f32);
            let bounds_cell = open.picker_bounds.clone();
            let (grid_cols, grid_rows) = (strip.cols, strip.rows);
            let pad = tiles::picker_pad_region(strip.tiles.len(), strip.cols);
            let background = cx.theme().colors().panel_background;
            // Same static-position trap as the preview overlay: without
            // explicit insets this canvas would record bounds one
            // sheet-height below the image it must cover.
            let overlay = gpui::canvas(
                move |bounds, _window, _cx| {
                    *bounds_cell.borrow_mut() = Some(bounds);
                },
                move |bounds, (), window, _cx| {
                    paint_tile_grid(bounds, grid_cols, grid_rows, border, window);
                    // The sheet's zero-filled partial last row is padding,
                    // not tiles -- cover its grid lines and interior so it
                    // reads as empty space, keeping only the 1px edges
                    // shared with real tiles.
                    if let Some((pad_col, pad_row)) = pad {
                        let line = px(1.);
                        window.paint_quad(gpui::fill(
                            Bounds::from_corners(
                                gpui::point(
                                    bounds.origin.x + px(pad_col as f32 * PICKER_CELL_PX) + line,
                                    bounds.origin.y + px(pad_row as f32 * PICKER_CELL_PX) + line,
                                ),
                                bounds.bottom_right(),
                            ),
                            background,
                        ));
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full();
            // One rect outline serves both the locked-in block selection
            // and the in-flight marquee (the marquee wins while dragging
            // so the user sees what a release would select).
            let rect = open
                .picker_drag
                .map(|(a, b)| (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)))
                .or_else(|| {
                    open.selection.as_ref().map(|block| {
                        let (c, r) = block.origin;
                        (c, r, c + block.cols - 1, r + block.rows - 1)
                    })
                });
            let selection = rect.map(|(c0, r0, c1, r1)| {
                div()
                    .absolute()
                    .left(px(c0 as f32 * PICKER_CELL_PX))
                    .top(px(r0 as f32 * PICKER_CELL_PX))
                    .w(px((c1 - c0 + 1) as f32 * PICKER_CELL_PX))
                    .h(px((r1 - r0 + 1) as f32 * PICKER_CELL_PX))
                    .border_1()
                    .border_color(accent)
            });
            column = column.child(
                div()
                    .id("ggo-sprite-tiles")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_1()
                    .child(
                        div()
                            .relative()
                            .w(sheet_w)
                            .h(sheet_h)
                            .child(img(strip.image.clone()).nearest(true).w(sheet_w).h(sheet_h))
                            .child(overlay)
                            .children(selection)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    this.on_picker_down(event.position, cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                |this, event: &gpui::MouseMoveEvent, _, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        this.on_picker_move(event.position, cx);
                                    }
                                },
                            ))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                                    this.on_picker_up(event.position, cx);
                                }),
                            ),
                    ),
            );
        }
        column.into_any_element()
    }

    /// The open "New …"/"Rename …" form, as a bar above the viewer.
    fn render_form(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let row = h_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border);
        match self.form.as_ref()? {
            PanelForm::New {
                kind,
                dir_rel,
                name,
                tilesets,
                selected,
                error,
            } => {
                let label = format!("{} \"{name}\" in {}", kind.label(), dir_rel);
                let has_tilesets = !tilesets.is_empty();
                let choice = tilesets.get(*selected);
                let picked: SharedString = choice
                    .map(TilesetChoice::label)
                    .unwrap_or_else(|| "no tilesets".to_string())
                    .into();
                let weak = cx.weak_entity();
                let rows: Vec<String> = tilesets.iter().map(TilesetChoice::label).collect();
                let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
                    for (ix, name) in rows.into_iter().enumerate() {
                        let weak = weak.clone();
                        menu = menu.entry(SharedString::from(name), None, move |_window, cx| {
                            weak.update(cx, |this, cx| this.select_new_tileset(ix, cx))
                                .ok();
                        });
                    }
                    menu
                });
                Some(
                    row.child(
                        Label::new(SharedString::from(label))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(DropdownMenu::new("ggo-sprite-new-tileset", picked, menu))
                    .child(
                        Button::new("ggo-sprite-new-create", "Create")
                            .disabled(!has_tilesets)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_new(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("ggo-sprite-new-cancel", "Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.cancel_form(cx);
                            },
                        )),
                    )
                    // The sharing warning is a WARNING, not an error: the
                    // binding is legal and sometimes wanted, it just has
                    // consequences for a file the user did not open.
                    .children(
                        choice
                            .and_then(TilesetChoice::share_warning)
                            .map(|w| Label::new(w).size(LabelSize::Small).color(Color::Warning)),
                    )
                    .children(
                        error
                            .clone()
                            .or_else(|| {
                                (!has_tilesets).then(|| {
                                    "no .til in this project -- import a tileset first".to_string()
                                })
                            })
                            .map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)),
                    )
                    .into_any_element(),
                )
            }
            PanelForm::Rename {
                source_rel,
                editor,
                error,
            } => Some(
                row.child(
                    Label::new(format!("Rename {source_rel} to"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(Self::editor_input(editor.clone(), cx))
                .child(
                    Button::new("ggo-sprite-rename-apply", "Rename")
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_rename(cx))),
                )
                .child(
                    Button::new("ggo-sprite-rename-cancel", "Cancel").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cancel_form(cx);
                        },
                    )),
                )
                .children(
                    error
                        .clone()
                        .map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)),
                )
                .into_any_element(),
            ),
            PanelForm::NameFrame { index, editor } => Some(
                row.child(
                    Label::new(format!("Name frame {}", index + 1))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(Self::editor_input(editor.clone(), cx))
                .child(
                    Button::new("ggo-sprite-name-frame-apply", "Set")
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_name_frame(cx))),
                )
                .child(
                    Button::new("ggo-sprite-name-frame-cancel", "Cancel").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cancel_form(cx);
                        },
                    )),
                )
                .into_any_element(),
            ),
        }
    }

    /// One field text input, in world_panel's minimal bordered box.
    fn editor_input(editor: Entity<Editor>, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .flex_1()
            .min_w_0()
            .px_1()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .bg(cx.theme().colors().editor_background)
            .child(editor)
            .into_any_element()
    }

    /// The clip-CRUD side column: per clip a name row (+ delete) and a
    /// from/to/loop row, an inline range error under the offending clip,
    /// and an add button.
    /// The clips as a horizontal card row along the BOTTOM (where the
    /// sequence has room to breathe); click a card to activate its clip,
    /// whose sequence renders in the row beneath.
    fn render_clips(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_clips is only called in the Ready state");
        };
        let state = open.store.state();
        let editor_for = |target: EditTarget| {
            open.editors
                .iter()
                .find(|e| e.target == target)
                .map(|e| e.editor.clone())
        };
        let mut cards = h_flex().p_1().gap_1().items_start();
        for (i, clip) in state.clips.iter().enumerate() {
            let mut row = v_flex()
                .id(("ggo-sprite-clip", i))
                .min_w(CLIPS_WIDTH)
                .flex_none()
                .gap_0p5()
                .p_0p5()
                .border_1()
                .rounded_sm()
                .border_color(if open.active_clip == Some(i) {
                    cx.theme().colors().border_focused
                } else {
                    cx.theme().colors().border_variant
                })
                // Click anywhere on the card to make the clip active
                // (clicks inside its editors bubble here too -- selecting
                // the clip being edited is a no-op).
                .on_click(cx.listener(move |this, _, _, cx| this.select_clip(Some(i), cx)))
                .child(
                    h_flex()
                        .gap_0p5()
                        .children(
                            editor_for(EditTarget::ClipName(i)).map(|e| Self::editor_input(e, cx)),
                        )
                        .child({
                            let weak = cx.weak_entity();
                            Checkbox::new(
                                ("ggo-sprite-clip-loop", i),
                                ToggleState::from(clip.loop_),
                            )
                            .label("loop")
                            .on_click(move |toggle, _window, cx| {
                                let on = matches!(toggle, ToggleState::Selected);
                                weak.update(cx, |this, cx| this.set_clip_loop(i, on, cx))
                                    .ok();
                            })
                        })
                        .child(
                            IconButton::new(("ggo-sprite-clip-delete", i), IconName::Trash)
                                .icon_size(IconSize::Small)
                                .tooltip(ui::Tooltip::text("Delete clip"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.delete_clip(i, cx)),
                                ),
                        ),
                )
                .child(self.render_sequence_for(i, cx));
            if let Some((at, message)) = &open.clip_error
                && *at == i
            {
                row = row.child(
                    Label::new(SharedString::from(message.clone()))
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                );
            }
            cards = cards.child(row);
        }
        cards = cards.child(
            Button::new("ggo-sprite-clip-add", "+ Clip")
                .on_click(cx.listener(|this, _, _, cx| this.add_clip(cx))),
        );
        div()
            .id("ggo-sprite-clips")
            .flex_none()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .overflow_x_scroll()
            .child(cards)
            .into_any_element()
    }

    /// Frame-op row above the strip: add/duplicate/delete/move buttons
    /// acting on the selected frame, its duration editor, and the
    /// hardware budget line (which used to head the tile palette; the
    /// palette became a narrow side column in F5.2, too narrow for it).
    fn render_frame_ops(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_frame_ops is only called in the Ready state");
        };
        let state = open.store.state();
        let len = state.frames.len();
        let selected = open.selected_frame;
        let meter = tiles::hw_meter_line(state, state.frames.get(selected));
        h_flex()
            .gap_1()
            .p_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("ggo-sprite-frame-left", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Move frame left"))
                    .disabled(selected == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.move_selected_frame(-1, cx))),
            )
            .child(
                IconButton::new("ggo-sprite-frame-right", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Move frame right"))
                    .disabled(selected + 1 >= len)
                    .on_click(cx.listener(|this, _, _, cx| this.move_selected_frame(1, cx))),
            )
            .child(div().flex_1())
            .child(
                Label::new(SharedString::from(meter))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children(open.op_error.as_ref().map(|e| {
                Label::new(SharedString::from(e.clone()))
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .into_any_element()
    }

    /// One clip's play sequence as a thumbnail row, embedded in its own
    /// card -- every clip is directly editable, no activation step. A
    /// library frame dropped on a thumbnail inserts a copy at that
    /// position, the trailing dashed slot appends, clicking a thumbnail
    /// selects (and previews) that frame. The transport's clip dropdown
    /// is playback-range only and never gates this.
    fn render_sequence_for(&self, clip_ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_sequence_for is only called in the Ready state");
        };
        let border = cx.theme().colors().border;
        let accent = cx.theme().colors().border_focused;
        let drop_bg = cx.theme().colors().drop_target_background;
        let state = open.store.state();
        let Some(clip) = state.clips.get(clip_ix) else {
            return div().into_any_element();
        };
        let mut row = h_flex().gap_0p5().items_center();
        let last = clip.to.min(state.frames.len().saturating_sub(1));
        for (seq_pos, ix) in (clip.from..=last).enumerate() {
            let thumb = open.frame_image(ix).map(|image| {
                let (w, h) = image_px_size(&image);
                let (fit_w, fit_h) = playback::fit_size(w, h, THUMB_PX);
                img(image).nearest(true).w(px(fit_w)).h(px(fit_h))
            });
            let duration_ms = state.frames[ix].duration_ms;
            row = row.child(
                v_flex()
                    .id(("ggo-sprite-seq", clip_ix * 1000 + seq_pos))
                    .items_center()
                    .gap_0p5()
                    .p_0p5()
                    .border_1()
                    .rounded_sm()
                    .border_color(if open.selected_frame == ix { accent } else { border })
                    .debug_selector(|| format!("ggo-sprite-seq-{clip_ix}-{seq_pos}"))
                    .child(
                        div()
                            .w(px(THUMB_PX))
                            .h(px(THUMB_PX))
                            .flex()
                            .justify_center()
                            .items_center()
                            .children(thumb),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .items_center()
                            .child(
                                Label::new(format!("{duration_ms} ms"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new(
                                    ("ggo-sprite-frame-dup", clip_ix * 1000 + seq_pos),
                                    IconName::Copy,
                                )
                                .icon_size(IconSize::XSmall)
                                .tooltip(ui::Tooltip::text("Duplicate frame"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.duplicate_frame_in_clip(clip_ix, seq_pos, cx);
                                })),
                            )
                            .child(
                                IconButton::new(
                                    ("ggo-sprite-frame-del", clip_ix * 1000 + seq_pos),
                                    IconName::Trash,
                                )
                                .icon_size(IconSize::XSmall)
                                .tooltip(ui::Tooltip::text("Delete frame"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_frame_in_clip(clip_ix, seq_pos, cx);
                                })),
                            )
                            .child(
                                IconButton::new(
                                    ("ggo-sprite-frame-settings", clip_ix * 1000 + seq_pos),
                                    IconName::Ellipsis,
                                )
                                .icon_size(IconSize::XSmall)
                                .tooltip(ui::Tooltip::text("Frame settings"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_frame_settings(
                                        clip_ix,
                                        ix,
                                        window.mouse_position(),
                                        cx,
                                    );
                                })),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select_frame(ix, cx)))
                    .drag_over::<DraggedFrame>(move |cell, _, _, _| cell.bg(drop_bg))
                    .on_drop(cx.listener(move |this, dragged: &DraggedFrame, _, cx| {
                        this.insert_frame_in_clip(clip_ix, seq_pos, dragged.ix, cx);
                    })),
            );
        }
        row = row.child(
            div()
                .id(("ggo-sprite-seq-append", clip_ix))
                .w(px(THUMB_PX))
                .h(px(THUMB_PX / 2.))
                .flex()
                .justify_center()
                .items_center()
                .border_1()
                .border_dashed()
                .rounded_sm()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new("Drop frame")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .drag_over::<DraggedFrame>(move |slot, _, _, _| slot.bg(drop_bg))
                .on_drop(cx.listener(move |this, dragged: &DraggedFrame, _, cx| {
                    this.drop_frame_on_clip(clip_ix, dragged.ix, cx);
                })),
        );
        row.into_any_element()
    }

    /// The frames LIBRARY, a right-dock column beside the tile picker:
    /// one thumbnail + name + duration per frame, click to select (and
    /// preview), double-click to name, drag out to reorder or to build a
    /// clip sequence. Its header carries the create/duplicate/delete
    /// buttons so the area is self-contained.
    fn render_strip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_strip is only called in the Ready state");
        };
        let selected = open.selected_frame;
        let border = cx.theme().colors().border;
        let accent = cx.theme().colors().border_focused;
        let drop_bg = cx.theme().colors().drop_target_background;
        let state = open.store.state();
        let frame_count = state.frames.len();
        let names = &open.frame_names;
        let header = h_flex()
            .gap_1()
            .px_1()
            .pt_1()
            .items_center()
            .child(Label::new("Frames").size(LabelSize::Small).color(Color::Muted))
            .child(
                IconButton::new("ggo-sprite-frame-add", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Add blank frame"))
                    .on_click(cx.listener(|this, _, _, cx| this.add_blank_frame(cx))),
            )
            .child(
                IconButton::new("ggo-sprite-frame-dup", IconName::Copy)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Duplicate frame"))
                    .on_click(cx.listener(|this, _, _, cx| this.duplicate_selected_frame(cx))),
            )
            .child(
                IconButton::new("ggo-sprite-frame-delete", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Delete frame"))
                    .disabled(frame_count <= 1)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected_frame(cx))),
            );
        let strip = div()
            .id("ggo-sprite-strip")
            .flex_1()
            .min_h_0()
            .p_1()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .gap_1()
                    .children(edits::library_indices(&state.frames).into_iter().map(|ix| {
                        let thumb = open.frames.get(ix).map(|image| {
                            let (w, h) = image_px_size(image);
                            let (fit_w, fit_h) = playback::fit_size(w, h, THUMB_PX);
                            img(image.clone()).nearest(true).w(px(fit_w)).h(px(fit_h))
                        });
                        let label = editor_meta::frame_label(names, ix);
                        v_flex()
                            .id(("ggo-sprite-frame", ix))
                            // Test-only bounds hook (a no-op in release
                            // builds): the drag-reorder test aims real
                            // mouse events at the cells.
                            .debug_selector(|| format!("ggo-sprite-frame-{ix}"))
                            .items_center()
                            .gap_0p5()
                            .p_0p5()
                            .border_1()
                            .rounded_sm()
                            .border_color(if ix == selected { accent } else { border })
                            .child(
                                div()
                                    .w(px(THUMB_PX))
                                    .h(px(THUMB_PX))
                                    .flex()
                                    .justify_center()
                                    .items_center()
                                    .children(thumb),
                            )
                            .child(
                                Label::new(label.clone())
                                    .size(LabelSize::XSmall)
                                    .color(if names.get(ix).is_some_and(|n| !n.is_empty()) {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    }),
                            )
                            // Single click selects; double click names.
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                if event.click_count() > 1 {
                                    this.begin_name_frame(ix, window, cx);
                                } else {
                                    this.select_frame(ix, cx);
                                }
                            }))
                            .on_drag(
                                DraggedFrame {
                                    ix,
                                    label: label.into(),
                                },
                                |frame, _, _, cx| cx.new(|_| frame.clone()),
                            )
                            .drag_over::<DraggedFrame>(move |cell, _, _, _| cell.bg(drop_bg))
                            .on_drop(cx.listener(move |this, dragged: &DraggedFrame, _, cx| {
                                this.move_frame_to(dragged.ix, ix, cx);
                            }))
                    })),
            );
        v_flex()
            .flex_none()
            .w(CLIPS_WIDTH)
            .h_full()
            .border_l_1()
            .border_color(border)
            .child(header)
            .child(strip)
            .into_any_element()
    }

    /// The per-frame settings popup: an anchored card at the "..."
    /// click, hosting the SELECTED frame's duration and affine transform
    /// editors plus the owning clip's From/To range. Click-away or
    /// Escape dismisses it (pending editor text blur-commits as the
    /// editors leave focus).
    fn render_frame_settings(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let (clip_ix, position) = open.frame_settings?;
        let editor_for = |target: EditTarget| {
            open.editors
                .iter()
                .find(|e| e.target == target)
                .map(|e| e.editor.clone())
        };
        let labelled = |label: &'static str, width: f32, target: EditTarget| {
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    div().w(px(28.)).child(
                        Label::new(label).size(LabelSize::XSmall).color(Color::Muted),
                    ),
                )
                .child(
                    div()
                        .w(px(width))
                        .flex_none()
                        .children(editor_for(target).map(|e| Self::editor_input(e, cx))),
                )
        };
        let card = v_flex()
            .id("ggo-sprite-frame-settings-popup")
            .debug_selector(|| "ggo-sprite-frame-settings-popup".into())
            .occlude()
            .p_1()
            .gap_0p5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_md()
            .child(
                Label::new(format!("Frame {}", open.selected_frame + 1))
                    .size(LabelSize::XSmall),
            )
            .child(labelled("ms", 56., EditTarget::Duration))
            .child(labelled("rot", 48., EditTarget::Rot))
            .child(labelled("sx", 56., EditTarget::ScaleX))
            .child(labelled("sy", 56., EditTarget::ScaleY))
            .child(labelled("shx", 56., EditTarget::ShearX))
            .child(labelled("shy", 56., EditTarget::ShearY))
            .child(
                Label::new("Clip range")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(labelled("from", 40., EditTarget::ClipFrom(clip_ix)))
            .child(labelled("to", 40., EditTarget::ClipTo(clip_ix)))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_frame_settings(cx)));
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(position)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .child(self.render_transport(window, cx))
            .child(self.render_onion(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_preview(cx))
                    .child(self.render_tile_picker(cx))
                    .child(self.render_strip(cx)),
            )
            .child(self.render_frame_ops(cx))
            .child(self.render_clips(cx))
            .children(self.render_frame_settings(cx))
            .into_any_element()
    }
}

/// A frame thumbnail mid-drag: the strip reorders via `DocOp::FrameMove`
/// on drop (workspace's `DraggedTab` shape -- the entity IS the drag
/// ghost).
#[derive(Clone)]
struct DraggedFrame {
    ix: usize,
    label: SharedString,
}

impl Render for DraggedFrame {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_1()
            .py_0p5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .child(Label::new(self.label.clone()).size(LabelSize::XSmall))
    }
}

/// Reorder an editor-name list the way `DocOp::FrameMove`'s splice
/// reorders frames: remove at `from`, insert at `to`. No-op when either
/// index is past the list (names may lag the frame count; the refresh
/// pad squares that up).
fn move_name(names: &mut Vec<String>, from: usize, to: usize) {
    if from < names.len() && to < names.len() {
        let name = names.remove(from);
        names.insert(to, name);
    }
}

/// A `RenderImage`'s pixel size (frame 0 -- worldlib composes are always
/// single-frame).
fn image_px_size(image: &Arc<RenderImage>) -> (u32, u32) {
    let size = image.size(0);
    (size.width.0 as u32, size.height.0 as u32)
}

/// Paint 1px lines over `bounds` marking every tile-cell boundary of a
/// `cols x rows` grid, outer edges included -- the preview and picker
/// overlays share it so both surfaces show where one tile ends and the
/// next begins. The `min` clamps keep the far edges' lines inside the box.
fn paint_tile_grid(
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

impl Render for SpritePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_editors(window, cx);
        let body = match &self.state {
            ViewerState::Empty => self.render_message(EMPTY_MESSAGE.to_string(), cx),
            ViewerState::Loading { rel_path } => {
                self.render_message(format!("Loading {rel_path}…"), cx)
            }
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(window, cx),
        };
        let form = self.render_form(window, cx);
        v_flex()
            .key_context(self.dispatch_context(window, cx))
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &PlayPause, _window, cx| this.toggle_play(cx)))
            .on_action(cx.listener(|this, _: &Undo, _window, cx| this.undo_impl(cx)))
            .on_action(cx.listener(|this, _: &Redo, _window, cx| this.redo_impl(cx)))
            .on_action(cx.listener(|this, _: &Save, _window, cx| this.save_impl(cx)))
            .on_action(cx.listener(|this, _: &DeselectTile, _window, cx| {
                // Escape dismisses an open form first: while one is up it
                // is the thing the user is looking at, and a form that
                // could only be closed by its Cancel button would be a
                // trap the rest of the panel's Escape handling isn't.
                if !this.cancel_form(cx) {
                    this.deselect_tile(cx);
                }
            }))
            .on_action(cx.listener(Self::on_commit_field))
            .bg(cx.theme().colors().panel_background)
            .children(form)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for SpritePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}


/// The `.spr`/`.til`/`.pal` fixture trio the crate's tests (and
/// `sprite_item`'s) write to a real-fs temp project: a 1x1-tile,
/// 2-frame, 2-tile sprite with one clip.
#[cfg(test)]
pub(crate) mod test_fixtures {
    use ggo_worldlib::sprites::cow::{ClipEdit, Frame, FrameTransform, SpriteState};
    use ggo_worldlib::sprites::hw::TILE_BYTES;
    use ggo_worldlib::sprites::io::save_sprite;

    pub(crate) fn write_sprite_fixture(root: &std::path::Path) {
        write_sprite_fixture_named(root, "hero");
    }

    /// [`write_sprite_fixture`] under an arbitrary stem, so a test can hold
    /// two distinct sprites (the routing tests switch between them).
    pub(crate) fn write_sprite_fixture_named(root: &std::path::Path, stem: &str) {
        write_sprite_fixture_at(root, &format!("sprites/{stem}"));
    }

    /// [`write_sprite_fixture`] at an arbitrary root-relative stem, so a
    /// test can put the trio DIRECTLY at an asset root (stem `hh` ->
    /// `hh.spr`/`hh.til`/`hh.pal`) rather than under `sprites/`. That is
    /// the wilds layout, and the one where a wrong root is unrecoverable:
    /// with no subdirectory there is no sibling for `resolve_sidecar` to
    /// fall back to.
    pub(crate) fn write_sprite_fixture_at(root: &std::path::Path, stem: &str) {
        save_fixture(
            root,
            &format!("{stem}.spr"),
            &format!("{stem}.til"),
            &format!("{stem}.pal"),
        );
    }

    /// The fixture trio with each of the three rels named INDEPENDENTLY, so
    /// a test can reproduce a `.spr` whose stored sidecars don't match the
    /// root they'll later be read against -- i.e. the exact corruption this
    /// change exists to stop, produced by the exact call that caused it
    /// (`save_sprite` handed a PROJECT root where the asset root belonged).
    /// A trio with TWO pickable tiles: pool = [blank, red 0x11, green
    /// 0x22], one 1x1 frame on tile 0. The picker hides blanks, so tests
    /// that need a real multi-tile sheet (marquee, wrap) use this.
    pub(crate) fn write_multi_tile_fixture(root: &std::path::Path) {
        use ggo_worldlib::sprites::cow::{Frame, SpriteState};
        use ggo_worldlib::sprites::hw::TILE_BYTES;
        use ggo_worldlib::sprites::io::save_sprite;

        let mut pool = vec![0u8; 3 * TILE_BYTES];
        for b in &mut pool[TILE_BYTES..2 * TILE_BYTES] {
            *b = 0x11;
        }
        for b in &mut pool[2 * TILE_BYTES..] {
            *b = 0x22;
        }
        let mut palette = [0u16; 16];
        palette[1] = 0xF800;
        palette[2] = 0x07E0;
        let state = SpriteState {
            pool,
            tile_count: 3,
            session_tiles: std::collections::HashSet::new(),
            palette,
            frames: vec![Frame {
                map: vec![0],
                duration_ms: 100,
                transform: FrameTransform::IDENTITY,
            }],
            clips: vec![],
            w_tiles: 1,
            h_tiles: 1,
            pool_shared: false,
        };
        save_sprite(root, "sprites/multi.spr", &state, "sprites/multi.til", "sprites/multi.pal")
            .unwrap();
    }

    pub(crate) fn save_fixture(root: &std::path::Path, spr_rel: &str, til_rel: &str, pal_rel: &str) {
        let mut pool = vec![0u8; 2 * TILE_BYTES];
        for b in &mut pool[TILE_BYTES..] {
            *b = 0x11; // both nibbles = palette index 1
        }
        let mut palette = [0u16; 16];
        palette[1] = 0xF800; // pure 565 red
        let state = SpriteState {
            pool,
            tile_count: 2,
            session_tiles: std::collections::HashSet::new(),
            palette,
            frames: vec![
                Frame {
                    map: vec![0],
                    duration_ms: 100,
                    transform: FrameTransform::IDENTITY,
                },
                Frame {
                    map: vec![1],
                    duration_ms: 200,
                    transform: FrameTransform::IDENTITY,
                },
            ],
            clips: vec![ClipEdit {
                name: "walk".to_string(),
                from: 1,
                to: 1,
                loop_: false,
            }],
            w_tiles: 1,
            h_tiles: 1,
            pool_shared: false,
        };
        save_sprite(root, spr_rel, &state, til_rel, pal_rel).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{save_fixture, write_sprite_fixture, write_sprite_fixture_at, write_sprite_fixture_named};
    use super::*;
    use ggo_worldlib::sprites::cow::ClipEdit;
    use ggo_worldlib::sprites::hw::{TILE_BYTES, TILE_PX};
    use ggo_worldlib::sprites::io::{open_sprite, save_tileset};
    use ggo_worldlib::sprites::sprite_doc::DEFAULT_FRAME_DURATION_MS;
    use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
    use ggo_worldlib::sprites::timeline_ops::MIN_FRAME_MS;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, WorktreeId};
    use workspace::{AppState, MultiWorkspace};

    /// [`new_sprite_rel`]'s stem rules: [`rename_target`]'s refusals aimed
    /// at a directory, a retyped `.spr` extension accepted without
    /// doubling, and the clicked dir joined in.
    #[test]
    fn new_sprite_rel_applies_the_stem_rules() {
        assert_eq!(
            new_sprite_rel("assets/sprites", "hero"),
            Ok("assets/sprites/hero.spr".to_string())
        );
        assert_eq!(
            new_sprite_rel("assets/sprites", "hero.spr"),
            Ok("assets/sprites/hero.spr".to_string())
        );
        assert_eq!(new_sprite_rel("", "hero"), Ok("hero.spr".to_string()));
        for bad in &["", "  ", "a/b", "a\\b", ".", "..", ".spr"] {
            assert!(
                new_sprite_rel("assets/sprites", bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Author a real 2-frame sprite trio (`.spr`/`.til`/`.pal`) via
    /// worldlib's own `save_sprite` -- one call persists all three files
    /// (the pool IS the tileset; `save_tileset` would only rewrite the
    /// same `.til`/`.pal` pair, so it isn't needed). Frame 0 shows the
    /// all-transparent tile 0; frame 1 shows tile 1, filled with palette
    /// index 1 (red) -- distinguishable thumbnails. One non-looping
    /// single-frame clip exercises the clip selector path.
    /// [`ready_panel`] for the multi-tile trio (two pickable tiles).
    async fn ready_multi_panel(
        cx: &mut TestAppContext,
        root: &std::path::Path,
    ) -> gpui::Entity<SpritePanel> {
        super::test_fixtures::write_multi_tile_fixture(root);
        let root = root.to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = SpritePanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("sprites/multi.spr", cx);
        });
        cx.executor().run_until_parked();
        panel
    }

    /// Load the fixture sprite into a fresh panel and return it Ready.
    async fn ready_panel(
        cx: &mut TestAppContext,
        root: &std::path::Path,
    ) -> gpui::Entity<SpritePanel> {
        write_sprite_fixture(root);
        let root = root.to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = SpritePanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("sprites/hero.spr", cx);
        });
        cx.executor().run_until_parked();
        panel
    }

    /// [`ready_panel`] drawn in a real test window with the crate's
    /// keybindings installed, for tests that drive rendered gestures and
    /// keystrokes (world_panel's `ready_panel_in_window` shape).
    async fn ready_panel_in_window<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (gpui::Entity<SpritePanel>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        write_sprite_fixture(root);
        let root = root.to_path_buf();
        let (panel, cx) = cx.add_window_view(|_, cx| {
            let mut panel = SpritePanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        // Focus in/out events only fire while the window is ACTIVE -- an
        // inactive window's focus paths are blanked in the draw's focus
        // phase (world_panel's rule).
        cx.update(|window, _| window.activate_window());
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("sprites/hero.spr", cx);
        });
        cx.run_until_parked();
        (panel, cx)
    }

    /// End-to-end viewer load against a real-fs temp project: opening the
    /// fixture `.spr` by rel path runs the off-thread
    /// loader, and the panel reaches Ready with one composed thumbnail
    /// per frame (frame 0 all-transparent, frame 1 opaque red -- proving
    /// per-frame compose order survived the BGRA bridge) and the resolved
    /// sidecar rels the save path writes back to (M5's `LoadedSprite`
    /// extension).
    #[gpui::test]
    async fn test_select_sprite_reaches_ready_with_frame_thumbnails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready state after load");
            };
            assert_eq!(open.rel_path, "sprites/hero.spr");
            assert_eq!(open.til_path, "sprites/hero.til");
            assert_eq!(open.pal_path, "sprites/hero.pal");
            let state = open.store.state();
            assert_eq!(state.frames.len(), 2);
            assert_eq!(open.frames.len(), 2, "one thumbnail per frame");
            assert_eq!(state.frames[0].duration_ms, 100);
            assert_eq!(state.frames[1].duration_ms, 200);
            assert_eq!(state.clips.len(), 1);
            assert_eq!(open.shown_frame(), 0, "not playing => selected frame");
            assert!(!open.store.dirty(), "freshly opened => clean");

            let px_count = TILE_PX * TILE_PX;
            let f0 = open.frames[0].as_bytes(0).unwrap();
            assert_eq!(f0.len(), px_count * 4);
            assert!(
                f0.chunks_exact(4).all(|p| p[3] == 0),
                "frame 0 (tile 0, all index 0) must be fully transparent"
            );
            let f1 = open.frames[1].as_bytes(0).unwrap();
            assert!(
                f1.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                "frame 1 (tile 1, palette red) must be opaque red in BGRA"
            );
        });
    }

    /// Frame selection + clip selection state: strip clicks move the
    /// selected (and shown) frame, out-of-range clicks are ignored, and
    /// the clip selector round-trips through `active_clip`.
    #[gpui::test]
    async fn test_frame_and_clip_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.select_frame(1, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected_frame, 1);
            assert_eq!(open.shown_frame(), 1);

            panel.select_frame(9, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected_frame, 1, "out-of-range click is ignored");

            panel.select_clip(Some(0), cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.active_clip, Some(0));
            assert_eq!(
                playback::play_range(&open.store.state().clips, open.active_clip, 2),
                (1, 1)
            );

            panel.select_clip(None, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.active_clip, None);
        });
    }

    /// The five transform editors follow the Duration editor's plumbing:
    /// a parsed commit merges ONE field into the selected frame's current
    /// transform through `DocOp::FrameTransformSet` (one undo step per
    /// commit), junk and unchanged commits push no op, and the unfocused
    /// display text reads back from the doc.
    #[gpui::test]
    async fn test_transform_editors_commit_through_the_doc(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.commit_edit(EditTarget::Rot, "90".into(), cx);
            assert_eq!(ready(panel).store.state().frames[0].transform.angle256, 64);
            panel.commit_edit(EditTarget::ScaleX, "2.5".into(), cx);
            {
                let t = ready(panel).store.state().frames[0].transform;
                assert_eq!(t.sx, 0x0280);
                assert_eq!(t.angle256, 64, "a field commit merges, not replaces");
            }
            assert_eq!(
                SpritePanel::edit_display_text(&EditTarget::Rot, ready(panel).store.state(), 0),
                "90"
            );
            assert_eq!(
                SpritePanel::edit_display_text(&EditTarget::ScaleX, ready(panel).store.state(), 0),
                "2.50"
            );

            // Only the SELECTED frame's transform moves.
            panel.select_frame(1, cx);
            panel.commit_edit(EditTarget::ShearY, "-0.25".into(), cx);
            {
                let state = ready(panel).store.state();
                assert_eq!(state.frames[1].transform.shear_y, -0x0040);
                assert_eq!(state.frames[0].transform.sx, 0x0280);
            }

            // Junk reverts like the duration editor: no op, doc untouched.
            panel.commit_edit(EditTarget::ScaleY, "junk".into(), cx);
            panel.commit_edit(EditTarget::Rot, "junk".into(), cx);
            // An unchanged commit pushes no undo entry either.
            panel.commit_edit(EditTarget::ShearY, "-0.25".into(), cx);

            // Undo unwinds exactly the three real commits, latest first.
            panel.undo_impl(cx);
            assert_eq!(ready(panel).store.state().frames[1].transform.shear_y, 0);
            panel.undo_impl(cx);
            {
                let t = ready(panel).store.state().frames[0].transform;
                assert_eq!(t.sx, 0x0100, "scale commit undone");
                assert_eq!(t.angle256, 64, "one field per op");
            }
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().frames[0].transform,
                FrameTransform::IDENTITY
            );
        });
    }

    /// The big preview composes the SHOWN frame through the transformed
    /// composer: a rotated frame renders on the doubled (DOUBLE_SIZE)
    /// canvas and is served from the per-shown-frame cache on repeat
    /// paints, while identity frames keep the legacy thumbnail image
    /// (same Arc as the strip).
    #[gpui::test]
    async fn test_preview_shows_the_transformed_frame(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                let image = open.preview_image().expect("identity preview");
                assert!(
                    Arc::ptr_eq(&image, &open.frames[0]),
                    "identity frames reuse the legacy composed image"
                );
                assert_eq!(image_px_size(&image), (TILE_PX as u32, TILE_PX as u32));
            }
            panel.commit_edit(EditTarget::Rot, "90".into(), cx);
            {
                let open = ready(panel);
                let image = open.preview_image().expect("transformed preview");
                assert_eq!(
                    image_px_size(&image),
                    (2 * TILE_PX as u32, 2 * TILE_PX as u32),
                    "non-identity -> the doubled canvas"
                );
                assert_eq!(
                    image_px_size(&open.frames[0]),
                    (TILE_PX as u32, TILE_PX as u32),
                    "the strip thumbnail stays legacy-composed"
                );
                let again = open.preview_image().expect("cached recompose");
                assert!(Arc::ptr_eq(&image, &again), "repeat paints hit the cache");
            }
            // Selecting an identity frame drops back to legacy dims.
            panel.select_frame(1, cx);
            assert_eq!(
                image_px_size(&ready(panel).preview_image().expect("identity again")),
                (TILE_PX as u32, TILE_PX as u32)
            );
        });
    }

    /// The single selected tile, in the shape the pre-marquee tests
    /// asserted -- `None` when nothing (or a bigger block) is selected.
    fn selected_single(panel: &SpritePanel) -> Option<u16> {
        ready(panel).selection.as_ref().and_then(tiles::TileBlock::single)
    }

    fn ready(panel: &SpritePanel) -> &OpenSprite {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
    }

    /// The onion controls reach the open document's state, and the ghost
    /// list the preview draws follows them. The frame SELECTION rule is
    /// worldlib's and is tested in `onion`; this is the wiring.
    #[gpui::test]
    async fn test_onion_controls_drive_the_preview_ghosts(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Off by default: no ghosts, whatever the counts say.
            assert!(!ready(panel).onion.on);
            assert!(ready(panel).ghosts().is_empty());

            panel.update_onion(onion::OnionState::toggle, cx);
            assert!(ready(panel).onion.on);
            // Fixture: 2 frames, selected 0 -> the one frame ahead ghosts.
            let ghosts = ready(panel).ghosts();
            assert_eq!(ghosts.len(), 1);
            assert_eq!(ghosts[0].idx, 1);
            assert!((ghosts[0].alpha - onion::DEFAULT_OPACITY).abs() < 1e-6);

            // The opacity stepper moves the alpha the preview draws at.
            panel.update_onion(|s| s.step_opacity(2), cx);
            let ghosts = ready(panel).ghosts();
            assert!(
                (ghosts[0].alpha - (onion::DEFAULT_OPACITY + 2.0 * onion::OPACITY_STEP)).abs()
                    < 1e-6
            );

            // Zeroing the forward count empties the list even while on.
            panel.update_onion(|s| s.step_fwd(-1), cx);
            assert_eq!(ready(panel).onion.fwd, 0);
            assert!(ready(panel).ghosts().is_empty());

            // Selecting frame 1 brings the BACK ghost in instead.
            panel.update_onion(|s| s.step_back(1), cx);
            panel.select_frame(1, cx);
            let ghosts = ready(panel).ghosts();
            assert_eq!(ghosts.iter().map(|g| g.idx).collect::<Vec<_>>(), vec![0]);

            // An active clip confines the walk: clip 0 is the single frame
            // 1, so nothing neighbours it inside the clip.
            panel.select_clip(Some(0), cx);
            assert!(
                ready(panel).ghosts().is_empty(),
                "a one-frame clip has no neighbours to ghost"
            );
        });
    }

    // ------------------------------------------- unsaved-document guard

    /// Dirty the open sprite (retime frame 0) so the close guard has
    /// something to protect.
    fn dirty_the_sprite(panel: &Entity<SpritePanel>, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            assert!(panel.apply_doc(DocOp::FrameDuration { at: 0, ms: 500 }, cx));
            assert!(
                panel.dirty_sprite_name().is_some(),
                "op should dirty the doc"
            );
        });
    }

    /// Read frame 0's duration straight off disk -- the fixture writes
    /// 100ms, [`dirty_the_sprite`] retimes it to 500ms in memory only.
    fn on_disk_duration(root: &std::path::Path) -> u16 {
        open_sprite(root, "sprites/hero.spr").unwrap().state.frames[0].duration_ms
    }

    // ------------------------------------------ explorer-driven routing

    /// A fake-fs project with one visible worktree holding the same file
    /// names the real-fs `root` fixture does: the interceptor only needs a
    /// worktree id and a rel path, while the panel loads the actual sprite
    /// bytes through `std::fs` from `root` (`root_override`).
    async fn routed_project(
        cx: &mut TestAppContext,
        root: &std::path::Path,
        run_init: bool,
    ) -> Entity<Project> {
        write_sprite_fixture(root);
        write_sprite_fixture_named(root, "other");
        cx.update(|cx| {
            AppState::test(cx);
            if run_init {
                init(cx);
            }
        });

        // Mount the fake worktree AT the real-fs fixture root: interceptors
        // read rel paths off the worktree, while item-created panels
        // resolve their project root from the worktree's abs path and then
        // do real `std::fs` work there -- one path serves both.
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root.to_str().expect("utf8 tempdir path"),
            serde_json::json!({
                "sprites": { "hero.spr": "", "other.spr": "", "hero.til": "" },
                "notes.txt": "",
            }),
        )
        .await;
        Project::test(fs, [root], cx).await
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

    /// With nothing registered, `intercept_path_open` claims nothing --
    /// i.e. the upstream open path (editor tab) is completely unchanged for
    /// every file, `.spr` included. `init` is deliberately NOT run here.
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
                &project_path(worktree_id, "sprites/hero.spr"),
                window,
                cx,
            )
        });
        assert!(
            !claimed,
            "an empty registry must never claim a path -- upstream behaviour byte for byte"
        );
    }

    /// The registered `.spr` predicate claims the path (so the project
    /// panel opens NO pane item for it), opens the dock, and loads the
    /// sprite. A non-`.spr` in the same worktree is declined.
    #[gpui::test]
    async fn test_spr_click_opens_one_item_per_file_and_refocuses(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);

        let open_rels = |workspace: &Entity<Workspace>, cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<sprite_item::SpriteEditorItem>(cx)
                    .map(|item| item.read(cx).rel().to_string())
                    .collect::<Vec<_>>()
            })
        };

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "sprites/hero.spr"),
                window,
                cx,
            )
        });
        assert!(claimed, "a .spr must be claimed, suppressing the raw editor");
        cx.run_until_parked();
        assert_eq!(open_rels(&workspace, cx), vec!["sprites/hero.spr"]);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "sprites/other.spr"),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            open_rels(&workspace, cx),
            vec!["sprites/hero.spr", "sprites/other.spr"],
            "a second sprite opens a second tab"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "sprites/hero.spr"),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            open_rels(&workspace, cx).len(),
            2,
            "re-opening an open sprite must not duplicate its tab"
        );
        workspace.read_with(cx, |workspace, cx| {
            let active_rel = workspace
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<sprite_item::SpriteEditorItem>())
                .map(|item| item.read(cx).rel().to_string());
            assert_eq!(
                active_rel.as_deref(),
                Some("sprites/hero.spr"),
                "re-open focuses the existing tab"
            );
        });

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "notes.txt"), window, cx)
        });
        assert!(!claimed, "everything but .spr opens the normal way");
    }

    /// A clean panel switches documents without a prompt.
    #[gpui::test]
    async fn test_open_rel_path_switches_a_clean_panel_directly(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture_named(dir.path(), "other");
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("sprites/other.spr", window, cx)
            })
        });
        assert!(
            !cx.has_pending_prompt(),
            "a clean panel must switch without asking"
        );
        cx.run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert_eq!(ready(panel).rel_path, "sprites/other.spr");
        });
    }

    /// Clicking the file that is ALREADY open must be a pure focus/reveal:
    /// no prompt (a dirty doc would otherwise be offered a "Don't Save" the
    /// user never asked for) and no reload (which would silently drop the
    /// undo stack). The undo assertion is the load-bearing one -- a reload
    /// would leave the frame at its on-disk 100ms with nothing to undo.
    #[gpui::test]
    async fn test_open_rel_path_on_the_open_sprite_does_not_reload(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_sprite(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("sprites/hero.spr", window, cx)
            })
        });
        assert!(
            !cx.has_pending_prompt(),
            "re-opening the open sprite must not prompt"
        );
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            let open = ready(panel);
            assert_eq!(
                open.store.state().frames[0].duration_ms,
                500,
                "the in-memory edit must survive an already-open click"
            );
            assert!(open.store.dirty(), "and the doc must still be dirty");

            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().frames[0].duration_ms,
                100,
                "the undo stack must have survived too"
            );
        });
    }

    /// The data-loss guard: a file-tree click while the open sprite has
    /// unsaved edits must PROMPT, and Cancel must abort the open -- the
    /// previously loaded document stays loaded, stays dirty, and stays
    /// unwritten.
    #[gpui::test]
    async fn test_open_rel_path_cancel_keeps_the_dirty_document(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture_named(dir.path(), "other");
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_sprite(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("sprites/other.spr", window, cx)
            })
        });
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some("sprites/hero.spr contains unsaved edits. Do you want to save it?".to_string()),
            "switching away from a dirty sprite must prompt first"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            assert_eq!(
                open.rel_path, "sprites/hero.spr",
                "Cancel must abort the open and leave the current sprite loaded"
            );
            assert!(open.store.dirty(), "and leave its edits in place");
        });
        assert_eq!(
            on_disk_duration(dir.path()),
            100,
            "Cancel must not have written the file"
        );
    }

    /// The prompt's middle path: answering "Save" writes the dirty
    /// document FIRST and only then proceeds with the open -- the hero
    /// trio on disk carries the edit, and the panel lands on the new
    /// sprite clean.
    #[gpui::test]
    async fn test_open_rel_path_save_prompt_writes_then_switches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture_named(dir.path(), "other");
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_sprite(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("sprites/other.spr", window, cx)
            })
        });
        assert!(
            cx.has_pending_prompt(),
            "switching away from a dirty sprite must prompt first"
        );
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();

        assert_eq!(
            on_disk_duration(dir.path()),
            500,
            "\"Save\" must have written the edit before switching"
        );
        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            assert_eq!(
                open.rel_path, "sprites/other.spr",
                "the open proceeds once the save landed"
            );
            assert!(!open.store.dirty(), "the fresh document starts clean");
        });
    }

    /// Clip CRUD round trip through the store: add with ggo-ide's
    /// defaults, rename (trim + apply), retarget the range, toggle loop,
    /// delete (clearing the active selection) -- then undo each step back
    /// to the fixture and redo forward again, with the dirty flag
    /// tracking the whole way.
    #[gpui::test]
    async fn test_clip_add_edit_delete_undo_round_trip(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.add_clip(cx);
            {
                let open = ready(panel);
                let clips = &open.store.state().clips;
                assert_eq!(clips.len(), 2);
                assert_eq!(
                    clips[1],
                    ClipEdit {
                        name: "clip2".into(), // fixture has 1 clip -> clip{len+1}
                        from: 0,              // selected frame
                        to: 0,
                        loop_: false
                    }
                );
                assert!(open.store.dirty());
            }

            panel.commit_edit(EditTarget::ClipName(1), "  run  ".into(), cx);
            assert_eq!(ready(panel).store.state().clips[1].name, "run");

            panel.commit_edit(EditTarget::ClipTo(1), "1".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips[1].to, 1);
                assert!(open.clip_error.is_none());
            }

            panel.set_clip_loop(1, true, cx);
            assert!(ready(panel).store.state().clips[1].loop_);

            panel.select_clip(Some(1), cx);
            panel.delete_clip(1, cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips.len(), 1);
                assert_eq!(
                    open.active_clip, None,
                    "deleting the active clip clears the selection"
                );
            }

            panel.undo_impl(cx); // un-delete
            assert!(ready(panel).store.state().clips[1].loop_);
            panel.undo_impl(cx); // un-loop
            assert!(!ready(panel).store.state().clips[1].loop_);
            panel.undo_impl(cx); // un-retarget
            assert_eq!(ready(panel).store.state().clips[1].to, 0);
            panel.undo_impl(cx); // un-rename
            assert_eq!(ready(panel).store.state().clips[1].name, "clip2");
            panel.undo_impl(cx); // un-add
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips.len(), 1);
                assert!(
                    !open.store.dirty(),
                    "undo back to the loaded state => clean"
                );
            }

            panel.redo_impl(cx);
            assert_eq!(ready(panel).store.state().clips[1].name, "clip2");
        });
    }

    /// Clip-edit guards: an out-of-range or reversed typed range is shown
    /// inline and NOT applied; unparsable text is dropped silently; stale
    /// clip indices (undo raced a click) are ignored everywhere instead of
    /// reaching the store's panicking arms.
    #[gpui::test]
    async fn test_clip_edit_guards_and_stale_indices(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Fixture clip walk = (1, 1) over 2 frames.
            panel.commit_edit(EditTarget::ClipTo(0), "9".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips[0].to, 1, "not applied");
                let (at, msg) = open.clip_error.as_ref().expect("inline error set");
                assert_eq!(*at, 0);
                assert!(msg.contains("outside frames"), "range message: {msg}");
            }

            panel.commit_edit(EditTarget::ClipTo(0), "0".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(
                    open.store.state().clips[0].to,
                    1,
                    "reversed range not applied"
                );
                let (_, msg) = open.clip_error.as_ref().expect("inline error set");
                assert!(msg.contains('>'), "inversion message: {msg}");
            }

            panel.commit_edit(EditTarget::ClipFrom(0), "abc".into(), cx);
            assert_eq!(
                ready(panel).store.state().clips[0].from,
                1,
                "unparsable text dropped"
            );

            // A valid edit applies and clears the inline error.
            panel.commit_edit(EditTarget::ClipFrom(0), "0".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips[0].from, 0);
                assert!(open.clip_error.is_none());
            }

            // Whitespace-only rename is dropped; over-long is byte-clamped.
            panel.commit_edit(EditTarget::ClipName(0), "   ".into(), cx);
            assert_eq!(ready(panel).store.state().clips[0].name, "walk");
            let long = "z".repeat(200);
            panel.commit_edit(EditTarget::ClipName(0), long, cx);
            assert_eq!(
                ready(panel).store.state().clips[0].name.len(),
                ggo_worldlib::sprites::sprite_doc::SPR_CLIP_NAME_MAX
            );

            // Stale indices: no panic, no change.
            panel.delete_clip(7, cx);
            panel.set_clip_loop(7, true, cx);
            panel.commit_edit(EditTarget::ClipName(7), "x".into(), cx);
            panel.commit_edit(EditTarget::ClipFrom(7), "0".into(), cx);
            assert_eq!(ready(panel).store.state().clips.len(), 1);

            // Stale FRAME selection: force one, then run every frame op.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selected_frame = 9;
            }
            panel.delete_selected_frame(cx);
            panel.duplicate_selected_frame(cx);
            panel.move_selected_frame(1, cx);
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
            assert_eq!(ready(panel).store.state().frames.len(), 2, "all ignored");
        });
    }

    /// Frame ops through the store: blank add, duplicate (selects the
    /// copy, thumbnail cache refreshed with the copied pixels), adjacent
    /// move (selection follows), duration floor, delete with ggo-ide's
    /// selection rule and last-frame guard -- then undo everything back to
    /// the fixture, thumbnails included.
    #[gpui::test]
    async fn test_frame_ops_with_undo(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Add blank: appended at the end, default duration.
            panel.add_blank_frame(cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 3);
                assert_eq!(state.frames[2].duration_ms, DEFAULT_FRAME_DURATION_MS);
                assert_eq!(open.frames.len(), 3, "thumbnail cache refreshed");
            }

            // Duplicate the red frame 1: copy lands at 2, selected, and
            // its RECOMPOSED thumbnail carries the copied pixels (the M5
            // invalidation hook actually recomposing, not just resizing).
            panel.select_frame(1, cx);
            panel.duplicate_selected_frame(cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 4);
                assert_eq!(open.selected_frame, 2);
                assert_eq!(state.frames[2].duration_ms, 200, "copy_of copies duration");
                let dup = open.frames[2].as_bytes(0).unwrap();
                assert!(
                    dup.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                    "duplicated frame's thumbnail is the copied red frame"
                );
            }

            // Move the copy left: neighbor swap, selection follows.
            panel.move_selected_frame(-1, cx);
            {
                let open = ready(panel);
                let durations: Vec<u16> = open
                    .store
                    .state()
                    .frames
                    .iter()
                    .map(|f| f.duration_ms)
                    .collect();
                assert_eq!(durations, [100, 200, 200, 100]);
                assert_eq!(open.selected_frame, 1);
            }
            // Move left at index 0 is a no-op.
            panel.select_frame(0, cx);
            panel.move_selected_frame(-1, cx);
            assert_eq!(ready(panel).store.state().frames[0].duration_ms, 100);

            // Duration: sub-floor input floors to MIN_FRAME_MS; a real
            // value sticks; the strip label data (doc) and cache agree.
            panel.select_frame(3, cx);
            panel.commit_edit(EditTarget::Duration, "1".into(), cx);
            assert_eq!(
                ready(panel).store.state().frames[3].duration_ms,
                MIN_FRAME_MS
            );
            panel.commit_edit(EditTarget::Duration, "320".into(), cx);
            assert_eq!(ready(panel).store.state().frames[3].duration_ms, 320);

            // Delete the selected last frame: selection clamps to the new
            // end (ggo-ide's rule), fixture clip survives untouched.
            panel.delete_selected_frame(cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames.len(), 3);
                assert_eq!(open.selected_frame, 2);
                assert_eq!(open.frames.len(), 3);
            }

            // Delete down to one, then verify the last-frame guard.
            panel.delete_selected_frame(cx);
            panel.delete_selected_frame(cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames.len(), 1);
                assert_eq!(open.selected_frame, 0);
                assert_eq!(
                    open.store.state().clips.len(),
                    0,
                    "deleting the walk clip's sole frame dropped the clip (store rule)"
                );
            }
            panel.delete_selected_frame(cx);
            assert_eq!(
                ready(panel).store.state().frames.len(),
                1,
                "the last frame can't be deleted"
            );

            // Undo everything: back to the loaded fixture, clean, with the
            // thumbnail cache re-derived from the restored doc.
            for _ in 0..20 {
                panel.undo_impl(cx);
            }
            {
                let open = ready(panel);
                let state = open.store.state();
                let durations: Vec<u16> = state.frames.iter().map(|f| f.duration_ms).collect();
                assert_eq!(durations, [100, 200]);
                assert_eq!(state.clips.len(), 1, "walk clip restored");
                assert_eq!(open.frames.len(), 2);
                assert!(!open.store.dirty());
                let f1 = open.frames[1].as_bytes(0).unwrap();
                assert!(f1.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
            }
        });
    }

    /// Tile setting end to end (M6): the pool-tile palette composes one
    /// pinned-byte thumbnail per tile; selection toggles (re-click and
    /// the Escape action's path both deselect) and ignores stale
    /// indices; a cell click with a tile active applies `FrameTileSet`
    /// on the SELECTED frame and the recomposed frame pixels become the
    /// assigned tile's pixels; unchanged-cell / out-of-range / no-tile
    /// clicks don't touch the store; undo restores the prior bytes and
    /// redo re-applies. Plus the window->cell mapping through manually
    /// stamped preview bounds (the headless panel never paints).
    #[gpui::test]
    async fn test_tile_palette_and_cell_set_with_undo(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                let strip = open.pool_strip.as_ref().expect("picker sheet composed");
                // The fixture's tile 0 is all-zero: the picker hides it,
                // showing ONLY the red tile 1.
                assert_eq!(strip.tiles, vec![1], "blank tile 0 is not offered");
                assert_eq!((strip.cols, strip.rows), (1, 1));
                let sheet = strip.image.as_bytes(0).unwrap();
                assert_eq!(sheet.len(), TILE_PX * TILE_PX * 4);
                assert!(
                    sheet.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                    "the sole shown tile composes opaque red in BGRA"
                );
                assert_eq!(open.selection, None);
            }

            // Selection toggles; stale indices are ignored.
            panel.select_tile(1, cx);
            assert_eq!(selected_single(panel), Some(1));
            panel.select_tile(1, cx);
            assert_eq!(selected_single(panel), None, "re-click deselects");
            panel.select_tile(9, cx);
            assert_eq!(selected_single(panel), None, "stale tile index ignored");
            panel.select_tile(1, cx);

            // Guarded clicks leave the store untouched: out-of-range
            // cell; a cell that already shows the selected tile.
            panel.set_tile_on_cell(5, cx);
            assert!(!ready(panel).store.dirty(), "out-of-range cell dropped");
            panel.select_frame(1, cx); // frame 1's sole cell is already tile 1
            panel.set_tile_on_cell(0, cx);
            assert!(!ready(panel).store.dirty(), "same-tile click drops the op");

            // The real edit: frame 0 cell 0 -> tile 1.
            panel.select_frame(0, cx);
            panel.set_tile_on_cell(0, cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames[0].map, vec![1]);
                assert!(open.store.dirty());
                let f0 = open.frames[0].as_bytes(0).unwrap();
                assert!(
                    f0.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                    "frame 0 recomposed to the assigned tile's red pixels"
                );
            }

            // Escape's action body; with nothing selected, clicks stop
            // mutating.
            panel.deselect_tile(cx);
            assert_eq!(selected_single(panel), None);
            panel.set_tile_on_cell(0, cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1]);

            // Undo restores the prior composed bytes; redo re-applies.
            panel.undo_impl(cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames[0].map, vec![0]);
                assert!(!open.store.dirty(), "back to the loaded state");
                let f0 = open.frames[0].as_bytes(0).unwrap();
                assert!(
                    f0.chunks_exact(4).all(|p| p[3] == 0),
                    "undo recomposed frame 0 back to transparent"
                );
            }
            panel.redo_impl(cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1]);

            // Window->cell mapping over stamped bounds: a 1x1-tile sprite
            // maps every in-box point to cell 0 and rejects outside.
            *ready(panel).preview_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(10.), px(20.)),
                gpui::size(px(240.), px(240.)),
            ));
            assert_eq!(
                panel.preview_cell_at(gpui::point(px(10.), px(20.))),
                Some(0)
            );
            assert_eq!(
                panel.preview_cell_at(gpui::point(px(249.), px(259.))),
                Some(0)
            );
            assert_eq!(panel.preview_cell_at(gpui::point(px(9.), px(20.))), None);
            assert_eq!(panel.preview_cell_at(gpui::point(px(250.), px(20.))), None);
        });
    }

    /// Marquee multi-select: press-drag-release over the picker selects a
    /// BLOCK of tiles (down/move/up through the real handler bodies over
    /// stamped bounds), and a preview click stamps the whole block through
    /// ONE `FrameTilesSet` -- a single undo reverts the stamp.
    #[gpui::test]
    async fn test_marquee_selects_a_block_and_stamps_it_in_one_undo(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_multi_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Display shows the two NON-blank tiles: pool 1 (red), 2
            // (green); 1x1 sprite -> 2x1 so the block has room to land.
            assert_eq!(
                ready(panel).pool_strip.as_ref().expect("sheet").tiles,
                vec![1, 2]
            );
            panel.step_size(1, 0, cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![0, 0]);

            *ready(panel).picker_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(PICKER_CELL_PX * 2.), px(PICKER_CELL_PX)),
            ));

            // Drag across both sheet cells: (0,0) -> (1,0).
            panel.on_picker_down(gpui::point(px(2.), px(2.)), cx);
            panel.on_picker_move(gpui::point(px(PICKER_CELL_PX + 2.), px(2.)), cx);
            panel.on_picker_up(gpui::point(px(PICKER_CELL_PX + 2.), px(2.)), cx);
            {
                let open = ready(panel);
                let block = open.selection.as_ref().expect("marquee selected a block");
                assert_eq!((block.cols, block.rows), (2, 1));
                assert_eq!(
                    block.tiles,
                    vec![Some(1), Some(2)],
                    "sheet cells map through the display table to POOL tiles"
                );
            }

            // Stamp anchored at cell 0: both cells repoint in one op.
            panel.set_tile_on_cell(0, cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1, 2]);
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().frames[0].map,
                vec![0, 0],
                "one undo reverts the whole stamp"
            );

            // Press-release on one cell still toggles a 1x1 selection.
            panel.on_picker_down(gpui::point(px(2.), px(2.)), cx);
            panel.on_picker_up(gpui::point(px(2.), px(2.)), cx);
            assert_eq!(
                ready(panel).selection.as_ref().and_then(tiles::TileBlock::single),
                Some(1),
                "plain click selects the single POOL tile under the cell"
            );
            panel.on_picker_down(gpui::point(px(2.), px(2.)), cx);
            panel.on_picker_up(gpui::point(px(2.), px(2.)), cx);
            assert!(
                ready(panel).selection.is_none(),
                "re-click of the same single tile deselects"
            );
        });
    }

    /// The clip row's "Drop frame" slot: dropping a strip frame appends
    /// a COPY of it right after the clip's last frame and extends the
    /// clip's range by one -- the dragged frame itself stays put, and
    /// its editor-only name travels onto the copy.
    #[gpui::test]
    async fn test_dropping_a_frame_on_a_clip_appends_a_copy_and_extends_the_range(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.set_frame_name(0, "idle".to_string(), cx);
            // Fixture clip "walk" spans 1..=1. Drop frame 0 on its slot.
            panel.drop_frame_on_clip(0, 0, cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 3, "a copy was appended");
                assert_eq!(
                    state.frames[2].map, state.frames[0].map,
                    "the copy shows the dropped frame"
                );
                assert_eq!(
                    (state.clips[0].from, state.clips[0].to),
                    (1, 2),
                    "the clip range now covers the copy"
                );
                assert_eq!(open.frame_names, vec!["idle", "", "idle"]);
            }

            // A stale clip index is a no-op, not a panic.
            panel.drop_frame_on_clip(9, 0, cx);
            assert_eq!(ready(panel).store.state().frames.len(), 3);
        });
    }



    /// The per-frame duplicate/delete buttons on the clip sequence:
    /// duplicate inserts a copy right after its frame (extending the
    /// clip), delete removes the frame (the store remaps the clip; a
    /// clip whose sole frame died vanishes), names follow both ways,
    /// and stale clip indices are no-ops.
    #[gpui::test]
    async fn test_duplicate_and_delete_buttons_edit_the_clip_sequence(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.set_frame_name(1, "step".to_string(), cx);
            // Fixture clip "walk" spans 1..=1: duplicate its only frame.
            panel.duplicate_frame_in_clip(0, 0, cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 3, "a copy was inserted");
                assert_eq!(
                    state.frames[2].map, state.frames[1].map,
                    "the copy sits right after its source"
                );
                assert_eq!((state.clips[0].from, state.clips[0].to), (1, 2));
                assert_eq!(open.frame_names, vec!["", "step", "step"]);
            }

            // Delete the copy (sequence position 1 -> physical frame 2).
            panel.delete_frame_in_clip(0, 1, cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 2);
                assert_eq!((state.clips[0].from, state.clips[0].to), (1, 1));
                assert_eq!(open.frame_names, vec!["", "step"]);
            }

            // Deleting the clip's sole frame drops the clip itself.
            panel.delete_frame_in_clip(0, 0, cx);
            {
                let state = ready(panel).store.state();
                assert_eq!(state.frames.len(), 1);
                assert!(state.clips.is_empty(), "an empty clip vanishes");
            }

            // Stale indices: no-ops, not panics.
            panel.duplicate_frame_in_clip(9, 0, cx);
            panel.delete_frame_in_clip(9, 0, cx);
            panel.delete_frame_in_clip(0, 0, cx);
            assert_eq!(ready(panel).store.state().frames.len(), 1);
        });
    }

    /// The per-frame "..." settings popup: opening selects the frame
    /// (preview and editors agree), Escape's path and click-away close
    /// it, and a vanished owning clip closes it on refresh.
    #[gpui::test]
    async fn test_frame_settings_popup_opens_selects_and_dismisses(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.open_frame_settings(0, 1, gpui::point(px(50.), px(50.)), cx);
            let open = ready(panel);
            assert_eq!(open.selected_frame, 1, "opening selects the frame");
            assert_eq!(open.frame_settings.map(|(c, _)| c), Some(0));
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("ggo-sprite-frame-settings-popup").is_some(),
            "the popup paints anchored"
        );

        // Click-away dismisses (mouse down outside the occluded card).
        cx.simulate_mouse_down(
            gpui::point(px(400.), px(400.)),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        panel.read_with(cx, |panel, _| {
            assert!(ready(panel).frame_settings.is_none(), "click-away closes");
        });

        // Escape's shared clear path closes it too.
        panel.update(cx, |panel, cx| {
            panel.open_frame_settings(0, 0, gpui::point(px(50.), px(50.)), cx);
            panel.deselect_tile(cx);
            assert!(ready(panel).frame_settings.is_none(), "Escape closes");
        });

        // A vanished owning clip closes it on the next doc refresh.
        panel.update(cx, |panel, cx| {
            panel.open_frame_settings(0, 0, gpui::point(px(50.), px(50.)), cx);
            panel.delete_clip(0, cx);
            assert!(
                ready(panel).frame_settings.is_none(),
                "stale clip index cannot survive the refresh"
            );
        });
    }


    /// Sequence thumbnails render the frame's TRANSFORM, not the legacy
    /// composite: a rotated frame's thumb image is the doubled canvas
    /// (cached per frame until the next doc mutation), identity frames
    /// reuse the strip's exact Arc.
    #[gpui::test]
    async fn test_sequence_thumbs_render_the_transform(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.select_frame(1, cx);
            panel.commit_edit(EditTarget::Rot, "90".into(), cx);
            let open = ready(panel);
            let legacy = open.frames[1].clone();
            let transformed = open.frame_image(1).expect("composes");
            let (w, h) = image_px_size(&transformed);
            let (lw, lh) = image_px_size(&legacy);
            assert_eq!((w, h), (lw * 2, lh * 2), "rotated thumb is the doubled canvas");
            let again = open.frame_image(1).expect("composes");
            assert!(Arc::ptr_eq(&transformed, &again), "cached per frame");
            let identity = open.frame_image(0).expect("identity frame");
            assert!(
                Arc::ptr_eq(&identity, &open.frames[0]),
                "identity thumbs reuse the strip image"
            );
        });
    }

    /// The clip sequence row's positional drop: dropping a library frame
    /// on a sequence thumbnail inserts a COPY at that position INSIDE the
    /// clip's range (extending it by one); dropping past the end appends
    /// -- the same op the end slot fires. Names travel; stale indices
    /// no-op.
    #[gpui::test]
    async fn test_insert_frame_in_clip_at_a_position(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.set_frame_name(0, "idle".to_string(), cx);
            // Fixture clip "walk" spans 1..=1 (its sole frame maps [1]).
            // Insert a copy of frame 0 at sequence position 0: the copy
            // lands at strip index 1, BEFORE the clip's old frame.
            panel.insert_frame_in_clip(0, 0, 0, cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 3);
                assert_eq!(
                    state.frames[1].map, state.frames[0].map,
                    "the copy sits first in the clip"
                );
                assert_eq!((state.clips[0].from, state.clips[0].to), (1, 2));
                assert_eq!(open.frame_names, vec!["idle", "idle", ""]);
            }

            // Sequence position == len appends (the end slot's case).
            panel.insert_frame_in_clip(0, 2, 0, cx);
            {
                let state = ready(panel).store.state();
                assert_eq!(state.frames.len(), 4);
                assert_eq!((state.clips[0].from, state.clips[0].to), (1, 3));
                assert_eq!(state.frames[3].map, state.frames[0].map);
            }

            // Stale clip / frame / position indices all no-op.
            panel.insert_frame_in_clip(9, 0, 0, cx);
            panel.insert_frame_in_clip(0, 0, 9, cx);
            panel.insert_frame_in_clip(0, 9, 0, cx);
            assert_eq!(ready(panel).store.state().frames.len(), 4);
        });
    }

    /// Adding a frame to a clip physically copies it (range clips), but
    /// the FRAMES library must keep showing only the unique frames --
    /// the copy is a clip-sequence detail, not a new library entry.
    #[gpui::test]
    async fn test_library_stays_unique_after_a_clip_drop(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.drop_frame_on_clip(0, 0, cx);
            let state = ready(panel).store.state();
            assert_eq!(state.frames.len(), 3, "the copy exists physically");
            assert_eq!(
                edits::library_indices(&state.frames),
                vec![0, 1],
                "the library still lists only the two unique frames"
            );
        });
    }

    /// The Eraser tool: toggled on, a preview click blanks the cell
    /// (one undoable `FrameCellsErase`, allocating the hidden blank tile
    /// when the pool lacks one); Escape turns it off along with any
    /// selection; toggling it back off restores stamp behavior.
    #[gpui::test]
    async fn test_eraser_blanks_cells_and_escape_clears_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.select_frame(1, cx); // map [1]
            panel.toggle_eraser(cx);
            assert!(ready(panel).eraser);

            *ready(panel).preview_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(240.), px(240.)),
            ));
            panel.on_preview_click(gpui::point(px(10.), px(10.)), cx);
            {
                let state = ready(panel).store.state();
                let blanked = state.frames[1].map[0];
                let off = blanked as usize * TILE_BYTES;
                assert!(
                    state.pool[off..off + TILE_BYTES].iter().all(|&b| b == 0),
                    "the cell now points at a blank tile"
                );
            }
            panel.undo_impl(cx);
            assert_eq!(
                ready(panel).store.state().frames[1].map,
                vec![1],
                "one undo reverts the erase"
            );

            panel.deselect_tile(cx);
            assert!(!ready(panel).eraser, "Escape's path clears the eraser");

            // Eraser off + a selection: clicks stamp again.
            panel.select_tile(1, cx);
            panel.select_frame(0, cx); // map [0]
            panel.on_preview_click(gpui::point(px(10.), px(10.)), cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1]);
        });
    }

    /// The picker's own click path -- the SOURCE half of "click a tile,
    /// then click a frame cell to place it" -- over manually stamped
    /// sheet bounds (the headless panel never paints). The 2-tile fixture
    /// lays out as one row of two [`PICKER_CELL_PX`] cells, so the sheet
    /// splits at its midpoint; anything past the tiles selects nothing
    /// rather than the nearest one.
    #[gpui::test]
    async fn test_tile_picker_click_selects_the_tile_under_the_cursor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Bounds match the REAL sheet: one displayed tile, one cell.
            *ready(panel).picker_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(10.), px(20.)),
                gpui::size(px(PICKER_CELL_PX), px(PICKER_CELL_PX)),
            ));

            // The hero fixture's only pickable tile is pool tile 1 (tile
            // 0 is blank and hidden): sheet cell 0 selects it.
            panel.on_picker_down(gpui::point(px(12.), px(22.)), cx);
            panel.on_picker_up(gpui::point(px(12.), px(22.)), cx);
            assert_eq!(selected_single(panel), Some(1), "first cell = pool tile 1");

            // Placing it is the already-shipped tile-set path.
            panel.select_frame(0, cx);
            panel.set_tile_on_cell(0, cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1]);
            panel.undo_impl(cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![0]);
            assert!(!ready(panel).store.dirty());

            // Outside the sheet on either side: no selection change.
            panel.on_picker_down(gpui::point(px(9.), px(22.)), cx);
            panel.on_picker_up(gpui::point(px(9.), px(22.)), cx);
            assert_eq!(selected_single(panel), Some(1));
            panel.on_picker_down(gpui::point(px(10. + PICKER_CELL_PX + 1.), px(22.)), cx);
            panel.on_picker_up(gpui::point(px(10. + PICKER_CELL_PX + 1.), px(22.)), cx);
            assert_eq!(
                selected_single(panel),
                Some(1),
                "a press past the sheet selects nothing new"
            );
        });
    }

    /// The editor sidecar round trip: settings written by an earlier
    /// session apply at open (picker wrap, frame names), and a save
    /// writes the current settings back.
    #[gpui::test]
    async fn test_editor_sidecar_applies_at_open_and_writes_on_save(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        editor_meta::save(
            dir.path(),
            "sprites/hero.spr",
            &editor_meta::EditorMeta {
                picker_cols: Some(1),
                frame_names: vec!["walk-a".to_string(), "walk-b".to_string()],
            },
        )
        .unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                assert_eq!(open.picker_cols, 1, "sidecar wrap applied at open");
                let strip = open.pool_strip.as_ref().expect("sheet");
                assert_eq!(strip.cols, 1, "sheet composed at the sidecar wrap");
                assert_eq!(open.frame_names, vec!["walk-a", "walk-b"]);
            }

            panel.set_frame_name(0, "idle".to_string(), cx);
            panel.step_picker_cols(1, cx);
            panel.save_impl(cx);
        });
        let meta = editor_meta::load(dir.path(), "sprites/hero.spr");
        assert_eq!(meta.picker_cols, Some(2));
        assert_eq!(meta.frame_names, vec!["idle", "walk-b"]);
    }

    /// Frame names stay index-parallel to the strip across every frame
    /// op: add appends unnamed, duplicate copies the name, delete
    /// removes, and a drag move reorders.
    #[gpui::test]
    async fn test_frame_names_follow_frame_ops(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.set_frame_name(0, "a".to_string(), cx);
            panel.set_frame_name(1, "b".to_string(), cx);

            panel.add_blank_frame(cx);
            assert_eq!(ready(panel).frame_names, vec!["a", "b", ""]);

            panel.select_frame(0, cx);
            panel.duplicate_selected_frame(cx);
            assert_eq!(ready(panel).frame_names, vec!["a", "a", "b", ""]);

            panel.delete_selected_frame(cx);
            assert_eq!(ready(panel).frame_names, vec!["a", "b", ""]);

            panel.move_frame_to(0, 2, cx);
            assert_eq!(ready(panel).frame_names, vec!["b", "", "a"]);
            assert_eq!(
                ready(panel).store.state().frames[2].map,
                vec![0],
                "the moved frame's map traveled with it"
            );
        });
    }

    /// The name-frame form's UI path end to end: `begin_name_frame` opens
    /// the form with an EMPTY editor for an unnamed frame (seeding the
    /// "Frame N" fallback would commit it as a literal name), and
    /// `confirm_name_frame` lands the typed name in `frame_names`, closes
    /// the form, and writes the sidecar -- the only persistence frame
    /// names have (the `.spr` has no name field).
    #[gpui::test]
    async fn test_name_frame_form_commits_through_the_ui_path(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture(dir.path());
        let root = dir.path().to_path_buf();
        let (panel, cx) = cx.add_window_view(|_, cx| {
            let mut panel = SpritePanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("sprites/hero.spr", cx);
        });
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| {
            panel.begin_name_frame(0, window, cx);
            let Some(PanelForm::NameFrame { index, editor }) = &panel.form else {
                panic!("begin_name_frame must open the name form");
            };
            assert_eq!(*index, 0);
            assert_eq!(editor.read(cx).text(cx), "", "an unnamed frame seeds empty");
            editor
                .clone()
                .update(cx, |editor, cx| editor.set_text("idle", window, cx));
            panel.confirm_name_frame(cx);
        });

        panel.update(cx, |panel, _cx| {
            assert_eq!(ready(panel).frame_names[0], "idle");
            assert!(panel.form.is_none(), "the commit closes the form");
        });
        assert_eq!(
            editor_meta::load(dir.path(), "sprites/hero.spr").frame_names,
            vec!["idle"],
            "the name landed in the sidecar"
        );
    }

    /// The W/H steppers' body: each step applies `DocOp::Resize` with a
    /// top-left anchor through `apply_doc` (undo/redo and recompose come
    /// with it), clamped to worldlib's 1..=16 tile range -- at-bound
    /// steps are dropped without an op.
    #[gpui::test]
    async fn test_size_steppers_resize_the_sprite_with_undo_and_clamps(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Grow the 1x1 fixture to 2x1: every frame map gains a cell.
            panel.step_size(1, 0, cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!((state.w_tiles, state.h_tiles), (2, 1));
                assert_eq!(state.frames[0].map.len(), 2);
                assert!(open.store.dirty());
            }

            // Undo restores the old grid in one step.
            panel.undo_impl(cx);
            {
                let state = ready(panel).store.state();
                assert_eq!((state.w_tiles, state.h_tiles), (1, 1));
                assert_eq!(state.frames[0].map.len(), 1);
                assert!(!ready(panel).store.dirty());
            }

            // Min clamp: shrinking a 1-tile side is a no-op, not an error.
            panel.step_size(-1, 0, cx);
            panel.step_size(0, -1, cx);
            assert!(!ready(panel).store.dirty(), "at-min steps drop the op");

            // Max clamp: walk height to 16, then one more step no-ops.
            for _ in 0..15 {
                panel.step_size(0, 1, cx);
            }
            assert_eq!(ready(panel).store.state().h_tiles, 16);
            panel.step_size(0, 1, cx);
            assert_eq!(
                ready(panel).store.state().h_tiles,
                16,
                "at-max step drops the op"
            );
        });
    }

    /// The picker-cols stepper: `picker_cols` starts at the
    /// [`loader::PICKER_COLS`] default, each step recomposes the sheet at
    /// the new wrap (clamped at 1), and the setting survives a doc edit's
    /// wholesale pool-strip recompose.
    #[gpui::test]
    async fn test_picker_cols_stepper_rewraps_the_sheet(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_multi_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Two pickable tiles at the default 4-col setting: one row.
            {
                let strip = ready(panel).pool_strip.as_ref().expect("sheet");
                assert_eq!((strip.cols, strip.rows), (2, 1));
            }

            // Step down to 1 column: the 2 tiles wrap onto 2 rows.
            for _ in 0..3 {
                panel.step_picker_cols(-1, cx);
            }
            {
                let open = ready(panel);
                assert_eq!(open.picker_cols, 1);
                let strip = open.pool_strip.as_ref().expect("sheet");
                assert_eq!((strip.cols, strip.rows), (1, 2));
            }

            // Min clamp.
            panel.step_picker_cols(-1, cx);
            assert_eq!(ready(panel).picker_cols, 1, "cols clamp at 1");

            // A doc edit recomposes the strip wholesale -- at the chosen
            // wrap, not the default.
            panel.select_tile(1, cx);
            panel.set_tile_on_cell(0, cx);
            {
                let strip = ready(panel).pool_strip.as_ref().expect("sheet");
                assert_eq!(
                    (strip.cols, strip.rows),
                    (1, 2),
                    "picker_cols survives refresh_after_doc_change"
                );
            }
        });
    }

    /// The full RENDERED click path -- the one every other tile test
    /// bypasses by stamping bounds by hand: draw the panel in a real test
    /// window (the overlay canvases record picker/preview bounds at
    /// prepaint), then drive platform mouse events at those bounds. A
    /// picker click must select the tile under the cursor and a preview
    /// click must repoint the selected frame's cell through the same
    /// listeners a user's clicks go through.
    #[gpui::test]
    async fn test_rendered_clicks_select_a_tile_and_repoint_the_cell(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture(dir.path());
        let root = dir.path().to_path_buf();
        let (panel, cx) = cx.add_window_view(|_, cx| {
            let mut panel = SpritePanel::new(None, cx);
            panel.root_override = Some(root);
            panel
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("sprites/hero.spr", cx);
        });
        cx.run_until_parked();

        let picker = panel
            .read_with(cx, |panel, _| *ready(panel).picker_bounds.borrow())
            .expect("picker bounds recorded at prepaint");
        // The sheet's sole cell: pool tile 1 (blank tile 0 is hidden).
        let target = picker.origin + gpui::point(px(PICKER_CELL_PX / 2.), px(PICKER_CELL_PX / 2.));
        cx.simulate_mouse_move(target, None, gpui::Modifiers::default());
        // Full press-release: selection resolves on mouse-up now that a
        // held press is a marquee in flight.
        cx.simulate_click(target, gpui::Modifiers::default());
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                selected_single(panel),
                Some(1),
                "rendered picker click selects the tile under the cursor"
            );
        });

        let preview = panel
            .read_with(cx, |panel, _| *ready(panel).preview_bounds.borrow())
            .expect("preview bounds recorded at prepaint");
        cx.simulate_mouse_down(
            preview.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                ready(panel).store.state().frames[0].map,
                vec![1],
                "rendered preview click repoints frame 0's sole cell"
            );
        });
    }

    /// The clip-name editor for clip 0, out of the rendered panel's
    /// editor set (built by `ensure_editors` on first draw).
    fn clip_name_editor(
        panel: &gpui::Entity<SpritePanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> gpui::Entity<Editor> {
        panel.read_with(cx, |panel, _| {
            ready(panel)
                .editors
                .iter()
                .find(|e| e.target == EditTarget::ClipName(0))
                .expect("the rendered panel has a clip-name editor")
                .editor
                .clone()
        })
    }

    /// The space keybinding's `not_editing` guard, end to end through
    /// real keystroke dispatch: with the panel focused, space toggles the
    /// transport on and off again; with a clip-name editor focused, the
    /// `editing` stamp in [`SpritePanel::dispatch_context`] keeps the
    /// panel-depth binding from matching, so playback must NOT start and
    /// the editor must receive the space as plain text.
    #[gpui::test]
    async fn test_space_keystroke_toggles_playback_only_when_not_editing(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("space");
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert!(open.playing.is_some(), "space starts the transport");
            assert!(open._tick_task.is_some(), "the tick loop is armed");
        });
        cx.simulate_keystrokes("space");
        panel.read_with(cx, |panel, _| {
            assert!(
                ready(panel).playing.is_none(),
                "space again stops the transport"
            );
        });

        let editor = clip_name_editor(&panel, cx);
        editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        // The focus subscription notifies; the re-render re-reads
        // `dispatch_context` and stamps `editing`.
        cx.run_until_parked();

        cx.simulate_keystrokes("space");
        panel.read_with(cx, |panel, _| {
            assert!(
                ready(panel).playing.is_none(),
                "space while a field editor is focused must not start playback"
            );
        });
        let text = editor.read_with(cx, |editor, cx| editor.text(cx));
        assert!(
            text.contains(' ') && text.trim() == "walk",
            "the space fell through to the editor as text, got {text:?}"
        );
    }

    /// Ctrl-z / ctrl-shift-z through real keystroke dispatch step a real
    /// edit back and forward (the `KEY_CONTEXT`-scoped Undo/Redo
    /// bindings reach `undo_impl`/`redo_impl`).
    #[gpui::test]
    async fn test_undo_redo_keystrokes_step_a_real_edit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(ready(panel).store.state().frames[0].duration_ms, 40);
        });

        cx.simulate_keystrokes("ctrl-z");
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(
                open.store.state().frames[0].duration_ms,
                100,
                "ctrl-z undoes the duration edit"
            );
            assert!(!open.store.dirty(), "undo back to the saved state");
        });

        cx.simulate_keystrokes("ctrl-shift-z");
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(
                open.store.state().frames[0].duration_ms,
                40,
                "ctrl-shift-z redoes it"
            );
            assert!(open.store.dirty());
        });
    }

    /// Ctrl-s through real keystroke dispatch runs the save: the store
    /// goes clean and the `.spr` on disk carries the edit.
    #[gpui::test]
    async fn test_ctrl_s_keystroke_writes_the_sprite_to_disk(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
        });
        assert_eq!(on_disk_duration(dir.path()), 100, "not saved yet");

        cx.simulate_keystrokes("ctrl-s");
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert!(!open.store.dirty(), "ctrl-s saves the document");
            assert!(open.save_error.is_none());
        });
        assert_eq!(
            on_disk_duration(dir.path()),
            40,
            "the edit reached the file on disk"
        );
    }

    /// Escape through real keystroke dispatch clears the active tile
    /// selection AND the eraser flag (the `DeselectTile` binding's
    /// click-doesn't-mutate affordance).
    #[gpui::test]
    async fn test_escape_keystroke_clears_selection_and_eraser(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        panel.update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
            panel.select_tile(1, cx);
            panel.toggle_eraser(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(selected_single(panel), Some(1));
            assert!(ready(panel).eraser);
        });

        cx.simulate_keystrokes("escape");
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert!(open.selection.is_none(), "escape drops the tile selection");
            assert!(!open.eraser, "escape clears the eraser flag");
        });
    }

    /// The transport at panel level: `toggle_play` arms the tick loop,
    /// a real tick (driven by advancing the test executor's clock across
    /// the parked [`TICK`] timer) recomputes the shown frame, and
    /// `toggle_play` again drops the loop and falls back to the
    /// selection. The transport is wall-clock anchored and a parked
    /// test's wall clock barely moves, so the test seeds
    /// `start_offset_ms` into frame 1's window (100..300ms) and lets the
    /// REAL tick path do the recompute.
    #[gpui::test]
    async fn test_toggle_play_runs_the_transport_and_advances_on_ticks(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| panel.toggle_play(cx));
        // Let the tick loop start and park on its first timer.
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            assert!(open.playing.is_some(), "toggle_play starts the transport");
            assert!(open._tick_task.is_some(), "the tick task is armed");
            assert_eq!(open.shown_frame(), 0, "playback starts on the selection");
            open.playing
                .as_mut()
                .expect("checked playing above")
                .start_offset_ms += 150;
        });

        cx.executor().advance_clock(TICK);
        cx.executor().run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                ready(panel).shown_frame(),
                1,
                "the tick recomputed the shown frame from elapsed time"
            );
        });

        panel.update(cx, |panel, cx| panel.toggle_play(cx));
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert!(open.playing.is_none(), "toggle_play again stops it");
            assert!(open._tick_task.is_none(), "the tick loop is dropped");
            assert_eq!(open.shown_frame(), 0, "the preview falls back to the selection");
        });
    }

    /// Rendered strip drag-reorder, through gpui's real drag machinery:
    /// mouse-down on frame 0's cell, drag onto frame 1's cell (the
    /// `debug_selector` bounds), release -- `DocOp::FrameMove` lands
    /// (frames swap, the editor-only names travel, the moved frame stays
    /// selected) and a single undo restores the strip, proving it was
    /// ONE op.
    #[gpui::test]
    /// Every clip card paints its own sequence WITHOUT any activation
    /// step (the transport's clip dropdown is playback-range only): a
    /// real drag from the FRAMES library onto a sequence thumbnail
    /// inserts a copy at that position, and clicking a sequence
    /// thumbnail selects (previews) that frame.
    #[gpui::test]
    async fn test_rendered_sequence_drop_inserts_and_click_selects(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                ready(panel).active_clip,
                None,
                "precondition: no clip is active -- the sequence must paint anyway"
            );
        });

        let library0 = cx
            .debug_bounds("ggo-sprite-frame-0")
            .expect("library frame 0 painted");
        let seq0 = cx
            .debug_bounds("ggo-sprite-seq-0-0")
            .expect("clip 0's sequence is painted with no activation step");

        cx.simulate_mouse_move(library0.center(), None, gpui::Modifiers::default());
        cx.simulate_mouse_down(library0.center(), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(seq0.center(), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(seq0.center(), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(seq0.center(), MouseButton::Left, gpui::Modifiers::default());

        panel.read_with(cx, |panel, _| {
            let state = ready(panel).store.state();
            assert_eq!(state.frames.len(), 3, "the drop inserted a copy");
            assert_eq!(
                state.frames[1].map, state.frames[0].map,
                "the copy landed at sequence position 0 (strip index 1)"
            );
            assert_eq!((state.clips[0].from, state.clips[0].to), (1, 2));
        });

        // Clicking the second sequence thumbnail selects its frame.
        cx.run_until_parked();
        let seq1 = cx
            .debug_bounds("ggo-sprite-seq-0-1")
            .expect("the extended sequence paints two thumbs");
        cx.simulate_click(seq1.center(), gpui::Modifiers::default());
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                ready(panel).selected_frame,
                2,
                "sequence position 1 is strip frame 2 -- selected and previewed"
            );
        });
    }

    #[gpui::test]
    async fn test_rendered_strip_drag_reorders_frames(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;
        panel.update(cx, |panel, cx| {
            panel.set_frame_name(0, "walk-a".into(), cx);
            panel.set_frame_name(1, "walk-b".into(), cx);
        });
        cx.run_until_parked();

        let cell0 = cx
            .debug_bounds("ggo-sprite-frame-0")
            .expect("frame 0's strip cell is painted");
        let cell1 = cx
            .debug_bounds("ggo-sprite-frame-1")
            .expect("frame 1's strip cell is painted");

        cx.simulate_mouse_move(cell0.center(), None, gpui::Modifiers::default());
        cx.simulate_mouse_down(cell0.center(), MouseButton::Left, gpui::Modifiers::default());
        // The first held move (past the 2px threshold) starts the drag
        // and refreshes; the second hovers frame 1's freshly painted
        // hitbox so the release lands on its drop listener.
        cx.simulate_mouse_move(cell1.center(), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(cell1.center(), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(cell1.center(), MouseButton::Left, gpui::Modifiers::default());

        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            let state = open.store.state();
            assert_eq!(
                state.frames[0].map,
                vec![1],
                "the drop moved frame 0 past frame 1"
            );
            assert_eq!(state.frames[0].duration_ms, 200);
            assert_eq!(state.frames[1].map, vec![0]);
            assert_eq!(state.frames[1].duration_ms, 100);
            assert_eq!(
                open.frame_names,
                ["walk-b", "walk-a"],
                "the editor-only names traveled with the frames"
            );
            assert_eq!(open.selected_frame, 1, "the moved frame stays selected");
            assert!(open.store.dirty());
        });

        panel.update(cx, |panel, cx| panel.undo_impl(cx));
        panel.read_with(cx, |panel, _| {
            let state = ready(panel).store.state();
            assert_eq!(
                (state.frames[0].duration_ms, state.frames[1].duration_ms),
                (100, 200),
                "one undo restores the order => the drop was a single FrameMove"
            );
        });
    }

    /// Enter ([`CommitField`], bound at `KEY_CONTEXT > Editor`) on a
    /// focused clip-name editor commits the typed text into the doc.
    #[gpui::test]
    async fn test_enter_commits_a_focused_clip_name_editor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, cx) = ready_panel_in_window(cx, dir.path()).await;

        let editor = clip_name_editor(&panel, cx);
        editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
            editor.set_text("", window, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("r u n enter");
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(
                open.store.state().clips[0].name,
                "run",
                "enter commits the typed clip name"
            );
            assert!(open.store.dirty());
        });
    }

    /// M6 fix-forward: a preview click while the transport is running
    /// pauses it, adopts the transport's SHOWN frame as the selection,
    /// and only then maps the click -- so the edit lands on the frame
    /// the user saw, not the stale pre-playback selection (which here
    /// would have been silently dropped as a same-tile click).
    #[gpui::test]
    async fn test_preview_click_during_playback_pauses_and_edits_shown_frame(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Tile 1 active (the only pickable tile -- 0 is blank and
            // hidden); frame 1 selected (map [1] -- a click on it would
            // be a same-tile no-op); the transport is showing frame 0
            // (map [0]).
            panel.select_tile(1, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selected_frame = 1;
                open.playing = Some(Playing {
                    started: Instant::now(),
                    start_offset_ms: 0,
                    frame: 0,
                });
                // Arm a (dummy) tick task so the drop assertion below
                // is not vacuously true.
                open._tick_task = Some(cx.spawn(async move |_, _| {}));
            }
            *ready(panel).preview_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(240.), px(240.)),
            ));

            panel.on_preview_click(gpui::point(px(10.), px(10.)), cx);
            {
                let open = ready(panel);
                assert!(open.playing.is_none(), "the click pauses the transport");
                assert!(open._tick_task.is_none(), "the tick loop is dropped");
                assert_eq!(
                    open.selected_frame, 0,
                    "selection adopts the transport's shown frame"
                );
                assert_eq!(
                    open.store.state().frames[0].map,
                    vec![1],
                    "the edit hit the DISPLAYED frame"
                );
                assert_eq!(
                    open.store.state().frames[1].map,
                    vec![1],
                    "the stale pre-playback selection is untouched"
                );
                assert_eq!(open.shown_frame(), 0, "preview stays on the edited frame");
            }

            // Not playing: the same click path edits the selected frame
            // directly (undo the playback edit first so the cell click
            // isn't a same-tile drop).
            panel.undo_impl(cx);
            panel.select_frame(0, cx);
            panel.on_preview_click(gpui::point(px(10.), px(10.)), cx);
            assert_eq!(
                ready(panel).store.state().frames[0].map,
                vec![1],
                "a click with no transport running still edits the selected frame"
            );
        });
    }

    /// Save: `save_sprite` -> `set_saved_state` clears dirty without an
    /// undo entry; the written trio `open_sprite`-round-trips equal to the
    /// store's state; undo after save still steps back through the user's
    /// edits (and re-dirties, since the saved generation moved on).
    #[gpui::test]
    async fn test_save_round_trips_through_open_sprite_and_clears_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
            panel.add_clip(cx);
            assert!(ready(panel).store.dirty());

            panel.save_impl(cx);
            {
                let open = ready(panel);
                assert!(!open.store.dirty(), "save clears dirty");
                assert!(open.save_error.is_none());
            }
        });

        let reopened = open_sprite(dir.path(), "sprites/hero.spr").unwrap();
        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(reopened.state.frames, state.frames);
                assert_eq!(reopened.state.clips, state.clips);
                assert_eq!(reopened.state.palette, state.palette);
                assert_eq!(reopened.state.pool, state.pool);
                assert_eq!(reopened.state.tile_count, state.tile_count);
                assert_eq!(reopened.state.w_tiles, state.w_tiles);
                assert_eq!(reopened.state.h_tiles, state.h_tiles);
                assert_eq!(reopened.til_path, open.til_path);
                assert_eq!(reopened.pal_path, open.pal_path);
                assert_eq!(reopened.state.frames[0].duration_ms, 40);
                assert_eq!(reopened.state.clips.len(), 2);
            }

            panel.undo_impl(cx); // fold-back wasn't an undo entry; this undoes add_clip
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips.len(), 1);
                assert!(open.store.dirty(), "undo past the save point re-dirties");
            }
        });
    }

    /// A failed write must leave the document recoverable: `save_error`
    /// surfaced, `dirty` STILL set -- `set_saved_state` only on success --
    /// so the close guards keep protecting the edits, and the file on
    /// disk untouched.
    #[gpui::test]
    async fn test_save_failure_keeps_the_document_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        // A root pointing at a regular FILE makes `save_sprite`'s
        // create-parent-dirs step fail deterministically, where a merely
        // missing directory would just be created.
        let bad_root = dir.path().join("sprites/hero.spr");
        panel.update(cx, |panel, cx| {
            assert!(panel.apply_doc(DocOp::FrameDuration { at: 0, ms: 500 }, cx));
            if let ViewerState::Ready(open) = &mut panel.state {
                open.root = bad_root;
            }
            panel.save_impl(cx);
            let open = ready(panel);
            assert!(open.save_error.is_some(), "the failure must be surfaced");
            assert!(open.store.dirty(), "a failed save must not clear dirty");
        });
        assert_eq!(
            on_disk_duration(dir.path()),
            100,
            "the failed save must not have written the sprite"
        );
    }

    // ------------------------------------------------- asset-root resolution

    /// An emerald project skeleton: `emerald.toml` at the root, the sprite
    /// trio directly inside `assets/`. Mirrors the wilds layout that
    /// surfaced this bug.
    fn emerald_project(stem: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("emerald.toml"),
            "[project]\nname='t'\ntitle='t'\n",
        )
        .unwrap();
        let assets = dir.path().join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        write_sprite_fixture_at(&assets, stem);
        dir
    }

    /// The split itself: inside an emerald project the asset root is
    /// `<project>/assets` and the doc rel is relative to THAT, so a sidecar
    /// written from it can never carry an `assets/` segment. Outside one,
    /// nothing changes.
    #[gpui::test]
    async fn test_split_sprite_path_derives_the_asset_root(_cx: &mut TestAppContext) {
        let dir = emerald_project("hh");
        let root = dir.path();
        let assets = root.join("assets");

        // The wilds case: sprite directly at the asset root.
        assert_eq!(
            split_sprite_path(root, "assets/hh.spr"),
            (assets.clone(), "hh.spr".to_string())
        );

        // Nested under the asset root: the nesting stays in the rel.
        std::fs::create_dir_all(assets.join("sprites")).unwrap();
        write_sprite_fixture_named(&assets, "hero");
        assert_eq!(
            split_sprite_path(root, "assets/sprites/hero.spr"),
            (assets, "sprites/hero.spr".to_string())
        );

        // Inside the project but OUTSIDE assets/: no asset root applies, so
        // the worktree root is kept rather than guessed at.
        assert_eq!(
            split_sprite_path(root, "scratch/x.spr"),
            (root.to_path_buf(), "scratch/x.spr".to_string())
        );

        // Not an emerald project at all: today's behavior, untouched.
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(
            split_sprite_path(plain.path(), "sprites/hero.spr"),
            (plain.path().to_path_buf(), "sprites/hero.spr".to_string())
        );
    }

    /// The Part C contract: opening `assets/hh.spr` resolves the asset root
    /// to `<project>/assets`, and a SAVE therefore writes `hh.til`/`hh.pal`
    /// -- asset-root-relative, exactly what emerald's packer expects -- not
    /// `assets/hh.til`. The regression this whole change exists to prevent.
    #[gpui::test]
    async fn test_save_writes_asset_root_relative_sidecars(cx: &mut TestAppContext) {
        let dir = emerald_project("hh");
        let root = dir.path().to_path_buf();
        let assets = dir.path().join("assets");

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = SpritePanel::new(None, cx);
                // The WORKTREE root -- the panel must narrow it itself.
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/hh.spr", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                assert_eq!(open.root, assets, "asset root is <project>/assets");
                assert_eq!(open.rel_path, "hh.spr", "doc rel is asset-root-relative");
                assert_eq!(open.source_rel, "assets/hh.spr", "clicked path is kept");
                assert_eq!(open.til_path, "hh.til");
                assert_eq!(open.pal_path, "hh.pal");
            }
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
            panel.save_impl(cx);
            assert!(ready(panel).save_error.is_none());
        });

        // The bytes on disk are what actually matter: the sidecar strings
        // stored INSIDE the `.spr` must be asset-root-relative.
        let bytes = std::fs::read(assets.join("hh.spr")).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("hh.til") && text.contains("hh.pal"),
            "sidecar rels missing from the saved .spr: {text:?}"
        );
        assert!(
            !text.contains("assets/hh.til") && !text.contains("assets/hh.pal"),
            "a saved sidecar must never carry the assets/ prefix: {text:?}"
        );
        // The trio landed at the asset root, not one level up.
        assert!(assets.join("hh.til").is_file() && assets.join("hh.pal").is_file());
        assert!(
            !dir.path().join("hh.til").exists(),
            "nothing may be written outside the asset root"
        );
        // Re-reading against the asset root (what emerald's packer does)
        // resolves the sidecars and sees the edit.
        let reopened = open_sprite(&assets, "hh.spr").unwrap();
        assert_eq!(reopened.til_path, "hh.til");
        assert_eq!(reopened.state.frames[0].duration_ms, 40);
    }

    /// A LEGACY `.spr` (sidecars stored project-root-relative, the wilds
    /// corruption) is surfaced, not silently perpetuated.
    ///
    /// The panel deliberately does NOT rescue it: teaching the loader to
    /// strip a leading `assets/` would mask this exact signature and
    /// silently launder future instances. Repairing the DATA is
    /// `ggo-sprfix`'s job. What this test pins is that the panel can't make
    /// things worse -- the corrupt file never reaches Ready, so there is no
    /// document to save the bad value back from, and the bytes on disk are
    /// left untouched.
    #[gpui::test]
    async fn test_a_legacy_prefixed_sprite_surfaces_instead_of_being_perpetuated(
        cx: &mut TestAppContext,
    ) {
        let dir = emerald_project("hh");
        let assets = dir.path().join("assets");

        // Reproduce the corruption the way it was actually produced: hand
        // `save_sprite` the PROJECT root, so every rel picks up an extra
        // `assets/` segment. The trio lands in the right places on disk; it
        // is the strings STORED in the `.spr` that are wrong.
        save_fixture(
            dir.path(),
            "assets/hh.spr",
            "assets/hh.til",
            "assets/hh.pal",
        );
        let corrupt = std::fs::read(assets.join("hh.spr")).unwrap();
        assert!(
            String::from_utf8_lossy(&corrupt).contains("assets/hh.til"),
            "fixture should carry the legacy prefixed sidecar"
        );

        let root = dir.path().to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = SpritePanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("assets/hh.spr", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            let ViewerState::Error(message) = &panel.state else {
                panic!("a legacy prefixed .spr must not silently reach Ready");
            };
            assert!(
                message.contains("assets/hh.til"),
                "the error must name the unresolvable sidecar, got: {message}"
            );
            // Saving in this state is a no-op, not a write of the bad value.
            panel.save_impl(cx);
        });
        assert_eq!(
            std::fs::read(assets.join("hh.spr")).unwrap(),
            corrupt,
            "the corrupt file must be left exactly as found"
        );

        // After the documented repair (what `ggo-sprfix --write` does to the
        // stored rels), the same click loads and resolves cleanly.
        save_fixture(&assets, "hh.spr", "hh.til", "hh.pal");

        panel.update(cx, |panel, cx| panel.load_rel_path("assets/hh.spr", cx));
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            assert_eq!(open.til_path, "hh.til");
            assert_eq!(open.rel_path, "hh.spr");
        });
    }

    // ----------------------------------------- context-menu file ops (G2)

    /// The `-copy` naming rule, without a filesystem: first free name
    /// wins, and the counter starts at 2 (there is no `-copy-1`).
    #[gpui::test]
    fn test_free_copy_base_finds_the_first_unused_name(_cx: &mut gpui::App) {
        assert_eq!(
            free_copy_base("sprites/hero", |_| false),
            "sprites/hero-copy"
        );
        assert_eq!(
            free_copy_base("hero", |c| c == "hero-copy"),
            "hero-copy-2",
            "a taken -copy must step to -copy-2"
        );
        assert_eq!(
            free_copy_base("hero", |c| c == "hero-copy" || c == "hero-copy-2"),
            "hero-copy-3"
        );
    }

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
        Entity<SpritePanel>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        let project = routed_project(cx, root, true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        // Dock era over: tests that drive panel methods directly get a
        // standalone entity (menu handlers create their own item panels,
        // which resolve the same root through the worktree).
        let weak = workspace.downgrade();
        let panel = cx.update(|_, cx| cx.new(|cx| SpritePanel::new(Some(weak), cx)));
        let root = root.to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));
        (workspace, panel, worktree_id, cx)
    }

    /// Load `rel` into the workspace's own panel and leave it Ready.
    fn open_in_menu_panel(
        panel: &Entity<SpritePanel>,
        cx: &mut gpui::VisualTestContext,
        rel: &str,
    ) {
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path(rel, cx);
        });
        cx.run_until_parked();
    }

    /// The panel inside the newest `SpriteEditorItem` -- where the menu
    /// handlers land their forms and documents now that the dock is gone.
    fn newest_item_panel(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<SpritePanel> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<sprite_item::SpriteEditorItem>(cx)
                .last()
                .expect("a menu handler should have opened an item")
                .read(cx)
                .panel()
                .clone()
        })
    }

    /// Open (or focus) the item for `rel` through the production entry
    /// plumbing and hand back its inner panel, loaded and parked.
    fn item_panel_for(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
        rel: &str,
    ) -> Entity<SpritePanel> {
        let handler =
            sprite_item_entry_handler(workspace.downgrade(), Some(rel.to_string()), |_, _, _| {});
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        let rel = rel.to_string();
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<sprite_item::SpriteEditorItem>(cx)
                .find(|item| item.read(cx).rel() == rel)
                .expect("the entry handler opened an item for the rel")
                .read(cx)
                .panel()
                .clone()
        })
    }

    /// The file entries are offered for a `.spr` and for NOTHING else --
    /// the same extension rule the open interceptor uses
    /// ([`is_sprite_path`]), so a `.til` sidecar and a plain text file
    /// leave upstream's menu as it was. A DIRECTORY gets the two "New …"
    /// entries instead, and only inside an emerald project's assets tree
    /// (this fixture has no `emerald.toml`, so every directory here is
    /// outside one).
    #[gpui::test]
    async fn test_context_menu_offers_sprite_ops_only_for_spr_files(cx: &mut TestAppContext) {
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
            contributed("sprites/hero.spr", false, cx),
            3,
            "a .spr must get Duplicate + Rename + Delete"
        );
        assert_eq!(
            contributed("sprites/hero.til", false, cx),
            0,
            "a tileset sidecar is not a sprite"
        );
        assert_eq!(contributed("notes.txt", false, cx), 0);
        assert_eq!(
            contributed("sprites", true, cx),
            0,
            "a directory outside an emerald assets tree gets nothing"
        );
    }

    /// Duplicate writes a real sprite: the copy round-trips through
    /// worldlib's own `open_sprite` with the SAME document the original
    /// has, and it points at its OWN `.til`/`.pal` -- which is the whole
    /// reason this isn't `fs::copy`. The original is left pointing at its
    /// own sidecars and un-shared, so nothing about editing it changed.
    /// A second duplicate steps to `-copy-2` instead of clobbering the
    /// first.
    #[gpui::test]
    async fn test_duplicate_sprite_writes_a_copy_that_round_trips(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        let root = dir.path().to_path_buf();
        let original = open_sprite(&root, "sprites/hero.spr").expect("the fixture opens");

        let handler =
            duplicate_sprite_handler(workspace.downgrade(), "sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
        assert!(
            !cx.has_pending_prompt(),
            "duplicating destroys nothing, so it must not prompt"
        );

        let copy = open_sprite(&root, "sprites/hero-copy.spr").expect("the copy is a real .spr");
        assert_eq!(
            copy.state, original.state,
            "the copy must carry the same document"
        );
        assert_eq!(copy.til_path, "sprites/hero-copy.til");
        assert_eq!(copy.pal_path, "sprites/hero-copy.pal");

        let reread = open_sprite(&root, "sprites/hero.spr").expect("the original still opens");
        assert_eq!(
            (reread.til_path.as_str(), reread.pal_path.as_str()),
            ("sprites/hero.til", "sprites/hero.pal"),
            "the original must keep its own sidecars"
        );
        assert!(
            !reread.state.pool_shared,
            "duplicating must not flip the original into shared-pool mode"
        );

        cx.update(|window, cx| handler(window, cx));
        assert!(
            root.join("sprites/hero-copy-2.spr").is_file(),
            "a second duplicate must take the next free name"
        );
        assert_eq!(
            copy.state,
            open_sprite(&root, "sprites/hero-copy.spr").unwrap().state,
            "and must not have touched the first copy"
        );
    }

    /// Cancel is the fail-safe answer: the file survives and the panel is
    /// untouched. Driven through the entry's OWN handler
    /// ([`delete_sprite_handler`]), which is what the contributed
    /// `ContextMenuEntry` runs -- `ContextMenuEntry::handler` is private,
    /// so this is the only way to fire the real thing.
    #[gpui::test]
    async fn test_delete_sprite_cancel_keeps_the_file_and_the_panel(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "sprites/hero.spr");

        let handler = delete_sprite_handler(workspace.downgrade(), "sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
        assert_eq!(
            cx.pending_prompt(),
            Some((
                "Delete the sprite sprites/hero.spr?".to_string(),
                "This cannot be undone.".to_string(),
            ))
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert!(
            dir.path().join("sprites/hero.spr").is_file(),
            "Cancel must leave the file on disk"
        );
        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("Cancel must leave the panel Ready");
            };
            assert_eq!(open.source_rel, "sprites/hero.spr");
        });
    }

    /// Confirm unlinks the `.spr`, and because that file was the OPEN
    /// document the panel drops back to Empty -- it must not keep showing
    /// a sprite you can still edit, undo and "save" into nothing. The
    /// sidecars deliberately survive: they are shareable, so one sprite's
    /// deletion must not break another's tiles.
    #[gpui::test]
    async fn test_delete_sprite_confirm_removes_the_file_and_clears_the_panel(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        let panel = item_panel_for(&workspace, cx, "sprites/hero.spr");

        let handler = delete_sprite_handler(workspace.downgrade(), "sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(
            !dir.path().join("sprites/hero.spr").exists(),
            "Delete must unlink the .spr"
        );
        assert!(
            dir.path().join("sprites/hero.til").is_file(),
            "a shareable sidecar must survive its sprite"
        );
        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(panel.state, ViewerState::Empty),
                "the open document's file is gone, so the panel must clear"
            );
        });
    }

    /// The prompt must SAY when the file being deleted is the open
    /// document and it has unsaved edits -- and only then. It deliberately
    /// does NOT offer to save: deleting the file makes the edit moot, and a
    /// "Save" here would write bytes about to be unlinked (ggo-ide's delete
    /// never dirty-guarded either).
    #[gpui::test]
    async fn test_delete_sprite_prompt_names_unsaved_edits(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        let panel = item_panel_for(&workspace, cx, "sprites/hero.spr");
        dirty_the_sprite(&panel, cx);

        // A different sprite, while the OPEN one is dirty: those edits are
        // not at stake, so the detail must not claim they are.
        let other = delete_sprite_handler(workspace.downgrade(), "sprites/other.spr".to_string());
        cx.update(|window, cx| other(window, cx));
        assert_eq!(
            cx.pending_prompt().map(|(_, detail)| detail),
            Some("This cannot be undone.".to_string()),
            "another sprite's deletion must not warn about THIS one's edits"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        let handler = delete_sprite_handler(workspace.downgrade(), "sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
        assert_eq!(
            cx.pending_prompt().map(|(_, detail)| detail),
            Some("This cannot be undone. Unsaved edits to it will be lost.".to_string()),
        );
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(!dir.path().join("sprites/hero.spr").exists());
    }

    /// Duplicating the OPEN document copies what the panel SHOWS, not the
    /// last-saved bytes: the fixture is 100ms on disk, `dirty_the_sprite`
    /// retimes frame 0 to 500ms in memory only, and the copy must carry the
    /// 500. Reading from disk here would silently produce a "duplicate"
    /// that is missing the user's visible edits -- the case ggo-ide put a
    /// "Discard unsaved changes?" prompt in front of.
    #[gpui::test]
    async fn test_duplicate_sprite_copies_the_live_document_not_the_disk(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        let panel = item_panel_for(&workspace, cx, "sprites/hero.spr");
        dirty_the_sprite(&panel, cx);
        assert_eq!(on_disk_duration(dir.path()), 100, "the edit is unsaved");

        let handler =
            duplicate_sprite_handler(workspace.downgrade(), "sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));

        let copy = open_sprite(dir.path(), "sprites/hero-copy.spr").expect("the copy is a .spr");
        assert_eq!(
            copy.state.frames[0].duration_ms, 500,
            "the copy must carry the unsaved edit the panel is showing"
        );
        assert_eq!(
            on_disk_duration(dir.path()),
            100,
            "and duplicating must not have saved the original"
        );
        panel.update(cx, |panel, _cx| {
            assert!(
                panel.dirty_sprite_name().is_some(),
                "nor cleared the original's dirty state"
            );
        });
    }

    /// Deleting a DIFFERENT sprite leaves the open document alone.
    #[gpui::test]
    async fn test_delete_sprite_leaves_another_open_document_alone(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "sprites/hero.spr");

        let handler = delete_sprite_handler(workspace.downgrade(), "sprites/other.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(!dir.path().join("sprites/other.spr").exists());
        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("deleting another sprite must not disturb the open one");
            };
            assert_eq!(open.source_rel, "sprites/hero.spr");
        });
    }

    // ------------------------------ New Sprite / New Metasprite / Rename

    /// The tileset a new sprite binds to: three tiles, each filled with a
    /// different palette index, so a bound sprite's pool is verifiable
    /// tile by tile.
    const FIXTURE_TILES: usize = 3;

    /// An emerald project holding one tileset and one sprite, laid out the
    /// way the menu entries see it: `emerald.toml` at the root,
    /// `assets/tiles/world.til` (+ `.pal`), `assets/sprites/hero.spr` (+
    /// its own trio). The sprite is deliberately NOT bound to `world.til`
    /// -- a new sprite binding to it must not perturb an unrelated one.
    fn emerald_with_tileset() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("emerald.toml"),
            "[project]\nname='t'\ntitle='t'\n",
        )
        .unwrap();
        let assets = dir.path().join("assets");
        std::fs::create_dir_all(assets.join("tiles")).unwrap();
        std::fs::create_dir_all(assets.join("sprites")).unwrap();
        let mut indices = vec![0u8; FIXTURE_TILES * TILE_PIXELS];
        for (t, chunk) in indices.chunks_exact_mut(TILE_PIXELS).enumerate() {
            chunk.fill(t as u8);
        }
        let mut palette = [0u16; 16];
        palette[1] = 0xF800; // pure 565 red
        palette[2] = 0x07E0; // pure 565 green
        save_tileset(
            &assets,
            "tiles/world.til",
            &indices,
            FIXTURE_TILES,
            &palette,
        )
        .unwrap();
        write_sprite_fixture_at(&assets, "sprites/hero");
        dir
    }

    /// [`menu_workspace`] over [`emerald_with_tileset`]: the fake-fs
    /// worktree is inserted AT the real temp path, so the contributor's
    /// `is_assets_dir` stats (which run against the worktree's `abs_path`)
    /// see the real `emerald.toml`.
    async fn emerald_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        Entity<Workspace>,
        Entity<SpritePanel>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            project_panel::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            serde_json::json!({
                "emerald.toml": "",
                "scratch": { "notes.txt": "" },
                "assets": {
                    "tiles": { "world.til": "", "world.pal": "" },
                    "sprites": { "hero.spr": "", "hero.til": "", "hero.pal": "" },
                },
            }),
        )
        .await;
        let project = Project::test(fs, [root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        // Standalone panel: see `menu_workspace`'s note.
        let weak = workspace.downgrade();
        let panel = cx.update(|_, cx| cx.new(|cx| SpritePanel::new(Some(weak), cx)));
        let root = root.to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));
        // The inline "New Sprite…"/"New Metasprite…" entries seed the
        // project panel's name editor, so the tests need one docked --
        // production gets it from `initialize_workspace`.
        workspace.update_in(cx, |workspace, window, cx| {
            let project_panel = project_panel::ProjectPanel::ggo_test_new(workspace, window, cx);
            workspace.add_panel(project_panel, window, cx);
        });
        (workspace, panel, worktree_id, cx)
    }

    /// Fire the real "New …" handler, type `name` into the project
    /// panel's inline editor, and press Enter -- everything up to the
    /// binding form opening.
    fn name_inline(
        workspace: &Entity<Workspace>,
        kind: NewKind,
        dir_rel: &str,
        dir_abs: std::path::PathBuf,
        name: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .id()
        });
        let handler = new_sprite_handler(
            workspace.downgrade(),
            worktree_id,
            kind,
            dir_rel.to_string(),
            dir_abs,
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        let project_panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<project_panel::ProjectPanel>(cx)
                .expect("docked")
        });
        project_panel.update_in(cx, |panel, window, cx| {
            panel
                .ggo_test_filename_editor()
                .clone()
                .update(cx, |editor, cx| editor.set_text(name, window, cx));
            panel.ggo_test_confirm_edit(window, cx);
        });
        cx.run_until_parked();
    }

    /// The "New …" entries are offered for directories inside the asset
    /// root and for nothing else -- not the project root, not a directory
    /// outside `assets/`, and not a file (which gets the file ops
    /// instead).
    #[gpui::test]
    async fn test_context_menu_offers_new_sprite_entries_on_assets_dirs(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let (workspace, _panel, worktree_id, cx) = emerald_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };
        assert_eq!(
            contributed("assets", true, cx),
            2,
            "the asset root itself: New Sprite + New Metasprite"
        );
        assert_eq!(contributed("assets/sprites", true, cx), 2, "and below it");
        assert_eq!(
            contributed("", true, cx),
            0,
            "the project root is not assets"
        );
        assert_eq!(contributed("scratch", true, cx), 0, "outside assets/");
        assert_eq!(
            contributed("assets/sprites/hero.spr", false, cx),
            3,
            "a file still gets the file ops, not the New entries"
        );
    }

    /// Fire the real menu handler, type `name` inline, and accept the
    /// form's DEFAULT binding with Create -- i.e. exactly the path a user
    /// who never touches the dropdown takes. Deliberate: that default is
    /// the thing fix round 1 BLOCKING 1 was about, so every caller of
    /// this helper is asserting what an untouched dropdown binds to.
    /// Returns the ITEM panel that hosted the form and now holds the
    /// created document (the handler opens a fresh empty item per "New").
    fn new_via_menu(
        workspace: &Entity<Workspace>,
        kind: NewKind,
        dir_rel: &str,
        dir_abs: std::path::PathBuf,
        name: &str,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<SpritePanel> {
        name_inline(workspace, kind, dir_rel, dir_abs, name, cx);
        let panel = newest_item_panel(workspace, cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.run_until_parked();
        panel
    }

    /// "New Sprite…" writes a real, BOUND `.spr`: it round-trips through
    /// worldlib's own `open_sprite`, its sidecar rels are the chosen
    /// tileset's (asset-root-relative, no `assets/` segment -- the
    /// `ggo-sprfix` contract), its pool is that tileset's tiles, it has
    /// exactly ONE frame and NO clips (a sprite is a single frame), and it
    /// is left open in the panel. A second invocation takes the next free
    /// name rather than clobbering the first.
    #[gpui::test]
    async fn test_new_sprite_creates_a_bound_single_frame_sprite(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        // Naming inline opens the form (on a fresh item's panel) with the
        // project's tilesets and writes nothing.
        name_inline(
            &workspace,
            NewKind::Sprite,
            "assets/sprites",
            assets.join("sprites"),
            "hero_idle",
            cx,
        );
        let panel = newest_item_panel(&workspace, cx);
        panel.update(cx, |panel, _| {
            let Some(PanelForm::New {
                kind,
                name,
                tilesets,
                selected,
                ..
            }) = &panel.form
            else {
                panic!("New Sprite… must open the binding form");
            };
            assert_eq!(*kind, NewKind::Sprite);
            assert_eq!(name, "hero_idle", "the typed name is fixed in the form");
            assert_eq!(
                tilesets.iter().map(|c| c.rel.as_str()).collect::<Vec<_>>(),
                vec!["sprites/hero.til", "tiles/world.til"],
                "every .til under the asset root, asset-root-relative"
            );
            assert_eq!(
                *selected, 1,
                "the default skips the sprite-owned tileset (BLOCKING 1)"
            );
        });
        assert!(
            !assets.join("sprites/hero_idle.spr").exists(),
            "opening the form must not write anything yet"
        );

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.run_until_parked();

        let opened = open_sprite(&assets, "sprites/hero_idle.spr").expect("round-trips");
        assert_eq!(opened.til_path, "tiles/world.til");
        assert_eq!(opened.pal_path, "tiles/world.pal");
        assert_eq!(opened.state.tile_count, FIXTURE_TILES, "bound pool");
        assert_eq!(opened.state.frames.len(), 1, "a sprite is ONE frame");
        assert!(opened.state.clips.is_empty(), "and defines no clips");
        assert_eq!(
            (opened.state.w_tiles, opened.state.h_tiles),
            (NEW_SPRITE_TILES, NEW_SPRITE_TILES)
        );
        assert!(
            opened.state.frames[0].map.iter().all(|&t| t == 0),
            "every cell addresses a tile the bound pool actually has"
        );
        // The bytes on disk must never carry the `assets/` prefix.
        let text =
            String::from_utf8_lossy(&std::fs::read(assets.join("sprites/hero_idle.spr")).unwrap())
                .into_owned();
        assert!(text.contains("tiles/world.til"), "{text:?}");
        assert!(!text.contains("assets/tiles/world.til"), "{text:?}");

        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/sprites/hero_idle.spr");
            assert_eq!(open.rel_path, "sprites/hero_idle.spr");
            assert_eq!(open.root, assets);
            assert!(!open.store.dirty(), "creating it is not an unsaved edit");
            let strip = open.pool_strip.as_ref().expect("picker sheet composed");
            assert_eq!(
                strip.tiles,
                vec![1, 2],
                "the picker offers the bound tileset's non-blank tiles \
                 (fixture tile 0 is all index 0 -> hidden)"
            );
            assert!(panel.form.is_none(), "the form closes on Create");
        });

        // A second sprite with its own name lands beside the first, in its
        // own item.
        let second = new_via_menu(
            &workspace,
            NewKind::Sprite,
            "assets/sprites",
            assets.join("sprites"),
            "hero_run",
            cx,
        );
        assert!(assets.join("sprites/hero_run.spr").is_file());
        second.update(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/sprites/hero_run.spr");
        });
    }

    /// "New Metasprite…" seeds the OTHER usage of the same format:
    /// several frames plus a first clip over them. Everything else --
    /// binding, sidecars, round trip -- is identical to a sprite's,
    /// because it is the same file type.
    #[gpui::test]
    async fn test_new_metasprite_seeds_frames_and_a_first_clip(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        let panel = new_via_menu(
            &workspace,
            NewKind::Metasprite,
            "assets",
            assets.clone(),
            "walker",
            cx,
        );

        let opened = open_sprite(&assets, "walker.spr").expect("round-trips");
        assert_eq!(opened.state.frames.len(), NEW_METASPRITE_FRAMES);
        assert_eq!(
            opened.state.clips,
            vec![ClipEdit {
                name: NEW_METASPRITE_CLIP.to_string(),
                from: 0,
                to: NEW_METASPRITE_FRAMES - 1,
                loop_: true,
            }],
            "a metasprite is clip definitions over its frames"
        );
        assert_eq!(
            opened.til_path, "tiles/world.til",
            "an untouched dropdown binds the first UNSHARED tileset, never \
             `sprites/hero.til` -- which sorts first but is hero.spr's own"
        );
        assert!(
            !open_sprite(&assets, "sprites/hero.spr")
                .unwrap()
                .state
                .pool_shared,
            "and the unrelated sprite is not dragged into pool sharing"
        );
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/walker.spr");
        });
    }

    /// Binding must not disturb the tileset it binds TO. `save_sprite`
    /// writes the document's pool back to `til_path`, so a new sprite
    /// rewrites the `.til` it just adopted -- with byte-identical content,
    /// or every existing sprite sharing that tileset silently changes.
    #[gpui::test]
    async fn test_new_sprite_leaves_the_bound_tileset_byte_identical(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let before_til = std::fs::read(assets.join("tiles/world.til")).unwrap();
        let before_pal = std::fs::read(assets.join("tiles/world.pal")).unwrap();
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        new_via_menu(
            &workspace,
            NewKind::Sprite,
            "assets/sprites",
            assets.join("sprites"),
            "fresh",
            cx,
        );
        assert!(
            assets.join("sprites/fresh.spr").is_file(),
            "the form's Create actually wrote the sprite"
        );

        assert_eq!(
            std::fs::read(assets.join("tiles/world.til")).unwrap(),
            before_til,
            "binding must round-trip the .til byte for byte"
        );
        assert_eq!(
            std::fs::read(assets.join("tiles/world.pal")).unwrap(),
            before_pal
        );
    }

    /// Per-file tabs retire the M2 displacement guard: "New …" opens its
    /// form on a FRESH item, so a dirty sprite in another tab is never at
    /// stake -- no prompt, no writes to it, its edits untouched.
    #[gpui::test]
    async fn test_new_sprite_leaves_a_dirty_open_sprite_untouched(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;
        let hero = item_panel_for(&workspace, cx, "assets/sprites/hero.spr");
        hero.update(cx, |panel, cx| {
            assert!(panel.apply_doc(DocOp::FrameDuration { at: 0, ms: 500 }, cx));
        });

        name_inline(
            &workspace,
            NewKind::Sprite,
            "assets/sprites",
            assets.join("sprites"),
            "fresh",
            cx,
        );
        assert!(
            !cx.has_pending_prompt(),
            "the form opens in its own tab; the dirty sprite is not displaced"
        );
        let form_panel = newest_item_panel(&workspace, cx);
        cx.update(|window, cx| form_panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.run_until_parked();

        assert!(assets.join("sprites/fresh.spr").is_file());
        hero.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/sprites/hero.spr");
            assert!(open.store.dirty(), "the edits stay put");
        });
        assert_eq!(
            open_sprite(&assets, "sprites/hero.spr")
                .unwrap()
                .state
                .frames[0]
                .duration_ms,
            100,
            "and nothing was written for them"
        );
    }

    // ------------------------------------- fix round 1: shared tilesets

    fn choice(rel: &str, sharers: &[&str]) -> TilesetChoice {
        TilesetChoice {
            rel: rel.to_string(),
            sharers: sharers.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The default binding, without a filesystem: the first tileset
    /// NOBODY is using, whatever the sort order; the first row only when
    /// every tileset is already owned (creating a sprite must stay
    /// possible), and the labels/warning say so either way.
    #[gpui::test]
    fn test_default_tileset_choice_skips_tilesets_a_sprite_already_owns(_cx: &mut gpui::App) {
        let owned = choice("sprites/hero.til", &["sprites/hero.spr"]);
        let free = choice("tiles/world.til", &[]);
        assert_eq!(
            default_tileset_choice(&[owned.clone(), free.clone()]),
            1,
            "sorts first but is owned -- the default must step past it"
        );
        assert_eq!(default_tileset_choice(&[free.clone(), owned.clone()]), 0);
        assert_eq!(
            default_tileset_choice(&[owned.clone(), choice("b.til", &["b.spr"])]),
            0,
            "every tileset shared: fall back to the first, don't block creation"
        );
        assert_eq!(default_tileset_choice(&[]), 0);

        assert_eq!(free.label(), "tiles/world.til");
        assert_eq!(owned.label(), "sprites/hero.til (used by sprites/hero.spr)");
        assert_eq!(
            choice("a.til", &["x.spr", "y.spr"]).label(),
            "a.til (used by x.spr +1 more)"
        );
        assert_eq!(free.share_warning(), None);
        let warning = owned.share_warning().expect("an owned tileset warns");
        assert!(warning.contains("sprites/hero.spr"), "{warning}");
        assert!(warning.contains("rewrites"), "{warning}");
        assert!(warning.contains("Dedup"), "{warning}");
    }

    /// **Fix round 1, BLOCKING 1.** The form's default must not silently
    /// pool-share an unrelated sprite's tileset -- and when the user picks
    /// one deliberately, the sharers are known so the UI can say so.
    #[gpui::test]
    async fn test_new_sprite_form_defaults_to_an_unshared_tileset(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        name_inline(
            &workspace,
            NewKind::Sprite,
            "assets/sprites",
            assets.join("sprites"),
            "fresh",
            cx,
        );
        let panel = newest_item_panel(&workspace, cx);
        panel.update(cx, |panel, _| {
            let Some(PanelForm::New {
                tilesets, selected, ..
            }) = &panel.form
            else {
                panic!("expected the binding form");
            };
            assert_eq!(
                tilesets[0],
                choice("sprites/hero.til", &["sprites/hero.spr"]),
                "a sprite-owned tileset stays OFFERED -- deliberate sharing is legal"
            );
            assert_eq!(tilesets[1], choice("tiles/world.til", &[]));
            assert_eq!(*selected, 1, "but it is never the default");
            assert_eq!(
                tilesets[*selected].share_warning(),
                None,
                "so the default raises no warning"
            );
        });

        // Deliberately choosing the shared one warns, then binds.
        panel.update(cx, |panel, cx| panel.select_new_tileset(0, cx));
        panel.update(cx, |panel, _| {
            let Some(PanelForm::New {
                tilesets, selected, ..
            }) = &panel.form
            else {
                unreachable!()
            };
            assert!(
                tilesets[*selected].share_warning().is_some(),
                "picking a sprite-owned tileset must warn before Create"
            );
        });
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.run_until_parked();
        assert_eq!(
            open_sprite(&assets, "sprites/fresh.spr").unwrap().til_path,
            "sprites/hero.til",
            "the deliberate choice is honoured"
        );
    }

    /// Binding to a `.til` with no readable `.pal` adopts worldlib's
    /// 16-gray fallback and `save_sprite` writes it -- so creating the
    /// sprite also creates the missing `.pal`. Wanted (a sprite needs a
    /// palette on disk, and the grays are what `ggo_tileset_panel` already
    /// shows for that file), but it is a write next to a tileset the user
    /// only pointed at, so it is pinned rather than left implicit.
    #[gpui::test]
    async fn test_new_sprite_binding_a_pal_less_tileset_writes_the_fallback_palette(
        cx: &mut TestAppContext,
    ) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        std::fs::remove_file(assets.join("tiles/world.pal")).unwrap();
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        new_via_menu(
            &workspace,
            NewKind::Sprite,
            "assets/sprites",
            assets.join("sprites"),
            "fresh",
            cx,
        );

        assert!(
            assets.join("tiles/world.pal").is_file(),
            "the missing .pal is created as a side effect of creating the sprite"
        );
        let opened = open_sprite(&assets, "sprites/fresh.spr").unwrap();
        assert_eq!(opened.pal_path, "tiles/world.pal");
        assert_eq!(
            opened.state.palette[1],
            0x1082, // rgb565(17, 17, 17) -- GRAYSCALE_STEP * 1
            "worldlib's 17-per-step gray ramp, not the sprite default"
        );
    }

    /// The typed-name rules, without a filesystem.
    #[gpui::test]
    fn test_rename_target_keeps_the_file_in_its_directory(_cx: &mut gpui::App) {
        assert_eq!(
            rename_target("assets/sprites/hero.spr", "villain"),
            Ok("assets/sprites/villain.spr".to_string())
        );
        assert_eq!(
            rename_target("hero.spr", " villain "),
            Ok("villain.spr".to_string()),
            "trimmed, and a root-level sprite keeps no directory"
        );
        assert_eq!(
            rename_target("assets/hero.spr", "villain.spr"),
            Ok("assets/villain.spr".to_string()),
            "a retyped extension is not doubled"
        );
        assert!(rename_target("a/hero.spr", "   ").is_err(), "empty");
        assert!(
            rename_target("a/hero.spr", "sub/villain").is_err(),
            "a rename may not move the file: it would strand a sibling sidecar"
        );
        assert!(rename_target("a/hero.spr", "..").is_err());
        assert_eq!(rename_seed("assets/sprites/hero.spr"), "hero");
        assert_eq!(rename_seed("hero.spr"), "hero");
    }

    /// Rename end to end: the file moves, **its sidecars do not** -- and
    /// the renamed sprite still resolves the same `.til`/`.pal` through
    /// worldlib's own `open_sprite`, which is the whole point (the stored
    /// rels are asset-root-relative, so the `.spr`'s own name never
    /// entered into them). The open document follows the file: its title
    /// path and its save target both repoint, and a save after the rename
    /// lands on the new name.
    #[gpui::test]
    async fn test_rename_sprite_preserves_sidecars_and_the_open_document_follows(
        cx: &mut TestAppContext,
    ) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;
        let panel = item_panel_for(&workspace, cx, "assets/sprites/hero.spr");

        let handler =
            rename_sprite_handler(workspace.downgrade(), "assets/sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
        panel.update(cx, |panel, cx| {
            let Some(PanelForm::Rename { editor, .. }) = &panel.form else {
                panic!("Rename Sprite… must open the rename form");
            };
            assert_eq!(editor.read(cx).text(cx), "hero", "prefilled with the stem");
        });
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                let Some(PanelForm::Rename { editor, .. }) = &panel.form else {
                    unreachable!()
                };
                editor.update(cx, |editor, cx| editor.set_text("villain", window, cx));
                panel.confirm_rename(cx);
            })
        });
        cx.run_until_parked();

        assert!(
            !assets.join("sprites/hero.spr").exists(),
            "the old name is gone"
        );
        assert!(assets.join("sprites/villain.spr").is_file());
        assert!(
            assets.join("sprites/hero.til").is_file() && assets.join("sprites/hero.pal").is_file(),
            "the sidecars keep their names -- they are shareable, and renaming \
             them would mean rewriting the stored rels"
        );

        let opened = open_sprite(&assets, "sprites/villain.spr")
            .expect("a renamed sprite must still resolve its tileset");
        assert_eq!(opened.til_path, "sprites/hero.til");
        assert_eq!(opened.pal_path, "sprites/hero.pal");
        assert_eq!(opened.state.tile_count, 2, "the fixture pool came through");

        // The document followed the file, and saves to the new name.
        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                assert!(panel.form.is_none());
                assert_eq!(open.source_rel, "assets/sprites/villain.spr");
                assert_eq!(open.rel_path, "sprites/villain.spr");
            }
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
            panel.save_impl(cx);
            assert!(ready(panel).save_error.is_none());
        });
        assert_eq!(
            open_sprite(&assets, "sprites/villain.spr")
                .unwrap()
                .state
                .frames[0]
                .duration_ms,
            40
        );
    }

    /// A rename onto a name that is already taken is refused inline: the
    /// form stays open with the message, and neither file moves.
    #[gpui::test]
    async fn test_rename_refuses_a_name_that_already_exists(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        write_sprite_fixture_at(&assets, "sprites/other");
        let (workspace, _panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        let handler =
            rename_sprite_handler(workspace.downgrade(), "assets/sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        let panel = newest_item_panel(&workspace, cx);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                let Some(PanelForm::Rename { editor, .. }) = &panel.form else {
                    unreachable!()
                };
                editor.update(cx, |editor, cx| editor.set_text("other", window, cx));
                panel.confirm_rename(cx);
            })
        });

        assert!(assets.join("sprites/hero.spr").is_file(), "nothing moved");
        panel.update(cx, |panel, _| {
            let Some(PanelForm::Rename { error, .. }) = &panel.form else {
                panic!("the form must stay open on a refused name");
            };
            assert_eq!(
                error.as_deref(),
                Some("assets/sprites/other.spr already exists")
            );
        });
    }
}
