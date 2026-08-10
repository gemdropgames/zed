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

mod edits;
mod loader;
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
    Action, App, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, KeyContext, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Render, RenderImage, Styled, Subscription, Task, WeakEntity, Window, actions, div, img, px,
};
use project::ProjectPath;
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, DropdownMenu, ToggleState};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_worldlib::sprites::cow::{ClipEdit, SpriteState};
use ggo_worldlib::sprites::io::{self, open_sprite, save_sprite};
use ggo_worldlib::sprites::sprite_doc::{
    DocOp, SpriteDocStore, blank_sprite_state, clamp_clip_name_bytes,
};
use ggo_worldlib::sprites::tileset_doc::pack_indices_to_til;
use ggo_worldlib::sprites::timeline_ops::{playback_frame_at, playback_total_ms};

actions!(
    ggo_sprite,
    [
        /// Toggles focus on the GGO sprite panel.
        ToggleFocus,
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

const GGO_SPRITE_PANEL_KEY: &str = "GGOSpritePanel";

/// The panel's key-dispatch context identifier. [`dispatch_context`]
/// additionally stamps `editing`/`not_editing` (project_panel's pattern)
/// so plain-key bindings (space) can be scoped away from focused text
/// editors -- see [`bind_panel_keys`].
///
/// [`dispatch_context`]: SpritePanel::dispatch_context
const KEY_CONTEXT: &str = "GgoSpritePanel";

/// Fixed default width until the panel grows real settings persistence.
/// Wider than the other panels' 360px since F5.2: the middle row now
/// carries three columns -- preview, tile picker, clips.
const DEFAULT_WIDTH: Pixels = px(480.);

/// Frame-strip thumbnail box (px, square -- frames fit inside it via
/// `playback::fit_size`).
const THUMB_PX: f32 = 48.0;

/// Large center preview box (px, square).
const PREVIEW_PX: f32 = 240.0;

/// One tile-picker cell's on-screen edge (px, square -- pool tiles are
/// always `TILE_PX` square, so the sheet only needs a uniform scale, no
/// fit math). Also the unit `tiles::picker_tile_at` divides a click by.
const PICKER_CELL_PX: f32 = 24.0;

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

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| SpritePanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SpritePanel>(window, cx);
        });
    })
    .detach();
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
    ggo_common::open_in_panel(
        workspace,
        window,
        cx,
        move |panel: &mut SpritePanel, window, cx| panel.open_rel_path(&rel, window, cx),
    )
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
        if !is_assets_dir(&worktree_root.join(&rel)) {
            return Vec::new();
        }
        return vec![
            ui::ContextMenuEntry::new("New Sprite…")
                .icon(ui::IconName::Plus)
                .handler(new_sprite_handler(
                    cx.weak_entity(),
                    NewKind::Sprite,
                    rel.clone(),
                ))
                .into(),
            ui::ContextMenuEntry::new("New Metasprite…")
                .icon(ui::IconName::Plus)
                .handler(new_sprite_handler(
                    cx.weak_entity(),
                    NewKind::Metasprite,
                    rel,
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

/// The "New Sprite…"/"New Metasprite…" entries' handler -- named for the
/// same reason as [`duplicate_sprite_handler`].
fn new_sprite_handler(
    workspace: WeakEntity<Workspace>,
    kind: NewKind,
    dir_rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(workspace, move |panel: &Entity<SpritePanel>, window, cx| {
        let dir_rel = dir_rel.clone();
        panel.update(cx, |panel, cx| panel.new_sprite(kind, &dir_rel, window, cx));
    })
}

/// The "Rename Sprite…" entry's handler -- see [`duplicate_sprite_handler`].
fn rename_sprite_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(workspace, move |panel: &Entity<SpritePanel>, window, cx| {
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
    ggo_common::panel_entry_handler(
        workspace,
        move |panel: &Entity<SpritePanel>, _window, cx| {
            let rel = rel.clone();
            panel.update(cx, |panel, cx| panel.duplicate_sprite(&rel, cx));
        },
    )
}

/// The "Delete Sprite" entry's handler -- see [`duplicate_sprite_handler`]
/// for why it is a named function.
fn delete_sprite_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(workspace, move |panel: &Entity<SpritePanel>, window, cx| {
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
    /// The stem a new document is named after -- "sprite"/"sprite-2"...,
    /// "metasprite"/"metasprite-2"... (see [`free_new_name`]). No text
    /// prompt at creation time: `window.prompt` is button-choice only, and
    /// renaming afterwards is now a panel form
    /// ([`SpritePanel::begin_rename`]).
    fn name_base(self) -> &'static str {
        match self {
            NewKind::Sprite => "sprite",
            NewKind::Metasprite => "metasprite",
        }
    }

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

/// The first free stem for a new document: `base`, then `base-2`,
/// `base-3`, ... `taken` answers whether a candidate is already in use.
/// Pure, so the naming rule is testable without a filesystem. Only the
/// `.spr` is checked by callers: unlike [`free_copy_base`], a new sprite
/// does NOT mint sidecars of its own -- it binds to an existing tileset,
/// so there is no `.til`/`.pal` of its name to collide.
fn free_new_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2u32..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .expect("a free base-N name exists long before N overflows")
}

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
fn create_sprite(
    project_root: &Path,
    dir_rel: &str,
    kind: NewKind,
    til_rel: &str,
) -> Result<String, String> {
    let dir_abs = project_root.join(dir_rel);
    let name = free_new_name(kind.name_base(), |candidate| {
        dir_abs.join(format!("{candidate}.{SPRITE_EXT}")).exists()
    });
    let file = format!("{name}.{SPRITE_EXT}");
    let source_rel = if dir_rel.is_empty() {
        file
    } else {
        format!("{dir_rel}/{file}")
    };
    let (root, rel_path) = split_sprite_path(project_root, &source_rel);

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
/// instead of rebuilding the editor set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditTarget {
    ClipName(usize),
    ClipFrom(usize),
    ClipTo(usize),
    Duration,
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
    /// The bound tileset composed as the tile picker's sheet; same
    /// invalidation as `frames`.
    pool_strip: Option<loader::PoolStrip>,
    /// The tile picker sheet's on-screen bounds, recorded at prepaint --
    /// same overlay-canvas idiom as [`Self::preview_bounds`].
    picker_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// The active pool tile: while `Some`, a preview-cell click assigns
    /// it (`FrameTileSet`); `None` means clicks don't mutate. Cleared by
    /// re-clicking the tile, Escape, or the tile vanishing under an
    /// undo/fold-back.
    selected_tile: Option<u16>,
    /// The preview image's on-screen bounds, recorded at prepaint by the
    /// overlay canvas so the click handler can map window coords to cell
    /// hits (world_panel's `last_bounds` idiom). `None` until the first
    /// Ready-state paint.
    preview_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    selected_frame: usize,
    /// Index into the doc's clips; `None` = whole-sprite range.
    active_clip: Option<usize>,
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
            pool_strip: loaded.pool_strip,
            picker_bounds: Rc::new(RefCell::new(None)),
            selected_tile: None,
            preview_bounds: Rc::new(RefCell::new(None)),
            selected_frame: 0,
            active_clip: None,
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

    fn durations(&self) -> Vec<u16> {
        self.store
            .state()
            .frames
            .iter()
            .map(|f| f.duration_ms)
            .collect()
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
}

pub struct SpritePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
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
            position: DockPosition::Right,
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
    fn new_sprite(
        &mut self,
        kind: NewKind,
        dir_rel: &str,
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
            tilesets,
            selected,
            ..
        }) = &self.form
        else {
            return;
        };
        let (kind, dir_rel) = (*kind, dir_rel.clone());
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
                this.create_and_open(kind, &dir_rel, &til_rel, cx)
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
        cx: &mut Context<Self>,
    ) {
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        match create_sprite(&project_root, dir_rel, kind, til_rel) {
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
        let pool_strip = loader::compose_pool_strip(open.store.state());
        let frame_count = open.store.state().frames.len();
        let clip_count = open.store.state().clips.len();
        let tile_count = open.store.state().tile_count;
        open.frames = frames;
        open.pool_strip = pool_strip;
        open.selected_frame = open.selected_frame.min(frame_count.saturating_sub(1));
        if open.active_clip.is_some_and(|c| c >= clip_count) {
            open.active_clip = None;
        }
        if open.selected_tile.is_some_and(|t| t as usize >= tile_count) {
            // The tile went away (undo past a COW clone, dedup fold-back
            // on save) -- drop the selection rather than repointing cells
            // at whatever tile inherits the index.
            open.selected_tile = None;
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
        if i >= open.store.state().frames.len() {
            return;
        }
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
        if self.apply_doc(DocOp::FrameDelete { at: i }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = next;
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
        }
    }

    // ----------------------------------------------------------- tile ops

    /// Click a tile-palette thumbnail: select it as the active tile, or
    /// deselect on a re-click of the already-active tile (the brief's
    /// "clicks don't always mutate" affordance, alongside Escape).
    fn select_tile(&mut self, tile: u16, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if (tile as usize) >= open.store.state().tile_count {
            return; // stale click racing an undo
        }
        open.selected_tile = if open.selected_tile == Some(tile) {
            None
        } else {
            Some(tile)
        };
        cx.notify();
    }

    /// The tile picker's left-click body: map the click through the
    /// recorded sheet bounds onto a pool index (`tiles::picker_tile_at`)
    /// and select it. A click on the padded tail of a partial last row
    /// selects nothing rather than the nearest tile.
    fn on_picker_click(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(strip) = open.pool_strip.as_ref() else {
            return;
        };
        let Some(bounds) = *open.picker_bounds.borrow() else {
            return;
        };
        let tile = tiles::picker_tile_at(
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
            PICKER_CELL_PX,
            strip.cols,
            strip.tile_count,
        );
        if let Some(tile) = tile {
            self.select_tile(tile, cx);
        }
    }

    fn deselect_tile(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.selected_tile.is_some()
        {
            open.selected_tile = None;
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

    /// Click a cell of the selected-frame preview with a tile active:
    /// repoint that cell at the tile (`DocOp::FrameTileSet`, M1's op)
    /// through the same `apply_doc` path as every other edit -- undo,
    /// thumbnail recompose, and error surfacing come with it. Targets the
    /// SELECTED frame -- which, on a click during playback, was just
    /// synced to the transport's shown frame by
    /// [`Self::pause_playback_on_edit`]. No-tile-selected and
    /// unchanged-cell clicks are dropped without an op (M5's undo-stack
    /// hygiene rule).
    fn set_tile_on_cell(&mut self, cell: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(tile) = open.selected_tile else {
            return;
        };
        let frame = open.selected_frame;
        let state = open.store.state();
        let Some(f) = state.frames.get(frame) else {
            return;
        };
        if cell >= f.map.len() || (tile as usize) >= state.tile_count {
            return; // stale geometry/selection racing an undo
        }
        if f.map[cell] == tile {
            return; // already that tile -- don't push a no-op undo entry
        }
        self.apply_doc(DocOp::FrameTileSet { frame, cell, tile }, cx);
    }

    /// The preview click handler's window->cell mapping: local coords
    /// against the recorded preview bounds, then `tiles::cell_at` over
    /// the selected frame's tile grid.
    fn preview_cell_at(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
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
        let mut targets = Vec::with_capacity(open.store.state().clips.len() * 3 + 1);
        for i in 0..open.store.state().clips.len() {
            targets.push(EditTarget::ClipName(i));
            targets.push(EditTarget::ClipFrom(i));
            targets.push(EditTarget::ClipTo(i));
        }
        targets.push(EditTarget::Duration);

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
            Some(PanelForm::Rename { editor, .. }) if editor.focus_handle(cx).is_focused(window)
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
        let shown = open.shown_frame();
        let mut preview = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(cx.theme().colors().editor_background);
        if let Some(image) = open.frames.get(shown) {
            let (w, h) = image_px_size(image);
            let (fit_w, fit_h) = playback::fit_size(w, h, PREVIEW_PX);
            let bounds_cell = open.preview_bounds.clone();
            let overlay = gpui::canvas(
                move |bounds, _window, _cx| {
                    *bounds_cell.borrow_mut() = Some(bounds);
                },
                |_, (), _, _| {},
            )
            .absolute()
            .size_full();
            preview = preview.child(
                div()
                    .relative()
                    .w(px(fit_w))
                    .h(px(fit_h))
                    .child(img(image.clone()).w(px(fit_w)).h(px(fit_h)))
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
        let mut column = v_flex()
            .flex_none()
            .w(PICKER_WIDTH)
            .h_full()
            .border_l_1()
            .border_color(border)
            .child(
                div().px_1().pt_1().child(
                    Label::new("Tiles")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        if let Some(strip) = open.pool_strip.as_ref() {
            let sheet_w = px(PICKER_CELL_PX * strip.cols as f32);
            let sheet_h = px(PICKER_CELL_PX * strip.rows as f32);
            let bounds_cell = open.picker_bounds.clone();
            let overlay = gpui::canvas(
                move |bounds, _window, _cx| {
                    *bounds_cell.borrow_mut() = Some(bounds);
                },
                |_, (), _, _| {},
            )
            .absolute()
            .size_full();
            let selection = open.selected_tile.map(|tile| {
                let ix = tile as usize;
                div()
                    .absolute()
                    .left(px((ix % strip.cols) as f32 * PICKER_CELL_PX))
                    .top(px((ix / strip.cols) as f32 * PICKER_CELL_PX))
                    .w(px(PICKER_CELL_PX))
                    .h(px(PICKER_CELL_PX))
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
                            .child(img(strip.image.clone()).w(sheet_w).h(sheet_h))
                            .child(overlay)
                            .children(selection)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    this.on_picker_click(event.position, cx);
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
                tilesets,
                selected,
                error,
            } => {
                let label = format!("{} in {}", kind.label(), dir_rel);
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
        let mut col = v_flex().p_1().gap_1();
        for (i, clip) in state.clips.iter().enumerate() {
            let mut row = v_flex()
                .gap_0p5()
                .p_0p5()
                .border_1()
                .rounded_sm()
                .border_color(if open.active_clip == Some(i) {
                    cx.theme().colors().border_focused
                } else {
                    cx.theme().colors().border_variant
                })
                .child(
                    h_flex()
                        .gap_0p5()
                        .children(
                            editor_for(EditTarget::ClipName(i)).map(|e| Self::editor_input(e, cx)),
                        )
                        .child(
                            IconButton::new(("ggo-sprite-clip-delete", i), IconName::Trash)
                                .icon_size(IconSize::Small)
                                .tooltip(ui::Tooltip::text("Delete clip"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.delete_clip(i, cx)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap_0p5()
                        .children(
                            editor_for(EditTarget::ClipFrom(i)).map(|e| Self::editor_input(e, cx)),
                        )
                        .children(
                            editor_for(EditTarget::ClipTo(i)).map(|e| Self::editor_input(e, cx)),
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
                        }),
                );
            if let Some((at, message)) = &open.clip_error
                && *at == i
            {
                row = row.child(
                    Label::new(SharedString::from(message.clone()))
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                );
            }
            col = col.child(row);
        }
        col = col.child(
            Button::new("ggo-sprite-clip-add", "+ Clip")
                .on_click(cx.listener(|this, _, _, cx| this.add_clip(cx))),
        );
        div()
            .id("ggo-sprite-clips")
            .w(CLIPS_WIDTH)
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .overflow_y_scroll()
            .child(col)
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
                    .disabled(len <= 1)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected_frame(cx))),
            )
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
            .child(Label::new("ms").size(LabelSize::Small).color(Color::Muted))
            .child(
                div().w(px(56.)).flex_none().children(
                    open.editors
                        .iter()
                        .find(|e| e.target == EditTarget::Duration)
                        .map(|e| Self::editor_input(e.editor.clone(), cx)),
                ),
            )
            .children(open.op_error.as_ref().map(|e| {
                Label::new(SharedString::from(e.clone()))
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .into_any_element()
    }

    /// The bottom frame strip: one thumbnail + duration label per frame,
    /// click to select, selected frame outlined.
    fn render_strip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_strip is only called in the Ready state");
        };
        let selected = open.selected_frame;
        let border = cx.theme().colors().border;
        let accent = cx.theme().colors().border_focused;
        let state = open.store.state();
        div()
            .id("ggo-sprite-strip")
            .flex_none()
            .p_1()
            .border_t_1()
            .border_color(border)
            .overflow_x_scroll()
            .child(
                h_flex()
                    .gap_1()
                    .children(state.frames.iter().enumerate().map(|(ix, frame)| {
                        let thumb = open.frames.get(ix).map(|image| {
                            let (w, h) = image_px_size(image);
                            let (fit_w, fit_h) = playback::fit_size(w, h, THUMB_PX);
                            img(image.clone()).w(px(fit_w)).h(px(fit_h))
                        });
                        v_flex()
                            .id(("ggo-sprite-frame", ix))
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
                                Label::new(format!("{} ms", frame.duration_ms))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.select_frame(ix, cx)))
                    })),
            )
            .into_any_element()
    }

    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .child(self.render_transport(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_preview(cx))
                    .child(self.render_tile_picker(cx))
                    .child(self.render_clips(cx)),
            )
            .child(self.render_frame_ops(cx))
            .child(self.render_strip(cx))
            .into_any_element()
    }
}

/// A `RenderImage`'s pixel size (frame 0 -- worldlib composes are always
/// single-frame).
fn image_px_size(image: &Arc<RenderImage>) -> (u32, u32) {
    let size = image.size(0);
    (size.width.0 as u32, size.height.0 as u32)
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

impl EventEmitter<PanelEvent> for SpritePanel {}

impl Panel for SpritePanel {
    fn persistent_name() -> &'static str {
        "GGO Sprite"
    }

    fn panel_key() -> &'static str {
        GGO_SPRITE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call as `ggo_world_panel`: no settings persistence yet, and
        // Bottom isn't a sensible spot for a sprite/frame editor sidebar.
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
        // `IconName` has no Film/animation glyph in this fork; `Image`
        // reads closest to a sprite/frame asset (checked against
        // crates/icons/src/icons.rs -- Sparkle was the other candidate but
        // reads as "AI/magic", not sprites).
        Some(IconName::Image)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO Sprite")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8 (grep activation_priority across
        // crates/).
        9
    }

    /// The open sprite lives in panel state, not in a workspace `Item`, so
    /// nothing else in the close flow knows it can be dirty. Same guard as
    /// `ggo_world_panel`: Save/Don't-Save/Cancel, and a failed write
    /// cancels the close rather than dropping the edits.
    fn prepare_to_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        ggo_common::prepare_to_close_dirty(
            self.dirty_sprite_name(),
            window,
            cx,
            Self::save_for_close,
        )
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_root` needs to READ the
            // workspace to find the project root -- reading it
            // re-entrantly panics (same as `ggo_world_panel`).
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
    use ggo_worldlib::sprites::cow::{ClipEdit, Frame};
    use ggo_worldlib::sprites::hw::{TILE_BYTES, TILE_PX};
    use ggo_worldlib::sprites::io::{open_sprite, save_sprite, save_tileset};
    use ggo_worldlib::sprites::sprite_doc::DEFAULT_FRAME_DURATION_MS;
    use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
    use ggo_worldlib::sprites::timeline_ops::MIN_FRAME_MS;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, WorktreeId};
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock and focuses the
    /// panel. Goes through `MultiWorkspace::test_new` rather than a bare
    /// `Workspace::test_new`, because `register_action` handlers (like
    /// `ToggleFocus`) are only mounted into the dispatch tree once
    /// something renders `Workspace::actions`, which in production is
    /// `MultiWorkspace`'s render (same lesson as `ggo_world_panel`'s test).
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
                workspace.panel::<SpritePanel>(cx).is_some(),
                "SpritePanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<SpritePanel>(cx)
                .expect("SpritePanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    /// Author a real 2-frame sprite trio (`.spr`/`.til`/`.pal`) via
    /// worldlib's own `save_sprite` -- one call persists all three files
    /// (the pool IS the tileset; `save_tileset` would only rewrite the
    /// same `.til`/`.pal` pair, so it isn't needed). Frame 0 shows the
    /// all-transparent tile 0; frame 1 shows tile 1, filled with palette
    /// index 1 (red) -- distinguishable thumbnails. One non-looping
    /// single-frame clip exercises the clip selector path.
    fn write_sprite_fixture(root: &std::path::Path) {
        write_sprite_fixture_named(root, "hero");
    }

    /// [`write_sprite_fixture`] under an arbitrary stem, so a test can hold
    /// two distinct sprites (the routing tests switch between them).
    fn write_sprite_fixture_named(root: &std::path::Path, stem: &str) {
        write_sprite_fixture_at(root, &format!("sprites/{stem}"));
    }

    /// [`write_sprite_fixture`] at an arbitrary root-relative stem, so a
    /// test can put the trio DIRECTLY at an asset root (stem `hh` ->
    /// `hh.spr`/`hh.til`/`hh.pal`) rather than under `sprites/`. That is
    /// the wilds layout, and the one where a wrong root is unrecoverable:
    /// with no subdirectory there is no sibling for `resolve_sidecar` to
    /// fall back to.
    fn write_sprite_fixture_at(root: &std::path::Path, stem: &str) {
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
    fn save_fixture(root: &std::path::Path, spr_rel: &str, til_rel: &str, pal_rel: &str) {
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
                },
                Frame {
                    map: vec![1],
                    duration_ms: 200,
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

    fn ready(panel: &SpritePanel) -> &OpenSprite {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
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

    /// A clean panel must be invisible to the close flow.
    #[gpui::test]
    async fn test_close_guard_lets_a_clean_panel_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        assert!(
            !cx.has_pending_prompt(),
            "a clean sprite must not prompt on close"
        );
        assert!(close.await, "a clean panel must not block the close");
    }

    /// Cancel aborts the close and leaves the document dirty and unwritten.
    #[gpui::test]
    async fn test_close_guard_cancel_aborts_the_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_sprite(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some("sprites/hero.spr contains unsaved edits. Do you want to save it?".to_string()),
        );
        cx.simulate_prompt_answer("Cancel");
        assert!(!close.await, "Cancel must veto the close");

        panel.update(cx, |panel, _cx| {
            assert!(
                panel.dirty_sprite_name().is_some(),
                "Cancel must leave the edits in place"
            );
        });
        assert_eq!(
            on_disk_duration(dir.path()),
            100,
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
        dirty_the_sprite(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        cx.simulate_prompt_answer("Save");
        assert!(close.await, "a successful save must allow the close");

        panel.update(cx, |panel, _cx| {
            assert!(panel.dirty_sprite_name().is_none(), "save clears dirty");
        });
        assert_eq!(
            on_disk_duration(dir.path()),
            500,
            "Save must have written the edit"
        );
    }

    /// "Don't Save" closes and deliberately drops the edits.
    #[gpui::test]
    async fn test_close_guard_discard_allows_close_without_writing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();
        dirty_the_sprite(&panel, cx);

        let close = cx
            .update(|window, cx| panel.update(cx, |panel, cx| panel.prepare_to_close(window, cx)));
        cx.simulate_prompt_answer("Don't Save");
        assert!(close.await, "Don't Save must allow the close");

        assert_eq!(
            on_disk_duration(dir.path()),
            100,
            "Don't Save must not write the file"
        );
    }

    /// The wiring test: a dirty panel docked in a REAL workspace makes
    /// `Workspace::prepare_to_close` (the single funnel for window close,
    /// quit and restart) prompt and, on Cancel, report `false`.
    #[gpui::test]
    async fn test_dirty_panel_vetoes_workspace_prepare_to_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture(dir.path());
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
                .panel::<SpritePanel>(cx)
                .expect("init() adds the panel")
        });

        panel.update(cx, |panel, cx| {
            panel.root_override = Some(dir.path().to_path_buf());
            panel.refresh_root(cx);
            panel.load_rel_path("sprites/hero.spr", cx);
        });
        cx.run_until_parked();
        dirty_the_sprite(&panel, cx);

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

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({
                "sprites": { "hero.spr": "", "other.spr": "", "hero.til": "" },
                "notes.txt": "",
            }),
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
    async fn test_spr_click_routes_into_the_panel_and_is_claimed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<SpritePanel>(cx)
                .expect("init() adds the panel")
        });
        let root = dir.path().to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "sprites/hero.spr"),
                window,
                cx,
            )
        });
        assert!(claimed, "a .spr must be claimed, suppressing the pane item");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(ready(panel).rel_path, "sprites/hero.spr");
        });
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "routing must open the panel's dock even if it was closed"
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
                assert_eq!(strip.tile_count, 2);
                assert_eq!(
                    (strip.cols, strip.rows),
                    (2, 1),
                    "a 2-tile pool is one row, clamped below PICKER_COLS"
                );
                // One sheet, tile 0 then tile 1 side by side: every row is
                // 16 transparent pixels followed by 16 opaque red ones.
                let sheet = strip.image.as_bytes(0).unwrap();
                assert_eq!(sheet.len(), 2 * TILE_PX * TILE_PX * 4);
                for row in sheet.chunks_exact(2 * TILE_PX * 4) {
                    assert!(
                        row[..TILE_PX * 4].chunks_exact(4).all(|p| p[3] == 0),
                        "tile 0 (all index 0) must compose fully transparent"
                    );
                    assert!(
                        row[TILE_PX * 4..]
                            .chunks_exact(4)
                            .all(|p| p == [0, 0, 255, 255]),
                        "tile 1 (palette red) must compose opaque red in BGRA"
                    );
                }
                assert_eq!(open.selected_tile, None);
            }

            // Selection toggles; stale indices are ignored.
            panel.select_tile(1, cx);
            assert_eq!(ready(panel).selected_tile, Some(1));
            panel.select_tile(1, cx);
            assert_eq!(ready(panel).selected_tile, None, "re-click deselects");
            panel.select_tile(9, cx);
            assert_eq!(ready(panel).selected_tile, None, "stale tile index ignored");
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
            assert_eq!(ready(panel).selected_tile, None);
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
            *ready(panel).picker_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(10.), px(20.)),
                gpui::size(px(PICKER_CELL_PX * 2.), px(PICKER_CELL_PX)),
            ));

            panel.on_picker_click(gpui::point(px(12.), px(22.)), cx);
            assert_eq!(ready(panel).selected_tile, Some(0));

            panel.on_picker_click(gpui::point(px(10. + PICKER_CELL_PX), px(22.)), cx);
            assert_eq!(ready(panel).selected_tile, Some(1), "second cell, tile 1");

            // Placing it is the already-shipped `FrameTileSet` path.
            panel.select_frame(0, cx);
            panel.set_tile_on_cell(0, cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1]);
            panel.undo_impl(cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![0]);
            assert!(!ready(panel).store.dirty());

            // Outside the two tiles: no selection change at all.
            panel.on_picker_click(gpui::point(px(9.), px(22.)), cx);
            assert_eq!(ready(panel).selected_tile, Some(1));
            panel.on_picker_click(gpui::point(px(10. + PICKER_CELL_PX * 2.), px(22.)), cx);
            assert_eq!(ready(panel).selected_tile, Some(1));
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
            // Tile 0 active; frame 0 selected (map [0] -- a click on it
            // would be a same-tile no-op); the transport is showing
            // frame 1 (map [1]).
            panel.select_tile(0, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selected_frame = 0;
                open.playing = Some(Playing {
                    started: Instant::now(),
                    start_offset_ms: 0,
                    frame: 1,
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
                    open.selected_frame, 1,
                    "selection adopts the transport's shown frame"
                );
                assert_eq!(
                    open.store.state().frames[1].map,
                    vec![0],
                    "the edit hit the DISPLAYED frame"
                );
                assert_eq!(
                    open.store.state().frames[0].map,
                    vec![0],
                    "the stale pre-playback selection is untouched"
                );
                assert_eq!(open.shown_frame(), 1, "preview stays on the edited frame");
            }

            // Not playing: the same click path edits the selected frame
            // directly (undo the playback edit first so the cell click
            // isn't a same-tile drop).
            panel.undo_impl(cx);
            panel.select_frame(1, cx);
            panel.on_preview_click(gpui::point(px(10.), px(10.)), cx);
            assert_eq!(
                ready(panel).store.state().frames[1].map,
                vec![0],
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
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<SpritePanel>(cx)
                .expect("init() adds the panel")
        });
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
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "sprites/hero.spr");

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
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "sprites/hero.spr");
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
        let (workspace, panel, _worktree_id, cx) = menu_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "sprites/hero.spr");
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
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<SpritePanel>(cx)
                .expect("init() adds the panel")
        });
        let root = root.to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));
        (workspace, panel, worktree_id, cx)
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

    /// Fire the real menu handler and accept the form's DEFAULT binding
    /// with Create -- i.e. exactly the path a user who never touches the
    /// dropdown takes. Deliberate: that default is the thing fix round 1
    /// BLOCKING 1 was about, so every caller of this helper is asserting
    /// what an untouched dropdown binds to.
    fn new_via_menu(
        workspace: &Entity<Workspace>,
        panel: &Entity<SpritePanel>,
        kind: NewKind,
        dir_rel: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        let handler = new_sprite_handler(workspace.downgrade(), kind, dir_rel.to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.run_until_parked();
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
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        // The form opens with the project's tilesets and writes nothing.
        let handler = new_sprite_handler(
            workspace.downgrade(),
            NewKind::Sprite,
            "assets/sprites".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        panel.update(cx, |panel, _| {
            let Some(PanelForm::New {
                kind,
                tilesets,
                selected,
                ..
            }) = &panel.form
            else {
                panic!("New Sprite… must open the binding form");
            };
            assert_eq!(*kind, NewKind::Sprite);
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
            !assets.join("sprites/sprite.spr").exists(),
            "opening the form must not write anything yet"
        );

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.run_until_parked();

        let opened = open_sprite(&assets, "sprites/sprite.spr").expect("round-trips");
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
            String::from_utf8_lossy(&std::fs::read(assets.join("sprites/sprite.spr")).unwrap())
                .into_owned();
        assert!(text.contains("tiles/world.til"), "{text:?}");
        assert!(!text.contains("assets/tiles/world.til"), "{text:?}");

        panel.update(cx, |panel, _| {
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/sprites/sprite.spr");
            assert_eq!(open.rel_path, "sprites/sprite.spr");
            assert_eq!(open.root, assets);
            assert!(!open.store.dirty(), "creating it is not an unsaved edit");
            let strip = open.pool_strip.as_ref().expect("picker sheet composed");
            assert_eq!(
                strip.tile_count, FIXTURE_TILES,
                "the picker offers the bound tileset's tiles"
            );
            assert!(panel.form.is_none(), "the form closes on Create");
        });

        // Second run: next free name, not a clobber.
        new_via_menu(&workspace, &panel, NewKind::Sprite, "assets/sprites", cx);
        assert!(assets.join("sprites/sprite-2.spr").is_file());
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/sprites/sprite-2.spr");
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
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        new_via_menu(&workspace, &panel, NewKind::Metasprite, "assets", cx);

        let opened = open_sprite(&assets, "metasprite.spr").expect("round-trips");
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
            assert_eq!(ready(panel).source_rel, "assets/metasprite.spr");
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
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        let handler = new_sprite_handler(
            workspace.downgrade(),
            NewKind::Sprite,
            "assets/sprites".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.run_until_parked();

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

    /// **The M2 lesson, structurally.** "New …" runs the unsaved-edits
    /// guard BEFORE it opens the form, and the form is the only thing that
    /// writes -- so a Cancel at the prompt leaves no orphaned file, no
    /// form, and the dirty document exactly as it was.
    #[gpui::test]
    async fn test_new_sprite_cancel_with_a_dirty_document_writes_nothing(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "assets/sprites/hero.spr");
        panel.update(cx, |panel, cx| {
            assert!(panel.apply_doc(DocOp::FrameDuration { at: 0, ms: 500 }, cx));
        });

        let handler = new_sprite_handler(
            workspace.downgrade(),
            NewKind::Sprite,
            "assets/sprites".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some(
                "assets/sprites/hero.spr contains unsaved edits. Do you want to save it?"
                    .to_string()
            ),
            "creating a sprite while the open one is dirty must prompt FIRST"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert!(
            !assets.join("sprites/sprite.spr").exists(),
            "Cancel must not leave a file behind"
        );
        panel.update(cx, |panel, _| {
            assert!(panel.form.is_none(), "and no form is left open");
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
            "and nothing was written for them either"
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
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        let handler = new_sprite_handler(
            workspace.downgrade(),
            NewKind::Sprite,
            "assets/sprites".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
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
            open_sprite(&assets, "sprites/sprite.spr").unwrap().til_path,
            "sprites/hero.til",
            "the deliberate choice is honoured"
        );
    }

    /// **Fix round 1, BLOCKING 2.** The viewer stays live under the form
    /// bar, so an edit made AFTER the form opened must still be guarded --
    /// the form-open guard is too early to see it. Cancelling keeps the
    /// edit, the old document, and the form; Don't-Save discards it and
    /// goes through.
    #[gpui::test]
    async fn test_edits_made_while_the_new_form_is_open_are_guarded(cx: &mut TestAppContext) {
        let dir = emerald_with_tileset();
        let assets = dir.path().join("assets");
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "assets/sprites/hero.spr");

        // Open the form CLEAN -- no prompt at this point by construction.
        let handler = new_sprite_handler(
            workspace.downgrade(),
            NewKind::Sprite,
            "assets/sprites".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt(), "a clean document must not prompt");

        // ...then edit the still-live document under the form.
        panel.update(cx, |panel, cx| {
            assert!(panel.apply_doc(DocOp::FrameDuration { at: 0, ms: 500 }, cx));
        });

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        assert_eq!(
            cx.pending_prompt().map(|(msg, _)| msg),
            Some(
                "assets/sprites/hero.spr contains unsaved edits. Do you want to save it?"
                    .to_string()
            ),
            "Create must not replace the document without asking"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();

        assert!(
            !assets.join("sprites/sprite.spr").exists(),
            "Cancel writes nothing"
        );
        panel.update(cx, |panel, _| {
            assert!(panel.form.is_some(), "the form stays open to retry");
            let open = ready(panel);
            assert_eq!(open.source_rel, "assets/sprites/hero.spr");
            assert!(open.store.dirty(), "and the edit is still there");
        });

        // Going through with Save writes the edit before replacing.
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.confirm_new(window, cx)));
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();
        assert_eq!(
            open_sprite(&assets, "sprites/hero.spr")
                .unwrap()
                .state
                .frames[0]
                .duration_ms,
            500,
            "the edit made under the form was saved, not discarded"
        );
        panel.update(cx, |panel, _| {
            assert_eq!(ready(panel).source_rel, "assets/sprites/sprite.spr");
        });
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
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        new_via_menu(&workspace, &panel, NewKind::Sprite, "assets/sprites", cx);

        assert!(
            assets.join("tiles/world.pal").is_file(),
            "the missing .pal is created as a side effect of creating the sprite"
        );
        let opened = open_sprite(&assets, "sprites/sprite.spr").unwrap();
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
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;
        open_in_menu_panel(&panel, cx, "assets/sprites/hero.spr");

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
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        let handler =
            rename_sprite_handler(workspace.downgrade(), "assets/sprites/hero.spr".to_string());
        cx.update(|window, cx| handler(window, cx));
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
