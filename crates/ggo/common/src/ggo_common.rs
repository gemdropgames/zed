//! Shared helpers for GGO fork panels: the RGBA->BGRA `RenderImage`
//! bridge (gpui's `RenderImage` frames are BGRA, see `gpui/src/assets.rs`'s
//! "A cached and processed image, in BGRA format"; worldlib composes
//! straight-alpha RGBA, so only a channel swap is needed, no alpha
//! unpremultiply), the unsaved-document close guard shared by the world and
//! metasprite panels, the destructive-action confirmation every GGO file op
//! goes through, and the file-explorer glue every panel's
//! `PathOpenInterceptor` and `ContextMenuContributor` needs. Deliberately
//! depends on no worldlib, so any GGO panel can use it without pulling in
//! world-doc types. It DOES depend on `workspace` (the routing helpers
//! below take a `Workspace`); that is not a cycle -- `workspace` knows
//! nothing about this crate, it only exposes the `Panel::prepare_to_close`,
//! `PathOpenInterceptor` and `ContextMenuContributor` extension points
//! these helpers plug into.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, PromptLevel, RenderImage, Task, WeakEntity, Window,
};
use image::Frame;
use project::ProjectPath;
use workspace::Workspace;
use workspace::dock::Panel;

/// In-place RGBA8 -> BGRA8 (straight alpha in, straight alpha out --
/// gpui's own non-SVG decode paths do exactly this `swap(0, 2)`, see
/// `gpui/src/elements/img.rs`'s WebP branch; the SVG path's extra alpha
/// divide is for tiny-skia's PREMULTIPLIED output, which worldlib's
/// composes are not).
pub fn rgba_to_bgra(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// Build the one gpui-side image for a composed `w`x`h` RGBA8 buffer.
/// Intended to be called once per source image at load time, never per
/// frame.
pub fn to_render_image(rgba: &[u8], w: u32, h: u32) -> Option<Arc<RenderImage>> {
    let mut data = rgba.to_vec();
    rgba_to_bgra(&mut data);
    let buffer = image::ImageBuffer::from_raw(w, h, data)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

// --------------------------------------------------------- shared db path

/// Database filename under `~/.ggo/`, matching `ggo-ide`'s
/// `backend/db.rs::DB_FILE`.
const DB_FILE: &str = "ggo_ide.db";
const DOT_GGO: &str = ".ggo";

/// `~/.ggo/ggo_ide.db`, matching `ggo-ide`'s `backend/db.rs::default_db_path`.
/// `None` only if neither `HOME` nor `USERPROFILE` resolves (mirrors that
/// function's `anyhow` error, downgraded to `Option` here since neither
/// caller treats an unresolvable home directory as a hard error).
///
/// Shared by `ggo_charts_panel::loader` (reads runs for the picker) and
/// `ggo_emu_panel::ingest` (writes a finished run) -- both touch the SAME
/// file, not copies, so a run the emu pane ingests shows up in the charts
/// panel's picker with no configuration. Kept in one place because the two
/// crates diverging here would be a silent split-brain: the round-trip test
/// that exercises both sides passes a `db_path_override`, so it would never
/// catch drift in this default.
pub fn default_db_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(DOT_GGO).join(DB_FILE))
}

// ------------------------------------------------- unsaved-document guard

/// What the user picked in the unsaved-document prompt raised by
/// [`prepare_to_close_dirty`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseChoice {
    /// Write the document, then close if the write succeeded.
    Save,
    /// Close without writing.
    Discard,
    /// Abort the close.
    Cancel,
}

/// Map a `Window::prompt` answer index onto a [`CloseChoice`], for the
/// `["Save", "Don't Save", "Cancel"]` button set `workspace`'s own dirty-item
/// prompt uses (`workspace/src/pane.rs`, `Pane::save_item`). Anything that
/// is not an explicit Save/Don't-Save -- Cancel, a dismissed dialog, a
/// dropped channel -- is Cancel, which is the safe answer: it keeps the
/// window (and the unsaved doc) alive. `pane.rs`'s `_ => return Ok(false)`
/// arm makes the same call.
pub fn close_choice(answer: Option<usize>) -> CloseChoice {
    match answer {
        Some(0) => CloseChoice::Save,
        Some(1) => CloseChoice::Discard,
        _ => CloseChoice::Cancel,
    }
}

/// The prompt text for a dirty document named `name`, worded like
/// `workspace/src/pane.rs`'s `dirty_message_for`.
pub fn dirty_message(name: &str) -> String {
    format!("{name} contains unsaved edits. Do you want to save it?")
}

/// The body of a GGO panel's `Panel::prepare_to_close`.
///
/// `dirty_name` is `Some(display name)` when the panel holds an unsaved
/// document and `None` when it has nothing to lose -- a clean panel never
/// prompts and never blocks. Otherwise this raises the same
/// Save/Don't-Save/Cancel warning `Pane::save_item` raises for a dirty
/// buffer, and resolves to `false` (cancel the close) on Cancel *and* on a
/// failed save, so a write error can't silently discard the document.
///
/// `save` runs on the panel and returns whether the write succeeded.
pub fn prepare_to_close_dirty<T: 'static>(
    dirty_name: Option<String>,
    window: &mut Window,
    cx: &mut Context<T>,
    save: impl FnOnce(&mut T, &mut Context<T>) -> bool + 'static,
) -> Task<bool> {
    let Some(name) = dirty_name else {
        return Task::ready(true);
    };
    let answer = window.prompt(
        PromptLevel::Warning,
        &dirty_message(&name),
        None,
        &["Save", "Don't Save", "Cancel"],
        cx,
    );
    cx.spawn(
        async move |this, cx| match close_choice(answer.await.ok()) {
            CloseChoice::Save => this.update(cx, save).unwrap_or(false),
            CloseChoice::Discard => true,
            CloseChoice::Cancel => false,
        },
    )
}

// ------------------------------------------------- destructive confirmation

/// The `detail` line every destructive GGO prompt carries. Upstream's own
/// permanent-delete prompt words it exactly this way -- `project_panel.rs`'s
/// `let detail = (!trash).then_some("This cannot be undone.")` -- and the
/// GGO file ops all go through `std::fs::remove_file`/overwrite rather than
/// a trash can, so the caveat always applies and is not worth a parameter.
const DESTRUCTIVE_DETAIL: &str = "This cannot be undone.";

/// Appended to [`DESTRUCTIVE_DETAIL`] when the file being destroyed is the
/// panel's OPEN document and that document has unsaved edits. Worded after
/// upstream's own multi-file delete warning ("N of these have unsaved
/// changes, which will be lost.", `project_panel.rs`).
const UNSAVED_DETAIL: &str = "Unsaved edits to it will be lost.";

/// The `detail` line for a destructive prompt, with the unsaved-edits
/// warning folded in when `unsaved`.
///
/// It is a `bool` and an APPEND rather than a free-form detail parameter on
/// purpose: the "this cannot be undone" half can then never be dropped by a
/// caller that only wanted to say something about dirty state, which is the
/// property that makes the helper worth having at all.
///
/// Note what the `true` case does NOT do: it does not offer to save. Deleting
/// a file makes an unsaved edit to it moot, so routing through
/// [`prepare_to_close_dirty`] would offer a "Save" that writes bytes about to
/// be unlinked. ggo-ide made the same call (`pages/assets/mod.rs`'s delete
/// path never dirty-guards). The user is told, not asked twice.
pub fn destructive_detail(unsaved: bool) -> String {
    if unsaved {
        format!("{DESTRUCTIVE_DETAIL} {UNSAVED_DETAIL}")
    } else {
        DESTRUCTIVE_DETAIL.to_string()
    }
}

/// Does a `Window::prompt` answer over `[confirm_label, "Cancel"]` mean
/// "go ahead"?
///
/// Only an explicit click on the FIRST button does. Cancel, a dismissed
/// dialog, a dropped channel (the window went away mid-prompt) and an index
/// the button list does not have are all "do NOT proceed" -- the same
/// fail-safe direction [`close_choice`] takes, and the same test upstream's
/// own delete makes (`if answer.await != Ok(0) { return }`,
/// `project_panel.rs`). Getting this backwards deletes a file nobody asked
/// to delete, which is why it is a named function with its own test rather
/// than an inline `== Some(0)`.
pub fn confirm_choice(answer: Option<usize>) -> bool {
    answer == Some(0)
}

/// Raise the confirmation a destructive action needs, resolving to whether
/// the user confirmed. `message` should name the thing being destroyed;
/// `confirm_label` is the verb on the go-ahead button ("Delete");
/// `unsaved` is whether the caller's OPEN document is the thing being
/// destroyed AND is dirty (see [`destructive_detail`]).
///
/// Takes `&mut App` rather than a `Context<T>` so a project-panel
/// context-menu entry handler -- which is handed only `(&mut Window,
/// &mut App)` -- can call it directly.
pub fn confirm_destructive(
    message: &str,
    confirm_label: &str,
    unsaved: bool,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    let answer = window.prompt(
        PromptLevel::Info,
        message,
        Some(&destructive_detail(unsaved)),
        &[confirm_label, "Cancel"],
        cx,
    );
    cx.background_spawn(async move { confirm_choice(answer.await.ok()) })
}

// -------------------------------------------------- explorer-driven routing

/// The project-relative, `/`-separated path `path` names -- but only when it
/// lives in the workspace's FIRST visible worktree, which is the one (and
/// only) worktree every GGO panel resolves its project root from. A path in
/// any other worktree yields `None` so an interceptor declines it instead of
/// loading the same rel path out of the wrong project.
///
/// A non-local project (SSH remote, or a collab guest) yields `None` for the
/// same reason: the GGO panels read their documents with `std::fs` against
/// the worktree's `abs_path`, which on a remote project names a directory
/// that does not exist on this machine. Declining lets the click fall through
/// to upstream's normal open, which DOES understand remote projects -- an
/// editor tab beats a panel stuck in an error state.
pub fn rel_in_primary_worktree(
    workspace: &Workspace,
    path: &ProjectPath,
    cx: &App,
) -> Option<String> {
    let project = workspace.project().read(cx);
    if !project.is_local() {
        return None;
    }
    let primary = project.visible_worktrees(cx).next()?.read(cx).id();
    (primary == path.worktree_id).then(|| path.path.as_unix_str().to_string())
}

/// Reveal + focus panel `P` and run `open` on it -- the body every GGO
/// `PathOpenInterceptor` ends with. Returns `false` (i.e. "not claimed, open
/// it the normal way") when no `P` is docked in this workspace, so a missing
/// panel degrades to upstream's editor rather than swallowing the click.
pub fn open_in_panel<P: Panel>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    open: impl FnOnce(&mut P, &mut Window, &mut Context<P>),
) -> bool {
    let Some(panel) = workspace.focus_panel::<P>(window, cx) else {
        return false;
    };
    // A zoomed center item would otherwise hide the dock we just focused.
    workspace.reveal_panel::<P>(window, cx);
    panel.update(cx, |panel, cx| open(panel, window, cx));
    true
}

/// Wrap `action` into the `Fn(&mut Window, &mut App)` a
/// `ui::ContextMenuEntry::handler` takes, resolving GGO panel `P` out of
/// the workspace first.
///
/// A `workspace::ContextMenuContributor` is a bare `fn` pointer (it cannot
/// capture) and the entry handlers it builds are handed only `(&mut Window,
/// &mut App)` -- no workspace, no panel. The one bridge is a
/// `WeakEntity<Workspace>` taken with `cx.weak_entity()` while the
/// contributor runs; this turns that handle into the panel every GGO
/// file-op entry actually wants to act on. Weak on purpose: a menu entry
/// outlives nothing in particular, and a closed window must not keep a
/// workspace alive.
///
/// Doing nothing when the workspace is gone or `P` is not docked is the
/// right answer for all of them: the entry only exists because the panel's
/// contributor put it there, so its absence means the action has no target.
///
/// SAFE TO CALL HERE, unlike from a contributor: contributors run while
/// `ProjectPanel` is leased (see `Workspace::context_menu_contributions`)
/// and panic if they touch a panel; handlers run after the lease is
/// released. `action` is also not inside a `Workspace` update, so a panel
/// method that reads the workspace back (`refresh_worlds`, `refresh_root`)
/// is fine from here.
pub fn panel_entry_handler<P: Panel>(
    workspace: WeakEntity<Workspace>,
    action: impl Fn(&Entity<P>, &mut Window, &mut App) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        let Some(panel) = workspace.read(cx).panel::<P>(cx) else {
            return;
        };
        action(&panel, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic red/blue swap bug: RGBA in, BGRA out, alpha untouched.
    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_only() {
        let mut data = vec![10, 20, 30, 40, 1, 2, 3, 4];
        rgba_to_bgra(&mut data);
        assert_eq!(data, vec![30, 20, 10, 40, 3, 2, 1, 4]);
    }

    #[test]
    fn to_render_image_produces_one_frame_of_the_right_size() {
        let rgba = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let rendered = to_render_image(&rgba, 2, 1).unwrap();
        assert_eq!(rendered.frame_count(), 1);
        // Red pixel first: BGRA bytes [0, 0, 255, 255].
        assert_eq!(
            rendered.as_bytes(0).unwrap(),
            &[0, 0, 255, 255, 255, 0, 0, 255]
        );
    }

    /// The prompt's button order is `["Save", "Don't Save", "Cancel"]`, and
    /// every non-answer (dismissed dialog, dropped channel, an index the
    /// button list doesn't have) must fall back to Cancel -- the only
    /// choice that can't lose the document.
    #[test]
    fn close_choice_maps_button_indices_and_fails_safe() {
        assert_eq!(close_choice(Some(0)), CloseChoice::Save);
        assert_eq!(close_choice(Some(1)), CloseChoice::Discard);
        assert_eq!(close_choice(Some(2)), CloseChoice::Cancel);
        assert_eq!(close_choice(Some(99)), CloseChoice::Cancel);
        assert_eq!(close_choice(None), CloseChoice::Cancel);
    }

    /// The destructive prompt's button order is `[<verb>, "Cancel"]`, and
    /// every non-answer (Cancel, a dismissed dialog, a dropped channel, an
    /// index the button list doesn't have) must fall back to "don't" --
    /// the only choice that can't delete a file nobody asked to delete.
    #[test]
    fn confirm_choice_only_proceeds_on_the_first_button() {
        assert!(confirm_choice(Some(0)));
        assert!(!confirm_choice(Some(1)));
        assert!(!confirm_choice(Some(99)));
        assert!(!confirm_choice(None));
    }

    /// The unsaved warning is an APPEND: the "cannot be undone" half is
    /// there either way, and the dirty case adds to it rather than
    /// replacing it.
    #[test]
    fn destructive_detail_appends_the_unsaved_warning() {
        assert_eq!(destructive_detail(false), "This cannot be undone.");
        assert_eq!(
            destructive_detail(true),
            "This cannot be undone. Unsaved edits to it will be lost."
        );
    }

    /// `default_db_path` must land on `~/.ggo/ggo_ide.db` -- the file both
    /// `ggo_charts_panel` and `ggo_emu_panel` read/write. HOME reliably
    /// resolves in the test environment.
    #[test]
    fn default_db_path_is_dot_ggo_ggo_ide_db() {
        let path = default_db_path().expect("HOME resolves in the test env");
        assert!(path.ends_with(".ggo/ggo_ide.db"));
    }

    #[test]
    fn dirty_message_names_the_document() {
        assert_eq!(
            dirty_message("worlds/test.toml"),
            "worlds/test.toml contains unsaved edits. Do you want to save it?"
        );
    }
}
