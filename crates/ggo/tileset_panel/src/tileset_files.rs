//! Project-panel file operations for a `.til`/`.pal` pair: the delete
//! interceptor and the context-menu contributor, plus the pure rename /
//! duplicate / cascade work they route to.
//!
//! A `.til` is never a self-contained file. Its colours live in the
//! `.pal` beside it, and every `.spr` and `.map` that draws through it
//! stores the tileset's asset-root-relative rel inside itself. Upstream's
//! generic "permanently delete `world.til`?" and generic rename say
//! nothing about either, so a rename through the file explorer silently
//! unbinds every sprite and map in the project. These hooks are what make
//! the pair and its binders visible before anything is moved.
//!
//! DECIDE HERE, ACT LATER (the fork's leased-hook rule): both hooks run
//! while `ProjectPanel` is leased, so their bodies are path inspection
//! plus the worktree lookups `&mut Workspace` already allows, and every
//! prompt or panel touch is pushed into `cx.defer_in` / an entry handler.

use std::path::{Path, PathBuf};

use gpui::{App, Context, Entity, PromptLevel, Task, WeakEntity, Window};
use project::ProjectPath;
use workspace::Workspace;

use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::map_doc::MapState;

use crate::{TILESET_EXT, TilesetEditorItem, TilesetPanel};

/// The palette extension paired with [`crate::TILESET_EXT`].
const PALETTE_EXT: &str = "pal";

/// The assets subdirectory hanging off an emerald project root -- the
/// frame a `.spr`'s/`.map`'s stored tileset rel resolves in. Hardcoded
/// upstream, not an `emerald.toml` key (same constant `ggo_sprite_panel`
/// keeps for the same reason).
const ASSETS_DIR: &str = "assets";

/// The emerald asset root the file at `abs` resolves its sidecar rels
/// against, plus that file's path relative to THAT root -- `None` when
/// the file is not inside an emerald project's `assets/` tree, which is
/// every hook here declining.
fn split_asset_path(abs: &Path) -> Option<(PathBuf, String)> {
    let assets = ggo_common::emerald_project_root(abs.parent()?)?.join(ASSETS_DIR);
    if !assets.is_dir() {
        return None;
    }
    let under = abs.strip_prefix(&assets).ok()?;
    Some((
        assets,
        under.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"),
    ))
}

/// Does `path` name one half of a tileset pair?
fn is_tileset_pair_path(path: &ProjectPath) -> bool {
    path.path.extension().is_some_and(|ext| {
        ext.eq_ignore_ascii_case(TILESET_EXT) || ext.eq_ignore_ascii_case(PALETTE_EXT)
    })
}

/// Swap one half of a tileset pair's extension for the other's,
/// preserving directories and the original file's case-insensitive
/// suffix match. Mirrors worldlib's own (private) `tileset_pal_path`
/// pairing rule, which is what every `.spr`/`.map` binder was written
/// against.
fn paired_rel(rel: &str) -> Option<String> {
    let (stem, ext) = rel.rsplit_once('.')?;
    if ext.eq_ignore_ascii_case(TILESET_EXT) {
        Some(format!("{stem}.{PALETTE_EXT}"))
    } else if ext.eq_ignore_ascii_case(PALETTE_EXT) {
        Some(format!("{stem}.{TILESET_EXT}"))
    } else {
        None
    }
}

/// The `.spr` and `.map` documents under the asset root `root` that store
/// `rel` as their tileset or their palette.
///
/// Deliberately NOT `io::scan_til_sharers`: that answers "is this SHARED",
/// and gates at two referrers so a sole binder reads as none. A delete
/// prompt has to name the sole binder too -- it is exactly the sprite that
/// is about to lose its art.
fn bound_documents(root: &Path, rel: &str) -> (Vec<String>, Vec<String>) {
    let mut sprites = Vec::new();
    let mut maps = Vec::new();
    for file in io::list_all_files(root) {
        match Path::new(&file).extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("spr") => {
                if let Ok(opened) = io::open_sprite(root, &file)
                    && (opened.til_path == rel || opened.pal_path == rel)
                {
                    sprites.push(file);
                }
            }
            Some(ext) if ext.eq_ignore_ascii_case("map") => {
                if let Ok(map) = io::open_map(root, &file)
                    && (map.til_path == rel || map.pal_path == rel)
                {
                    maps.push(file);
                }
            }
            _ => {}
        }
    }
    (sprites, maps)
}

/// What deleting `rel_in_root` (under the asset root `root`) costs beyond
/// the file itself, as prompt cascade lines: every `.spr` and `.map` that
/// draws through it, then the other half of the pair -- which is left on
/// disk, and says so, because this delete removes exactly what was
/// selected.
fn delete_cascade(root: &Path, rel_in_root: &str) -> Vec<String> {
    let (sprites, maps) = bound_documents(root, rel_in_root);
    let mut lines: Vec<String> = sprites
        .into_iter()
        .chain(maps)
        .map(|binder| format!("{binder} draws through it"))
        .collect();
    if let Some(pair) = paired_rel(rel_in_root)
        && root.join(&pair).is_file()
    {
        lines.push(format!("{pair} is its other half and stays on disk"));
    }
    lines
}

/// The noun a claimed path is called in its own prompt.
fn pair_noun(rel: &str) -> &'static str {
    match Path::new(rel).extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case(PALETTE_EXT) => "palette",
        _ => "tileset",
    }
}

/// The title over every "we stopped this delete and here is why" prompt.
const CANT_DELETE_TITLE: &str = "Can't delete these together";

/// A one-button "here is what happened" prompt -- the shape the emerald
/// interceptor uses for a claim it cannot carry out.
fn explain(title: &str, detail: &str, window: &mut Window, cx: &mut App) {
    // The answer is dropped deliberately: one button, nothing to decide.
    let _answer = window.prompt(PromptLevel::Info, title, Some(detail), &["OK"], cx);
}

/// `workspace::DeleteInterceptor` for a `.til`/`.pal` inside an emerald
/// project's asset tree.
///
/// **Why a claim rather than upstream's prompt.** A `.til` is half a
/// pair, and every `.spr`/`.map` that draws through it stores its rel.
/// Upstream's "permanently delete `world.til`?" says nothing about
/// either, so the user cannot tell whether they are about to blank three
/// maps. [`delete_cascade`] names them; a claim is what stops the stock
/// prompt from asking first.
///
/// **And a claim always answers**: every path claimed here ends in a
/// confirm, an unlink, or an explanation -- never silence.
///
/// A multi-selection holding a claimed path is claimed WHOLE and
/// explained rather than split: half the selection through this cascade
/// and half through upstream's unlink, from one keystroke, is worse than
/// either (the emerald interceptor's rule).
pub(crate) fn intercept_tileset_delete(
    workspace: &mut Workspace,
    paths: &[ProjectPath],
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(worktree_root) = primary_worktree_root(workspace, cx) else {
        return false;
    };
    let claimed = paths
        .iter()
        .filter(|path| is_tileset_pair_path(path))
        .filter_map(|path| ggo_common::rel_in_primary_worktree(workspace, path, cx))
        .filter(|rel| split_asset_path(&worktree_root.join(rel)).is_some())
        .collect::<Vec<_>>();
    let Some(rel) = claimed.first().cloned() else {
        return false;
    };
    if paths.len() > 1 {
        let detail = format!(
            "A tileset delete names every sprite and map bound to it, one \
             file at a time, and this selection includes:\n\n{}\n\n\
             Delete them one at a time.",
            claimed.join("\n")
        );
        cx.defer_in(window, move |_workspace, window, cx| {
            explain(CANT_DELETE_TITLE, &detail, window, cx);
        });
        return true;
    }

    // Resolved HERE and handed in: the deferred body re-enters the
    // workspace's own update and so may not read the workspace entity.
    let open_panel = workspace
        .items_of_type::<TilesetEditorItem>(cx)
        .find(|item| item.read(cx).rel() == rel)
        .map(|item| item.read(cx).panel().clone());
    let unsaved = open_panel
        .as_ref()
        .is_some_and(|panel| panel.read(cx).dirty());
    let open_panel = open_panel.map(|panel| panel.downgrade());
    cx.defer_in(window, move |_workspace, window, cx| {
        confirm_tileset_delete(worktree_root, rel, unsaved, open_panel, window, cx).detach();
    });
    true
}

/// The workspace's first visible worktree's absolute path -- the one root
/// every GGO panel resolves against.
fn primary_worktree_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

/// Confirm, then unlink the worktree-relative `rel` under
/// `worktree_root`, clearing the tab showing it once the file is gone.
///
/// Workspace-free on purpose: the only caller reaches it with the
/// workspace leased, so the root, the dirty flag and the tab's panel are
/// resolved by the caller and handed in.
fn confirm_tileset_delete(
    worktree_root: PathBuf,
    rel: String,
    unsaved: bool,
    panel: Option<WeakEntity<TilesetPanel>>,
    window: &mut Window,
    cx: &mut App,
) -> Task<()> {
    let cascade = match split_asset_path(&worktree_root.join(&rel)) {
        Some((root, rel_in_root)) => delete_cascade(&root, &rel_in_root),
        None => Vec::new(),
    };
    // Named, not offered a save: deleting the file makes an unsaved edit
    // to it moot, so this warns instead of offering to write bytes that
    // are about to be unlinked.
    let confirm = ggo_common::confirm_destructive_cascade(
        &format!("Delete the {} {rel}?", pair_noun(&rel)),
        &cascade,
        "Delete",
        unsaved,
        window,
        cx,
    );
    cx.spawn(async move |cx| {
        if !confirm.await {
            return;
        }
        if let Err(e) = std::fs::remove_file(worktree_root.join(&rel)) {
            log::error!("GGO: failed to delete {rel}: {e}");
            let detail = format!("{rel} could not be deleted: {e}");
            cx.update(|cx| {
                let Some(window) = cx.active_window() else {
                    return;
                };
                if let Err(e) = window.update(cx, |_, window, cx| {
                    explain("Delete failed", &detail, window, cx);
                }) {
                    log::error!("GGO: no window for the delete failure prompt: {e}");
                }
            });
            return;
        }
        if let Some(panel) = panel {
            panel
                .update(cx, |panel, cx| panel.clear_if_deleted(&rel, cx))
                .ok();
        }
    })
}

// ------------------------------------------------------- the context menu

/// `workspace::ContextMenuContributor` for `*.til`: the tileset file ops
/// the project panel's own menu can't offer.
///
/// **Rename** and **Duplicate** are here rather than upstream's generic
/// ones because a `.til` is half a pair whose rel is stored inside every
/// `.spr` and `.map` that draws through it. Upstream's rename moves one
/// file and unbinds all of them; upstream's duplicate copies one file and
/// leaves the copy pointing at the ORIGINAL's palette.
///
/// MUST NOT touch any panel: contributors run while `ProjectPanel` is
/// leased. All panel work is deferred into the handlers, which run after
/// the lease is released. (The `is_dir` stat [`split_asset_path`] makes
/// is not panel work and is legal here, same as in `ggo_sprite_panel`.)
pub(crate) fn contribute_tileset_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    if is_dir
        || !path
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(TILESET_EXT))
    {
        return Vec::new();
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    let Some(worktree_root) = primary_worktree_root(workspace, cx) else {
        return Vec::new();
    };
    // Outside an asset root there is no frame for the stored rels to
    // resolve in, so a rename here could not rebind anything correctly.
    if split_asset_path(&worktree_root.join(&rel)).is_none() {
        return Vec::new();
    }
    vec![
        ui::ContextMenuEntry::new("Rename Tileset…")
            .icon(ui::IconName::Pencil)
            .handler(rename_tileset_handler(
                cx.weak_entity(),
                path.worktree_id,
                rel.clone(),
                worktree_root,
            ))
            .into(),
        ui::ContextMenuEntry::new("Duplicate Tileset")
            .icon(ui::IconName::Copy)
            .handler(duplicate_tileset_handler(cx.weak_entity(), rel.clone()))
            .into(),
        ui::ContextMenuEntry::new("Delete Tileset")
            .icon(ui::IconName::Trash)
            .handler(delete_tileset_handler(cx.weak_entity(), rel))
            .into(),
    ]
}

/// The worktree-relative directory `rel` sits in, `""` at the root.
fn parent_rel(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// `<rel's dir>/<stem>.<ext>`.
fn sibling_rel(rel: &str, stem: &str, ext: &str) -> String {
    match rel.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{stem}.{ext}"),
        None => format!("{stem}.{ext}"),
    }
}

/// `rel`'s file stem, or the whole file name when it has no extension.
fn stem_of(rel: &str) -> &str {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    file.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(file)
}

/// The "Rename Tileset…" entry's handler: seed the project panel's inline
/// name editor (New File's UX) in the tileset's own directory, on the
/// tileset's current stem and fully selected; the commit moves the pair
/// and rebinds every document that named it. Split out
/// from [`contribute_tileset_menu`] so a test can invoke exactly what the
/// menu invokes -- `ContextMenuEntry` keeps its handler private.
fn rename_tileset_handler(
    workspace: WeakEntity<Workspace>,
    worktree_id: project::WorktreeId,
    rel: String,
    worktree_root: PathBuf,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(
        workspace.clone(),
        move |panel: &Entity<project_panel::ProjectPanel>, window, cx| {
            let workspace = workspace.clone();
            let rel = rel.clone();
            let worktree_root = worktree_root.clone();
            panel.update(cx, |panel, cx| {
                let Some(dir) = ggo_common::inline_project_path(worktree_id, parent_rel(&rel))
                else {
                    return;
                };
                let stem = stem_of(&rel).to_string();
                panel.ggo_new_entry_inline_seeded(
                    &dir,
                    Some(&stem),
                    rename_validate(worktree_root.clone(), rel.clone()),
                    rename_commit(workspace, worktree_root, rel),
                    window,
                    cx,
                );
            });
        },
    )
}

/// The inline tileset-name gate: [`tileset_stem_error`]'s stem rules plus
/// the already-exists refusal for EITHER half of the pair, surfaced while
/// typing rather than as a failed rename.
fn rename_validate(
    worktree_root: PathBuf,
    rel: String,
) -> impl Fn(&str) -> Option<String> + 'static {
    move |typed| {
        if let Some(error) = tileset_stem_error(typed) {
            return Some(error);
        }
        let typed = typed.trim();
        if typed == stem_of(&rel) {
            return Some("That is already its name.".to_string());
        }
        [TILESET_EXT, PALETTE_EXT].into_iter().find_map(|ext| {
            let candidate = sibling_rel(&rel, typed, ext);
            worktree_root
                .join(&candidate)
                .exists()
                .then(|| format!("{candidate} already exists here."))
        })
    }
}

/// The inline rename commit: move the pair, rebind every binder, and say
/// what was rewritten in the tileset tab's own status line.
fn rename_commit(
    workspace: WeakEntity<Workspace>,
    worktree_root: PathBuf,
    rel: String,
) -> impl FnOnce(String, &mut Window, &mut App) + 'static {
    move |typed, window, cx| {
        let Some((root, rel_in_root)) = split_asset_path(&worktree_root.join(&rel)) else {
            return;
        };
        match rename_tileset(&root, &rel_in_root, &typed) {
            Ok((new_rel_in_root, report)) => {
                let new_rel = sibling_rel(&rel, stem_of(&new_rel_in_root), TILESET_EXT);
                land_status(
                    &workspace,
                    new_rel,
                    Some(rel),
                    format!("Renamed to {new_rel_in_root} — {}", report.summary()),
                    false,
                    window,
                    cx,
                );
            }
            Err(e) => land_status(
                &workspace,
                rel,
                None,
                format!("Rename failed: {e}"),
                true,
                window,
                cx,
            ),
        }
    }
}

/// The "Duplicate Tileset" entry's handler -- see
/// [`rename_tileset_handler`] for why it is a named function.
fn duplicate_tileset_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let Some(root) = workspace
            .upgrade()
            .and_then(|workspace| workspace.read_with(cx, primary_worktree_root))
        else {
            return;
        };
        let Some((assets, rel_in_root)) = split_asset_path(&root.join(&rel)) else {
            return;
        };
        let (message, failed) = match duplicate_tileset(&assets, &rel_in_root) {
            Ok(copy) => (format!("Duplicated to {copy}"), false),
            Err(e) => (format!("Duplicate failed: {e}"), true),
        };
        land_status(&workspace, rel.clone(), None, message, failed, window, cx);
    }
}

/// The "Delete Tileset" entry's handler: the SAME confirm the Delete key
/// raises ([`intercept_tileset_delete`]), so the two routes cannot drift.
fn delete_tileset_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let Some(workspace_entity) = workspace.upgrade() else {
            return;
        };
        let rel = rel.clone();
        let resolved = workspace_entity.update(cx, |workspace, cx| {
            let root = primary_worktree_root(workspace, cx)?;
            let panel = workspace
                .items_of_type::<TilesetEditorItem>(cx)
                .find(|item| item.read(cx).rel() == rel)
                .map(|item| item.read(cx).panel().clone());
            let unsaved = panel.as_ref().is_some_and(|panel| panel.read(cx).dirty());
            Some((root, panel.map(|panel| panel.downgrade()), unsaved))
        });
        let Some((root, panel, unsaved)) = resolved else {
            return;
        };
        confirm_tileset_delete(root, rel, unsaved, panel, window, cx).detach();
    }
}

// --------------------------------------------------- rename and duplicate

/// What a rename rewrote besides the pair itself.
struct RenameReport {
    sprites: Vec<String>,
    maps: Vec<String>,
}

impl RenameReport {
    /// The status line's tail: which documents were rebound, by name, so
    /// the rename is never a silent success.
    fn summary(&self) -> String {
        let rebound = self
            .sprites
            .iter()
            .chain(self.maps.iter())
            .cloned()
            .collect::<Vec<_>>();
        if rebound.is_empty() {
            "nothing else was bound to it".to_string()
        } else {
            format!("rebound {}", rebound.join(", "))
        }
    }
}

/// Why a typed tileset name is not usable, or `None`. The extension is
/// refused outright rather than stripped: both halves are derived from
/// the stem, so "world.til" would have to mean the stem "world" in one
/// place and "world.til" in another.
fn tileset_stem_error(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Some("Name the tileset.".to_string());
    }
    if typed.contains('/') || typed.contains('\\') {
        return Some("A tileset name cannot contain a path separator.".to_string());
    }
    if typed.contains('.') {
        return Some("Leave the extension off — the .til and .pal follow the name.".to_string());
    }
    None
}

/// Move the tileset at `old_rel` (under the asset root `root`) to
/// `new_stem`, taking its `.pal` with it and rewriting the stored rel in
/// every `.spr`/`.map` that named either half. Returns the new rel and
/// what was rebound.
///
/// The pair is COPIED first, the binders are rewritten while the old
/// files are still readable (a `.spr` cannot be opened without its
/// `.til`), and only then are the originals unlinked -- so a failure
/// part-way leaves both names on disk rather than a project pointing at
/// a file that no longer exists.
fn rename_tileset(
    root: &Path,
    old_rel: &str,
    new_stem: &str,
) -> Result<(String, RenameReport), String> {
    if let Some(error) = tileset_stem_error(new_stem) {
        return Err(error);
    }
    let new_stem = new_stem.trim();
    let new_rel = sibling_rel(old_rel, new_stem, TILESET_EXT);
    if new_rel == old_rel {
        return Err("That is already its name.".to_string());
    }
    let old_pal = paired_rel(old_rel).ok_or_else(|| format!("{old_rel} is not a .til"))?;
    let new_pal = sibling_rel(old_rel, new_stem, PALETTE_EXT);
    for candidate in [&new_rel, &new_pal] {
        if root.join(candidate).exists() {
            return Err(format!("{candidate} already exists here."));
        }
    }

    let (sprites, maps) = bound_documents(root, old_rel);
    let copy = |from: &str, to: &str| {
        std::fs::copy(root.join(from), root.join(to))
            .map(|_| ())
            .map_err(|e| format!("copying {from} to {to}: {e}"))
    };
    copy(old_rel, &new_rel)?;
    let had_pal = root.join(&old_pal).is_file();
    if had_pal {
        copy(&old_pal, &new_pal)?;
    }

    for sprite in &sprites {
        let opened = io::open_sprite(root, sprite).map_err(|e| e.to_string())?;
        let til = swap(&opened.til_path, old_rel, &new_rel);
        let pal = swap(&opened.pal_path, &old_pal, &new_pal);
        io::save_sprite(root, sprite, &opened.state, &til, &pal).map_err(|e| e.to_string())?;
    }
    for map_rel in &maps {
        let data = io::open_map(root, map_rel).map_err(|e| e.to_string())?;
        let state = MapState {
            w: data.w,
            h: data.h,
            cells: data.cells,
            til_path: swap(&data.til_path, old_rel, &new_rel),
            pal_path: swap(&data.pal_path, &old_pal, &new_pal),
            dirty: false,
        };
        io::save_map(root, map_rel, &state).map_err(|e| e.to_string())?;
    }

    let unlink = |rel: &str| {
        std::fs::remove_file(root.join(rel)).map_err(|e| format!("removing {rel}: {e}"))
    };
    unlink(old_rel)?;
    if had_pal {
        unlink(&old_pal)?;
    }
    Ok((new_rel, RenameReport { sprites, maps }))
}

/// `stored` rewritten to `to` when it named `from`, left alone otherwise
/// -- a binder may name a DIFFERENT palette than the one paired with the
/// tileset, and that binding is not this rename's to change.
fn swap(stored: &str, from: &str, to: &str) -> String {
    if stored == from {
        to.to_string()
    } else {
        stored.to_string()
    }
}

/// Copy the pair at `rel` (under the asset root `root`) to the first free
/// `<stem>_copy` name, returning the copy's rel. Bytes only: a copy of a
/// tileset binds nothing, so there is no stored rel to rewrite.
fn duplicate_tileset(root: &Path, rel: &str) -> Result<String, String> {
    let taken = |candidate: &str| {
        [TILESET_EXT, PALETTE_EXT]
            .into_iter()
            .any(|ext| root.join(sibling_rel(rel, candidate, ext)).exists())
    };
    let stem = free_copy_stem(stem_of(rel), taken)
        .ok_or_else(|| format!("no free copy name for {rel}"))?;
    let new_rel = sibling_rel(rel, &stem, TILESET_EXT);
    std::fs::copy(root.join(rel), root.join(&new_rel))
        .map_err(|e| format!("copying {rel}: {e}"))?;
    let old_pal = paired_rel(rel).ok_or_else(|| format!("{rel} is not a .til"))?;
    if root.join(&old_pal).is_file() {
        let new_pal = sibling_rel(rel, &stem, PALETTE_EXT);
        std::fs::copy(root.join(&old_pal), root.join(&new_pal))
            .map_err(|e| format!("copying {old_pal}: {e}"))?;
    }
    Ok(new_rel)
}

/// The first free `_copy` stem for `base`: `world` -> `world_copy`, then
/// `world_copy_2`, ... `taken` answers whether a candidate is in use by
/// EITHER half of a pair. Bounded so it cannot spin.
fn free_copy_stem(base: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let first = format!("{base}_copy");
    if !taken(&first) {
        return Some(first);
    }
    (2u32..=999)
        .map(|n| format!("{base}_copy_{n}"))
        .find(|candidate| !taken(candidate))
}

/// Put `message` in the tileset tab's status line, where the user can see
/// what a menu entry did. `retarget_from` is the rel a tab may still be
/// showing under its OLD name (a rename): that tab follows the file
/// rather than being left pointing at a path that no longer exists.
///
/// A file with no tab open gets one, because a menu entry that quietly
/// did nothing visible would be indistinguishable from a broken click.
fn land_status(
    workspace: &WeakEntity<Workspace>,
    rel: String,
    retarget_from: Option<String>,
    message: String,
    failed: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace_entity) = workspace.upgrade() else {
        return;
    };
    let existing = workspace_entity.read_with(cx, |workspace, cx| {
        workspace
            .items_of_type::<TilesetEditorItem>(cx)
            .find(|item| {
                let open = item.read(cx).rel();
                Some(open) == retarget_from.as_deref() || open == rel
            })
    });
    let item = match existing {
        Some(item) => {
            if retarget_from.is_some() {
                item.update(cx, |item, cx| item.adopt_rel(rel, window, cx));
            }
            item
        }
        None => {
            workspace_entity.update(cx, |workspace, cx| {
                crate::open_tileset_item(workspace, rel.clone(), window, cx);
            });
            let Some(opened) = workspace_entity.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<TilesetEditorItem>(cx)
                    .find(|item| item.read(cx).rel() == rel)
            }) else {
                return;
            };
            opened
        }
    };
    let panel = item.read(cx).panel().clone();
    panel.update(cx, |panel, cx| {
        panel.set_file_status(message, failed, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::cow::SpriteState;
    use ggo_worldlib::sprites::io::{save_new_bound_map, save_sprite, save_tileset};
    use ggo_worldlib::sprites::palette565::PAL_SLOTS;
    use ggo_worldlib::sprites::sprite_doc::blank_sprite_state;
    use ggo_worldlib::sprites::tileset_doc::{TILE_PIXELS, pack_indices_to_til};
    use gpui::{Entity, TestAppContext};
    use project::{FakeFs, Project, WorktreeId};
    use project_panel::ProjectPanel;
    use workspace::{AppState, MultiWorkspace};

    /// The fixture tile count -- the same 3 the panel's own fixture uses.
    const FIXTURE_TILES: usize = 3;

    /// A real emerald project on the real filesystem: an asset root with a
    /// tileset pair, one `.spr` bound to it, one `.map` bound to it, and an
    /// unrelated file beside them.
    ///
    /// Real fs, because the hooks read documents with `std::fs` through
    /// worldlib; the `FakeFs` mirror below is only what the worktree scans.
    fn write_project_fixture(root: &Path) -> std::io::Result<()> {
        std::fs::write(root.join(ggo_common::EMERALD_MANIFEST), "")?;
        std::fs::write(root.join("notes.txt"), "")?;
        let assets = root.join(ASSETS_DIR);
        std::fs::create_dir_all(&assets)?;

        let mut indices = vec![0u8; FIXTURE_TILES * TILE_PIXELS];
        for byte in &mut indices[TILE_PIXELS..] {
            *byte = 1;
        }
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800;
        save_tileset(&assets, "tiles/world.til", &indices, FIXTURE_TILES, &palette)
            .expect("writing the fixture tileset");

        let state = SpriteState {
            pool: pack_indices_to_til(&indices, FIXTURE_TILES),
            tile_count: FIXTURE_TILES,
            palette,
            ..blank_sprite_state(2, 2).expect("a 2x2 blank sprite")
        };
        save_sprite(
            &assets,
            "sprites/hero.spr",
            &state,
            "tiles/world.til",
            "tiles/world.pal",
        )
        .expect("writing the fixture sprite");
        save_new_bound_map(&assets, "maps/arena.map", 2, 2, "tiles/world.til")
            .expect("writing the fixture map");
        Ok(())
    }

    /// The `FakeFs` mirror of [`write_project_fixture`] -- what the
    /// worktree scans so the project panel has rows to select. Contents
    /// are irrelevant here; every read that matters goes to the real tree.
    fn fixture_tree() -> serde_json::Value {
        serde_json::json!({
            "emerald.toml": "",
            "notes.txt": "",
            "assets": {
                "tiles": { "world.til": "", "world.pal": "" },
                "sprites": { "hero.spr": "" },
                "maps": { "arena.map": "" },
            },
        })
    }

    /// The fixture behind a real workspace with a real [`ProjectPanel`],
    /// modelled on `ggo_emerald_panel`'s `delete_workspace`.
    async fn file_ops_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Project>,
        Entity<Workspace>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        write_project_fixture(root).expect("the fixture project");
        cx.update(|cx| {
            AppState::test(cx);
            project_panel::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(root, fixture_tree()).await;
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
        workspace.update_in(cx, |workspace, window, cx| {
            let project_panel = ProjectPanel::ggo_test_new(workspace, window, cx);
            workspace.add_panel(project_panel, window, cx);
        });
        cx.run_until_parked();
        (project, workspace, worktree_id, cx)
    }

    fn project_path(worktree_id: WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    /// Select `rel` in the project panel and fire the stock delete action
    /// -- exactly what a user pressing Delete on that row does. Lifted
    /// from `ggo_emerald_panel`'s test of the same hook.
    async fn delete_from_project_panel(
        project: &Entity<Project>,
        worktree_id: WorktreeId,
        rel: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        let mut ancestor = String::new();
        for segment in rel.split('/') {
            let expanded = project.update(cx, |project, cx| {
                let entry = project.entry_for_path(&project_path(worktree_id, &ancestor), cx)?;
                project.expand_entry(worktree_id, entry.id, cx)
            });
            if let Some(expanded) = expanded {
                expanded.await.expect("expanding a directory");
            }
            cx.run_until_parked();
            if !ancestor.is_empty() {
                ancestor.push('/');
            }
            ancestor.push_str(segment);
        }
        let entry_id = project
            .read_with(cx, |project, cx| {
                Some(project.entry_for_path(&project_path(worktree_id, rel), cx)?.id)
            })
            .unwrap_or_else(|| panic!("{rel} is in the worktree"));
        project.update(cx, |_, cx| {
            cx.emit(project::Event::RevealInProjectPanel(entry_id));
        });
        cx.run_until_parked();
        let action = cx
            .update(|_, cx| {
                cx.build_action(
                    "project_panel::Delete",
                    Some(serde_json::json!({ "skip_prompt": false })),
                )
            })
            .expect("project_panel::Delete is a registered action");
        cx.update(|window, cx| window.dispatch_action(action, cx));
        cx.run_until_parked();
    }

    /// **The delete feature, end to end.** Pressing Delete on a `.til`
    /// raises THIS panel's cascade confirm -- the one naming the sprite
    /// and the map still bound to it and the palette it pairs with -- not
    /// upstream's "permanently delete `world.til`?", and confirming
    /// unlinks the `.til` and nothing else.
    #[gpui::test]
    async fn test_deleting_a_til_names_its_binders_then_unlinks_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temp project");
        let (project, _workspace, worktree_id, cx) = file_ops_workspace(cx, dir.path()).await;

        delete_from_project_panel(&project, worktree_id, "assets/tiles/world.til", cx).await;

        let (message, detail) = cx.pending_prompt().expect("the tileset confirm goes up");
        assert_eq!(message, "Delete the tileset assets/tiles/world.til?");
        assert!(
            !message.contains("permanently delete"),
            "the stock file prompt must not be what the user sees: {message}"
        );
        assert!(
            detail.contains("sprites/hero.spr"),
            "the bound sprite has to be named: {detail}"
        );
        assert!(
            detail.contains("maps/arena.map"),
            "the bound map has to be named: {detail}"
        );
        assert!(
            detail.contains("tiles/world.pal"),
            "the paired palette has to be named: {detail}"
        );
        assert!(
            dir.path().join("assets/tiles/world.til").exists(),
            "nothing is unlinked while the prompt is up"
        );

        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(
            !dir.path().join("assets/tiles/world.til").exists(),
            "confirming unlinks the tileset"
        );
        assert!(
            dir.path().join("assets/tiles/world.pal").exists(),
            "and nothing else -- the palette is named, not deleted"
        );
        assert!(
            dir.path().join("assets/sprites/hero.spr").exists(),
            "nor the sprite bound to it"
        );
    }

    /// An unrelated file in the same worktree is declined, so upstream's
    /// own prompt is what the user gets.
    #[gpui::test]
    async fn test_an_unrelated_file_keeps_the_stock_delete_prompt(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temp project");
        let (project, _workspace, worktree_id, cx) = file_ops_workspace(cx, dir.path()).await;

        delete_from_project_panel(&project, worktree_id, "notes.txt", cx).await;

        let (message, _detail) = cx.pending_prompt().expect("upstream's prompt goes up");
        assert!(
            message.contains("permanently delete") && message.contains("notes.txt"),
            "an unclaimed file must reach upstream's own prompt: {message}"
        );
    }
    /// Fire the real "Rename Tileset…" handler and leave the project
    /// panel's inline editor open, as the menu entry does.
    fn open_rename_inline(
        workspace: &Entity<Workspace>,
        rel: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        let (worktree_id, worktree_root) = workspace.read_with(cx, |workspace, cx| {
            let worktree = workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .expect("a visible worktree");
            let worktree = worktree.read(cx);
            (worktree.id(), worktree.abs_path().to_path_buf())
        });
        let handler = rename_tileset_handler(
            workspace.downgrade(),
            worktree_id,
            rel.to_string(),
            worktree_root,
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
    }

    /// [`open_rename_inline`] then type `name` and press Enter.
    fn rename_inline(
        workspace: &Entity<Workspace>,
        rel: &str,
        name: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        open_rename_inline(workspace, rel, cx);
        let project_panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<ProjectPanel>(cx)
                .expect("the project panel is docked")
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

    /// The tileset ops are offered on a `.til` and on nothing else -- not
    /// its `.pal` (whose ops are the tileset's), not an unrelated file,
    /// and not a directory.
    #[gpui::test]
    async fn test_the_menu_offers_the_tileset_ops_only_on_a_til(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temp project");
        let (_project, workspace, worktree_id, cx) = file_ops_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };

        assert_eq!(
            contributed("assets/tiles/world.til", false, cx),
            3,
            "rename, duplicate, delete"
        );
        assert_eq!(contributed("assets/tiles/world.pal", false, cx), 0);
        assert_eq!(contributed("notes.txt", false, cx), 0);
        assert_eq!(contributed("assets/tiles", true, cx), 0);
    }

    /// The rename field opens on the tileset's CURRENT stem, selected --
    /// a rename starts from the name it has, and the selection means the
    /// first keystroke still replaces it wholesale.
    #[gpui::test]
    async fn test_the_rename_field_opens_on_the_current_stem(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temp project");
        let (_project, workspace, _worktree_id, cx) = file_ops_workspace(cx, dir.path()).await;

        open_rename_inline(&workspace, "assets/tiles/world.til", cx);

        let project_panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<ProjectPanel>(cx)
                .expect("the project panel is docked")
        });
        project_panel.update(cx, |panel, cx| {
            panel.ggo_test_filename_editor().update(cx, |editor, cx| {
                assert_eq!(editor.text(cx), "world", "seeded with the current stem");
                let selections = editor
                    .selections
                    .all::<editor::MultiBufferOffset>(&editor.display_snapshot(cx));
                assert_eq!(selections.len(), 1);
                assert_eq!(
                    (selections[0].start, selections[0].end),
                    (
                        editor::MultiBufferOffset(0),
                        editor::MultiBufferOffset("world".len())
                    ),
                    "the whole stem is selected, so typing replaces it"
                );
            });
        });
    }

    /// **Rename, end to end.** Both halves of the pair move, and every
    /// `.spr`/`.map` that stored the old rel is rewritten to the new one
    /// -- the silent unbinding upstream's rename would leave behind.
    #[gpui::test]
    async fn test_renaming_a_tileset_moves_the_pair_and_rebinds_every_binder(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("a temp project");
        let (_project, workspace, _worktree_id, cx) = file_ops_workspace(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);

        rename_inline(&workspace, "assets/tiles/world.til", "meadow", cx);

        assert!(assets.join("tiles/meadow.til").is_file(), "the .til moved");
        assert!(assets.join("tiles/meadow.pal").is_file(), "the .pal with it");
        assert!(!assets.join("tiles/world.til").exists(), "no leftover .til");
        assert!(!assets.join("tiles/world.pal").exists(), "no leftover .pal");

        let sprite = io::open_sprite(&assets, "sprites/hero.spr").expect("the sprite reopens");
        assert_eq!(sprite.til_path, "tiles/meadow.til");
        assert_eq!(sprite.pal_path, "tiles/meadow.pal");
        let map = io::open_map(&assets, "maps/arena.map").expect("the map reopens");
        assert_eq!(map.til_path, "tiles/meadow.til");
        assert_eq!(map.pal_path, "tiles/meadow.pal");

        // And the rename SAYS what it rewrote: the tab follows the file
        // to its new name and reports the rebinding, so a rename through
        // the file explorer is never a silent cascade.
        let item = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<TilesetEditorItem>(cx)
                .next()
                .expect("the renamed tileset has a tab")
        });
        assert_eq!(
            item.read_with(cx, |item, _| item.rel().to_string()),
            "assets/tiles/meadow.til"
        );
        let status = item.read_with(cx, |item, cx| {
            item.panel()
                .read(cx)
                .file_status
                .as_ref()
                .map(|status| (status.message.clone(), status.failed))
        });
        let (message, failed) = status.expect("the rename reports itself");
        assert!(!failed, "a landed rename is not an error: {message}");
        assert!(
            message.contains("tiles/meadow.til")
                && message.contains("sprites/hero.spr")
                && message.contains("maps/arena.map"),
            "the status line names the new file and every rebound document: {message}"
        );
    }

    /// **Duplicate.** Both halves are copied under a free `_copy` name;
    /// the original is untouched.
    #[gpui::test]
    async fn test_duplicating_a_tileset_copies_both_halves(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("a temp project");
        let (_project, workspace, _worktree_id, cx) = file_ops_workspace(cx, dir.path()).await;
        let assets = dir.path().join(ASSETS_DIR);

        let handler = duplicate_tileset_handler(
            workspace.downgrade(),
            "assets/tiles/world.til".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        assert!(assets.join("tiles/world_copy.til").is_file(), "the .til copy");
        assert!(assets.join("tiles/world_copy.pal").is_file(), "the .pal copy");
        assert_eq!(
            std::fs::read(assets.join("tiles/world_copy.til")).expect("the copy reads"),
            std::fs::read(assets.join("tiles/world.til")).expect("the original reads"),
            "a duplicate is a copy, not a re-encode"
        );

        // A second duplicate has to find the next free name rather than
        // clobbering the first.
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        assert!(assets.join("tiles/world_copy_2.til").is_file());
        assert!(assets.join("tiles/world_copy_2.pal").is_file());
    }
}
