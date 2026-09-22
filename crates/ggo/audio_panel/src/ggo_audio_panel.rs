//! GGO Audio: a center-pane tab per `.wav` / `.ogg` / `.adp` that lets the
//! studio HEAR a musician's file, hear what the hardware will make of it,
//! pick the baked rate, and write the `.adp` the cart ships.
//!
//! There is deliberately no synthesis, sequencer, or PSG/ADSR/pan authoring
//! here: musicians deliver finished wav/ogg, and emerald's runtime never
//! programs those APU dimensions, so nothing authored in them could ship.
//! What the editor owns is the trip from a delivered file to a cart asset
//! -- `ggo_audio` does the codec work, this panel is the surface.
//!
//! **Import, not sidecar.** The rate knob is editor-side: Import bakes at
//! the chosen rate and writes `assets/<stem>.adp`, which `emd pack-ggo`
//! copies verbatim (`AssetKind::Adp`). Emerald is not touched and knows
//! nothing about this editor. Dropping a raw `.wav`/`.ogg` under `assets/`
//! keeps working at emerald's default rate for anyone who doesn't care.
//!
//! **Baked preview is the real thing.** `preview.rs` runs the blob through
//! a standalone `ggo_emu_core::apu::Apu` -- 4-bit ADPCM, the 4.12 phase
//! step, the 32 kHz mix -- into the emulator pane's cpal ring. Source is
//! the decoded PCM as delivered. A/B between them is the whole point of
//! the rate picker.

mod audio_item;
mod editor_meta;
mod import_modal;
mod load;
mod preview;
mod world_refs;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use editor::Editor;
use gpui::{
    App, Bounds, Context, Entity, EntityId, FocusHandle, Focusable, IntoElement, MouseButton,
    MouseDownEvent, Pixels, Render, Styled, Task, WeakEntity, Window, actions, div, point, px,
    size,
};
use project::ProjectPath;
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, DropdownMenu, ToggleState};
use workspace::Workspace;

use ggo_audio::Decoded;
use ggo_daemon_client::{AudioBudget, AudioProbe, Connect};
use ggo_emu_panel::audio::AudioStatus;

pub use audio_item::AudioItem;
use import_modal::ImportModal;
use load::Loaded;
use preview::{Preview, Spec, BLOCK_BYTES, SAMPLES_PER_BLOCK};

actions!(
    ggo_audio,
    [
        /// Plays or stops the audio preview.
        PlayStop,
        /// Toggles looping playback.
        ToggleLoop,
    ]
);

const KEY_CONTEXT: &str = "GgoAudioPanel";

/// The extensions this tab claims from the file explorer: the two source
/// containers emerald bakes, and the baked form itself.
const AUDIO_EXTS: [&str; 3] = ["wav", "ogg", "adp"];

/// The baked form's extension -- what Import writes and what the delete
/// interceptor claims.
const BAKED_EXT: &str = "adp";

/// The waveform's height before any [`Divider::Waveform`] drag.
const WAVEFORM_HEIGHT_PX: f32 = 160.0;

/// The smallest a waveform drag may leave the canvas: below this the
/// outline is a smear rather than a shape, and the handle would end up
/// on top of the header row it was dragged past.
const MIN_WAVEFORM_HEIGHT: Pixels = px(48.);

/// The grab strip's thickness, `workspace::dock`'s resize-handle figure.
const DIVIDER_SIZE: Pixels = px(6.);

// `debug_selector` handles for the regions whose overflow behaviour the
// layout tests assert. gpui records a selector's painted bounds only in
// test builds (`div.rs` discards the closure unevaluated otherwise), so
// these cost nothing shipped.
const HEADER_SELECTOR: &str = "ggo-audio-header";
const TRANSPORT_SELECTOR: &str = "ggo-audio-transport";
/// The transport's Import button -- the card's only entry point from the
/// tab, and what a rendered test clicks.
const IMPORT_SELECTOR: &str = "ggo-audio-import";
/// The waveform canvas itself -- present only while the section is shown.
const WAVEFORM_SELECTOR: &str = "ggo-audio-waveform";
/// The grab strip under the waveform.
const WAVEFORM_DIVIDER_SELECTOR: &str = "ggo-audio-divider-waveform";
/// The centred one-line message that stands in for the viewer when there
/// is nothing to show -- empty, loading, failed, or deleted.
const MESSAGE_SELECTOR: &str = "ggo-audio-message";
/// The playhead redraw cadence while a preview runs.
const PLAYHEAD_TICK: Duration = Duration::from_millis(33);

pub fn init(cx: &mut App) {
    workspace::register_path_open_interceptor(cx, intercept_audio_open);
    // Upstream's delete would unlink a `.adp` silently, leaving every
    // world that names its stem playing nothing -- see
    // [`intercept_audio_delete`].
    workspace::register_delete_interceptor(cx, intercept_audio_delete);
    workspace::register_context_menu_contributor(cx, contribute_audio_menu);
}

/// `workspace::ContextMenuContributor` for audio files: Import on one of
/// the two source containers, Delete on the baked form.
///
/// MUST NOT touch the project panel or any GGO panel: contributors run
/// while `ProjectPanel` is leased (see
/// `Workspace::context_menu_contributions`). Everything panel-shaped is
/// deferred into the entries' handlers, which run after the lease is
/// released. The `is_dir` stat the asset-root check makes is not panel
/// work and is legal here, same as in the sibling panels.
fn contribute_audio_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    if is_dir {
        return Vec::new();
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    let Some(worktree_root) = primary_worktree_root(workspace, cx) else {
        return Vec::new();
    };
    let extension = Path::new(&rel)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "wav" | "ogg" => vec![
            ui::ContextMenuEntry::new("Import as .adp…")
                .icon(ui::IconName::Plus)
                .handler(import_entry_handler(cx.weak_entity(), rel))
                .into(),
        ],
        // Only inside an asset root: a stray `.adp` elsewhere is no
        // world's audio, so this crate has nothing to say about it that
        // upstream's own delete does not.
        ext if ext == BAKED_EXT
            && world_refs::split_asset_path(&worktree_root.join(&rel)).is_some() =>
        {
            vec![
            ui::ContextMenuEntry::new("Delete Audio")
                .icon(ui::IconName::Trash)
                .handler(delete_entry_handler(cx.weak_entity(), worktree_root, rel))
                .into(),
            ]
        }
        _ => Vec::new(),
    }
}

/// The Import entry's handler: open the clicked source's tab and raise
/// the card over it. Split out from [`contribute_audio_menu`] so a test
/// can invoke exactly what the menu invokes -- `ContextMenuEntry` keeps
/// its handler private, so a contributed entry cannot be fired any other
/// way.
///
/// **Deliberately NOT `ggo_common::panel_entry_handler`**: this tab is a
/// center-pane item, not a dock panel, so there is no dock to reveal --
/// and revealing one would evict whatever the user was looking at. The
/// entry runs after the project panel's lease is released, so reaching
/// the workspace directly here is legal.
fn import_entry_handler(
    workspace: WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        let rel = rel.clone();
        workspace.update(cx, |workspace, cx| {
            open_audio_item(workspace, rel.clone(), window, cx);
            let Some(panel) = workspace
                .items_of_type::<AudioItem>(cx)
                .find(|item| item.read(cx).rel() == rel)
                .map(|item| item.read(cx).panel_entity().clone())
            else {
                return;
            };
            // The card itself goes up on `window.defer` from in here --
            // see `AudioPanel::show_import_card` for why it has to.
            panel.update(cx, |panel, cx| panel.show_import_card(window, cx));
        });
    }
}

/// The Delete entry's handler -- the same confirm-and-unlink route
/// [`intercept_audio_delete`] takes, so the two cannot answer the same
/// question differently. Split out for the same testability reason as
/// [`import_entry_handler`].
fn delete_entry_handler(
    workspace: WeakEntity<Workspace>,
    worktree_root: PathBuf,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        // Legal here: the entry runs after the project panel's lease is
        // released, so the workspace may be read for the tabs to clear.
        let showing = workspace
            .read_with(cx, |workspace, cx| panels_showing(workspace, &rel, cx))
            .unwrap_or_default();
        confirm_audio_delete(worktree_root.clone(), rel.clone(), showing, window, cx).detach();
    }
}

/// The title over the "we stopped this delete and here is why" prompt.
const CANT_DELETE_TITLE: &str = "Can't delete these together";

/// A one-button prompt. The answer is dropped deliberately: there is
/// nothing to decide.
fn explain(title: &str, detail: &str, window: &mut Window, cx: &mut App) {
    let _answer = window.prompt(gpui::PromptLevel::Info, title, Some(detail), &["OK"], cx);
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

/// `workspace::DeleteInterceptor` for a `.adp` inside an emerald
/// project's asset tree.
///
/// **Why a claim rather than upstream's prompt.** A `.adp` is named by
/// STEM from worlds that never mention the file, so "permanently delete
/// `jump.adp`?" tells the user nothing about the three levels that go
/// silent when they say yes. [`world_refs::worlds_playing`] names them.
///
/// **And a claim always answers**: every path claimed here ends in a
/// confirm, an unlink, or an explanation -- never silence.
///
/// A multi-selection holding a claimed path is claimed WHOLE and
/// explained rather than split: half the selection through this cascade
/// and half through upstream's unlink, from one keystroke, is worse than
/// either (the emerald interceptor's rule).
///
/// MUST decide synchronously from path inspection: this runs while
/// `ProjectPanel` is leased, so everything panel- or prompt-shaped is
/// pushed into `cx.defer_in(window, ..)` with the root resolved here and
/// handed in -- the deferred body re-enters the workspace's own update
/// and so may not read the workspace entity either.
fn intercept_audio_delete(
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
        .filter(|path| {
            path.path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(BAKED_EXT))
        })
        .filter_map(|path| ggo_common::rel_in_primary_worktree(workspace, path, cx))
        .filter(|rel| world_refs::split_asset_path(&worktree_root.join(rel)).is_some())
        .collect::<Vec<_>>();
    let Some(rel) = claimed.first().cloned() else {
        return false;
    };
    if paths.len() > 1 {
        let detail = format!(
            "An audio delete names every world that plays the stem, one \
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
    let showing = panels_showing(workspace, &rel, cx);
    cx.defer_in(window, move |_workspace, window, cx| {
        confirm_audio_delete(worktree_root, rel, showing, window, cx).detach();
    });
    true
}

/// The panels of every open tab showing worktree-relative `rel`.
fn panels_showing(
    workspace: &Workspace,
    rel: &str,
    cx: &App,
) -> Vec<WeakEntity<AudioPanel>> {
    workspace
        .items_of_type::<AudioItem>(cx)
        .filter(|item| item.read(cx).rel() == rel)
        .map(|item| item.read(cx).panel_entity().downgrade())
        .collect()
}

/// Confirm, then unlink the worktree-relative `rel` under `worktree_root`.
///
/// Workspace-free on purpose: the interceptor reaches it with the
/// workspace leased, so the root -- and the panels of the tabs showing
/// this file, which must stop painting it once it is gone -- are
/// resolved by the caller and handed in.
fn confirm_audio_delete(
    worktree_root: PathBuf,
    rel: String,
    showing: Vec<WeakEntity<AudioPanel>>,
    window: &mut Window,
    cx: &mut App,
) -> Task<()> {
    let cascade = audio_delete_cascade(&worktree_root, &rel);
    let confirm = ggo_common::confirm_destructive_cascade(
        &format!("Delete the audio {rel}?"),
        &cascade,
        "Delete",
        false,
        window,
        cx,
    );
    cx.spawn(async move |cx| {
        if !confirm.await {
            return;
        }
        let Err(e) = std::fs::remove_file(worktree_root.join(&rel)) else {
            // The tabs still painting the decoded file it no longer is.
            for panel in showing {
                panel
                    .update(cx, |panel, cx| panel.clear_if_deleted(&rel, cx))
                    .ok();
            }
            return;
        };
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
    })
}

/// The worlds that go silent if the `.adp` at worktree-relative `rel` is
/// removed or replaced.
fn audio_delete_cascade(worktree_root: &Path, rel: &str) -> Vec<String> {
    let Some((asset_root, asset_rel)) = world_refs::split_asset_path(&worktree_root.join(rel))
    else {
        return Vec::new();
    };
    let Some(stem) = world_refs::adp_stem(&asset_rel) else {
        return Vec::new();
    };
    world_refs::worlds_playing(&asset_root, stem)
}

/// Whether `path` is a file this tab opens (extension only, case-insensitive).
pub fn claims(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// `workspace::PathOpenInterceptor` for audio files: claim the path and
/// open (or focus) its tab. Declines for anything else and for a path
/// outside the primary worktree.
fn intercept_audio_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !claims(path.path.as_std_path()) {
        return false;
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    open_audio_item(workspace, rel, window, cx);
    true
}

/// Open (or focus) the tab for worktree-relative `rel` -- one per file.
pub fn open_audio_item(
    workspace: &mut Workspace,
    rel: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<AudioItem>(cx)
        .find(|item| item.read(cx).rel() == rel);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let item = cx.new(|cx| AudioItem::new(rel, weak, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

/// The Import target for source `rel`: the same name under `assets/`
/// with the baked extension. A source already under `assets/` keeps its
/// directory (so `assets/sfx/jump.wav` → `assets/sfx/jump.adp`); anything
/// else lands flat in `assets/` (emerald's stem is the path under the
/// asset root, so this is what a world's `Sfx{stem}` will name).
pub fn default_import_target(rel: &str) -> String {
    let path = Path::new(rel);
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_string());
    if rel.starts_with("assets/") {
        let dir = path.parent().unwrap_or(Path::new(""));
        let dir = dir.to_string_lossy();
        if dir.is_empty() {
            format!("{name}.adp")
        } else {
            format!("{dir}/{name}.adp")
        }
    } else {
        format!("assets/{name}.adp")
    }
}

// ------------------------------------------------------------- view state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Source,
    Baked,
}

pub(crate) enum ViewerState {
    Empty,
    Loading(String),
    Error { rel: String, message: String },
    /// The open file was deleted out from under the tab. Its own state
    /// rather than an `Error`: nothing failed, and the difference is
    /// what the tab says to the user.
    Deleted(String),
    Ready(Open),
}

pub(crate) struct Open {
    pub(crate) rel: String,
    pub(crate) is_adp: bool,
    /// The clip's shape, as the daemon reported it. Its `waveform` is
    /// moved out into [`Self::waveform`] at load -- see there.
    pub(crate) probe: AudioProbe,
    /// The outline the canvas paints, behind an `Arc` because `render`
    /// runs on every paint and this is ~2048 pairs.
    waveform: Arc<Vec<(i16, i16)>>,
    /// What a baked blob costs and which rates may be offered -- the
    /// daemon's answer, so this panel holds no copy of either.
    pub(crate) budget: AudioBudget,
    /// PCM for the Source-mode preview only. `None` for a `.adp`, which
    /// previews from its blob. Goes in P4 with the preview itself.
    pub(crate) decoded: Option<Arc<Decoded>>,
    /// The rate the bake (and Import) uses. For a `.adp` this is the
    /// file's own rate and cannot change.
    pub(crate) rate: u32,
    /// The `.adp` blob at `rate`, once the bake has landed.
    pub(crate) baked: Option<Arc<Vec<u8>>>,
    baking: bool,
    mode: Mode,
    looping: bool,
    /// Bake / import / playback problem, shown under the transport.
    pub(crate) error: Option<String>,
}

impl Open {
    /// What the bake costs, in one line: the readout under the transport
    /// AND the import card's preview, so the two can never disagree
    /// about what is about to be written.
    fn readout(&self) -> String {
        match (&self.baked, self.baking) {
            (Some(_), _) => {
                let bytes = self.budget.region_bytes;
                let region = self.budget.sample_region_bytes;
                let blocks = bytes / BLOCK_BYTES;
                let pct = u64::from(bytes) * 100 / u64::from(region.max(1));
                let baked_secs =
                    blocks as f32 * SAMPLES_PER_BLOCK as f32 / self.rate.max(1) as f32;
                format!(
                    "baked {} Hz · {blocks} blocks · {bytes} B · {pct}% of {} KiB · {baked_secs:.2} s",
                    self.rate,
                    region / 1024
                )
            }
            (None, true) => format!("baking at {} Hz…", self.rate),
            (None, false) => String::new(),
        }
    }
}

/// The viewer's one session-only divider: between the waveform and the
/// chrome under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Divider {
    Waveform,
}

/// A divider mid-drag. `workspace::DraggedDock`'s shape: the drag state
/// rides on the drag itself and the ghost renders nothing, because the
/// visible feedback is the resized layout, not a floating chip.
///
/// The [`EntityId`] is the panel the handle belongs to, and it is load
/// bearing: `on_drag_move` fires in the CAPTURE phase on every mounted
/// listener whose drag type matches, with no hitbox test, so with two
/// audio tabs open in split panes a drag in one would resize both.
#[derive(Clone, Copy)]
struct DraggedDivider(Divider, EntityId);

impl Render for DraggedDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Resolve a waveform-divider drag at window position `position` into the
/// canvas's new height, given the canvas's current bounds.
///
/// Measured DOWN from the canvas's own top, which the header above it
/// pins: the bottom edge is the one following the drag, so it cannot be
/// the edge the height is measured from. No upper clamp -- the tab's
/// column scrolls (`test_c_a_short_tab_scrolls_its_chrome_into_reach`),
/// so a waveform taller than the pane is reachable rather than lost.
///
/// Pure so the floor is testable without a window.
fn divider_size(position: gpui::Point<Pixels>, canvas: Bounds<Pixels>) -> Pixels {
    (position.y - canvas.top()).max(MIN_WAVEFORM_HEIGHT)
}

pub struct AudioPanel {
    focus_handle: FocusHandle,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    pub(crate) root_override: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) state: ViewerState,
    load_generation: u64,
    bake_generation: u64,
    _load_task: Option<Task<()>>,
    _bake_task: Option<Task<()>>,
    /// The budget refresh that follows a bake. Its own field so a rate
    /// change can supersede it without cancelling the bake it belongs to.
    _budget_task: Option<Task<()>>,
    /// Shared with the preview thread; the readout line shows its label.
    status: AudioStatus,
    preview: Option<Preview>,
    _playhead_task: Option<Task<()>>,
    /// The Import target path, editable.
    import_target: Entity<Editor>,
    /// Session-only: the eye in the waveform's title row.
    waveform_visible: bool,
    /// Session-only: the dragged canvas height, `None` until dragged.
    waveform_height: Option<Pixels>,
    /// The canvas's painted bounds, recorded by its own prepaint hook --
    /// what a drag measures its new height from.
    waveform_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// How this panel reaches the daemon: every decode, bake, size and
    /// write goes over that socket. Injectable so a test can script one
    /// instead of needing `ggo serve` running.
    pub(crate) connect: Connect,
}

impl AudioPanel {
    pub fn new(
        workspace: Option<WeakEntity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            root_override: None,
            project_root: None,
            state: ViewerState::Empty,
            load_generation: 0,
            bake_generation: 0,
            _load_task: None,
            _bake_task: None,
            _budget_task: None,
            status: AudioStatus::new(),
            preview: None,
            _playhead_task: None,
            import_target: cx.new(|cx| Editor::single_line(window, cx)),
            waveform_visible: true,
            waveform_height: None,
            waveform_bounds: Rc::new(RefCell::new(None)),
            connect: ggo_daemon_client::system_connect(),
        }
    }

    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        cx.notify();
    }

    /// Load worktree-relative `rel`. Deferred onto a task: the item is
    /// constructed from inside the workspace's own update, and
    /// `refresh_root` reads the workspace back.
    pub fn open_rel_path(&mut self, rel: &str, _window: &mut Window, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &self.state
            && open.rel == rel
        {
            return;
        }
        let rel = rel.to_string();
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.refresh_root(cx);
                // Before the load, not after it: a context-menu Import
                // raises the card the moment the tab opens, and a target
                // that only appeared once the decode landed would leave
                // the field empty under the user's cursor -- or stay
                // empty for good on a file that failed to decode.
                this.seed_import_target(&rel, cx);
                this.load_rel_path(&rel, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn load_rel_path(&mut self, rel: &str, cx: &mut Context<Self>) {
        self.stop_preview(cx);
        self.load_generation += 1;
        let generation = self.load_generation;
        let rel = rel.to_string();
        let Some(root) = self.project_root.clone() else {
            self.state = ViewerState::Error {
                rel,
                message: "no project folder is open".to_string(),
            };
            cx.notify();
            return;
        };
        self.state = ViewerState::Loading(rel.clone());
        cx.notify();
        let path = root.join(&rel);
        let connect = self.connect.clone();
        let loaded = cx.background_spawn(async move { load::load(&connect, &path) });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = loaded.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                match loaded {
                    Ok(loaded) => this.set_loaded(rel, loaded, cx),
                    Err(e) => {
                        this.state = ViewerState::Error {
                            rel,
                            message: format!("{e:#}"),
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_loaded(&mut self, rel: String, loaded: Loaded, cx: &mut Context<Self>) {
        let is_adp = loaded.adp.is_some();
        // A `.adp` carries its own rate; a source opens on the rate
        // emerald's baker would pick, which the daemon reports rather
        // than this panel deciding it a second time.
        let rate = match is_adp {
            true => loaded.probe.rate_hz,
            false => loaded.probe.default_rate_hz,
        };
        // Taken, not cloned: the probe carries the outline across the
        // socket once, and the canvas wants it behind an `Arc` rather
        // than re-cloned on every paint.
        let mut probe = loaded.probe;
        let waveform = Arc::new(std::mem::take(&mut probe.waveform));
        self.state = ViewerState::Ready(Open {
            rel,
            is_adp,
            probe,
            waveform,
            budget: loaded.budget,
            decoded: loaded.decoded,
            rate,
            baked: loaded.adp,
            baking: false,
            // A `.adp` has only one form; a source opens on the form the
            // hardware will play, since that is the question the tab
            // exists to answer.
            mode: Mode::Baked,
            looping: false,
            error: None,
        });
        if !is_adp {
            self.start_bake(cx);
        }
    }

    /// Fill the Import target field for `rel`.
    ///
    /// Through the buffer rather than `Editor::set_text`: this runs from
    /// the open task, which has no window, and the target is plain text
    /// with no selection to preserve.
    ///
    /// The sidecar wins over the computed default: a studio that imports
    /// `jump.wav` into `assets/sfx/` once means it every time, and
    /// retyping the directory on each re-import is how a stray
    /// `assets/jump.adp` ends up shipping beside the real one.
    fn seed_import_target(&mut self, rel: &str, cx: &mut Context<Self>) {
        let remembered = self
            .project_root
            .as_ref()
            .and_then(|root| editor_meta::load(root, rel).import_target)
            .filter(|target| !target.trim().is_empty());
        let target = remembered.unwrap_or_else(|| default_import_target(rel));
        let buffer = self.import_target.read(cx).buffer().read(cx).as_singleton();
        if let Some(buffer) = buffer {
            buffer.update(cx, |buffer, cx| {
                buffer.set_text(target, cx);
            });
        }
    }

    /// A connected client, or why the daemon could not be reached.
    ///
    /// Only the import needs one synchronously -- every other call runs
    /// inside `cx.background_spawn` and connects there.
    fn connect(&self) -> anyhow::Result<std::sync::Arc<ggo_daemon_client::Client>> {
        (self.connect)().map_err(|error| anyhow::anyhow!("the GemdropGo daemon is unavailable: {error:#}"))
    }

    fn open_mut(&mut self) -> Option<&mut Open> {
        match &mut self.state {
            ViewerState::Ready(open) => Some(open),
            _ => None,
        }
    }

    /// Re-bake the open source at its current rate, off-thread; a later
    /// bake (rate change, reload) invalidates this one by generation.
    fn start_bake(&mut self, cx: &mut Context<Self>) {
        self.bake_generation += 1;
        let generation = self.bake_generation;
        let Some(open) = self.open_mut() else {
            return;
        };
        if open.is_adp {
            return;
        }
        open.baking = true;
        open.baked = None;
        let rate = open.rate;
        let rel = open.rel.clone();
        cx.notify();
        let connect = self.connect.clone();
        let root = self.project_root.clone();
        // The bake is the daemon's: it owns the codec emerald packs with,
        // so an editor-baked `.adp` and a pack-baked one cannot drift.
        let bake = cx.background_spawn(async move {
            let root = root.ok_or_else(|| "no project folder is open".to_string())?;
            let client = connect().map_err(|error| format!("{error:#}"))?;
            client
                .audio_bake(&root.join(&rel).to_string_lossy(), rate)
                .map(Arc::new)
                .map_err(|error| format!("{error:#}"))
        });
        self._bake_task = Some(cx.spawn(async move |this, cx| {
            let baked = bake.await;
            this.update(cx, |this, cx| {
                if this.bake_generation != generation {
                    return;
                }
                let mut landed = false;
                if let Some(open) = this.open_mut() {
                    open.baking = false;
                    match baked {
                        Ok(blob) => {
                            open.baked = Some(blob);
                            landed = true;
                        }
                        // A bake that failed has to say so where the user
                        // is looking: the readout would otherwise sit at
                        // "baking…" for ever.
                        Err(error) => open.error = Some(error),
                    }
                }
                // The readout's numbers belong to THIS blob. Without
                // this, a re-bake at another rate would leave the
                // previous rate's block count and percentage on screen.
                if landed {
                    this.refresh_budget(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The budget line's numbers for `blob`, fetched off-thread.
    ///
    /// Asked of the daemon rather than computed here: the region size and
    /// the block arithmetic are the codec's, and a second copy in the
    /// editor is how a readout starts disagreeing with what a cart loads.
    fn refresh_budget(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open_mut() else {
            return;
        };
        let Some(blob) = open.baked.clone() else {
            return;
        };
        let generation = self.bake_generation;
        let connect = self.connect.clone();
        let budget = cx.background_spawn(async move {
            connect()
                .map_err(|error| format!("{error:#}"))?
                .audio_budget(&blob)
                .map_err(|error| format!("{error:#}"))
        });
        self._budget_task = Some(cx.spawn(async move |this, cx| {
            let budget = budget.await;
            this.update(cx, |this, cx| {
                if this.bake_generation != generation {
                    return;
                }
                if let (Some(open), Ok(budget)) = (this.open_mut(), budget) {
                    open.budget = budget;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn set_rate(&mut self, rate: u32, cx: &mut Context<Self>) {
        let Some(open) = self.open_mut() else {
            return;
        };
        if open.is_adp || open.rate == rate {
            return;
        }
        open.rate = rate;
        self.stop_preview(cx);
        self.start_bake(cx);
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let was_playing = self.preview.is_some();
        let Some(open) = self.open_mut() else {
            return;
        };
        if open.mode == mode {
            return;
        }
        open.mode = mode;
        self.stop_preview(cx);
        if was_playing {
            self.play(cx);
        }
        cx.notify();
    }

    fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        let was_playing = self.preview.is_some();
        let Some(open) = self.open_mut() else {
            return;
        };
        open.looping = !open.looping;
        self.stop_preview(cx);
        if was_playing {
            self.play(cx);
        }
        cx.notify();
    }

    fn play_stop(&mut self, cx: &mut Context<Self>) {
        if self.preview.is_some() {
            self.stop_preview(cx);
        } else {
            self.play(cx);
        }
    }

    fn play(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open_mut() else {
            return;
        };
        let spec = match open.mode {
            // `decoded` is `None` only for a `.adp`, which has no Source
            // mode to be in -- but the state is expressible, so it is
            // answered rather than unwrapped.
            Mode::Source => match open.decoded.clone() {
                Some(decoded) => Spec::Source(decoded),
                None => {
                    open.error = Some("this file has no source form".to_string());
                    cx.notify();
                    return;
                }
            },
            Mode::Baked => match &open.baked {
                Some(blob) => Spec::Baked(blob.clone()),
                None => {
                    open.error = Some("still baking — try again in a moment".to_string());
                    cx.notify();
                    return;
                }
            },
        };
        open.error = None;
        let looping = open.looping;
        self.status.reset_for_run();
        self.preview = Some(Preview::start(spec, looping, self.status.clone()));
        // Redraw the playhead until the thread reports done.
        self._playhead_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PLAYHEAD_TICK).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        let done = this.preview.as_ref().is_none_or(|p| p.is_done());
                        if done {
                            this.preview = None;
                        }
                        cx.notify();
                        !done
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn stop_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(preview) = self.preview.take() {
            preview.stop();
        }
        self._playhead_task = None;
        cx.notify();
    }

    // --------------------------------------------------------------- import

    pub(crate) fn import_target(&self, cx: &App) -> String {
        self.import_target.read(cx).text(cx).trim().to_string()
    }

    /// The open file's worktree-relative path, for the card's header.
    fn open_rel(&self) -> Option<String> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.rel.clone()),
            _ => None,
        }
    }

    /// The bake readout the card previews -- see [`Open::readout`].
    fn readout(&self) -> String {
        match &self.state {
            ViewerState::Ready(open) => open.readout(),
            _ => String::new(),
        }
    }

    /// Whether Import has something to write: a source file whose bake
    /// has landed. Drives the button's `disabled`, in both places it is
    /// offered, so the guard is always visible rather than silent.
    fn can_import(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if !open.is_adp && open.baked.is_some())
    }

    /// The bake / import problem the card repeats under its field.
    fn import_error(&self) -> Option<String> {
        match &self.state {
            ViewerState::Ready(open) => open.error.clone(),
            _ => None,
        }
    }

    /// Put [`ImportModal`] over the window.
    ///
    /// **Deferred, and it has to be.** This runs inside the panel's own
    /// update, and the modal layer READS the new modal the instant it is
    /// shown (for the focus handle to focus), which reads this panel --
    /// a read of an entity that is still leased. `window.defer`, not
    /// `cx.defer_in`, which would re-take this entity's update one frame
    /// later for the identical panic.
    pub(crate) fn show_import_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let panel = cx.entity();
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                // `toggle_modal` would CLOSE an open card rather than
                // reopen it, taking the target that was just typed with
                // it. A second click just leaves the one already up.
                if workspace.active_modal::<ImportModal>(cx).is_some() {
                    return;
                }
                workspace.toggle_modal(window, cx, |_, cx| ImportModal::new(panel, cx));
            });
        });
    }

    /// Whether Import would replace an existing file.
    pub(crate) fn import_would_overwrite(&self, cx: &App) -> bool {
        let target = self.import_target(cx);
        self.project_root
            .as_ref()
            .is_some_and(|root| !target.is_empty() && root.join(&target).exists())
    }

    /// Write the baked blob to the import target. The confirm (when the
    /// target exists) is the caller's; this is the half tests exercise.
    pub(crate) fn write_import(&mut self, cx: &mut Context<Self>) -> anyhow::Result<String> {
        let target = self.import_target(cx);
        if target.is_empty() {
            anyhow::bail!("import target is empty");
        }
        if !target.to_ascii_lowercase().ends_with(".adp") {
            anyhow::bail!("import target must end in .adp");
        }
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no project folder is open"))?;
        let blob = match &self.state {
            ViewerState::Ready(open) if open.is_adp => anyhow::bail!("already a .adp"),
            ViewerState::Ready(Open {
                baked: Some(blob), ..
            }) => blob.clone(),
            ViewerState::Ready(_) => anyhow::bail!("still baking — try again in a moment"),
            _ => anyhow::bail!("nothing is open"),
        };
        // The daemon writes it: temp file plus rename, so a crash cannot
        // leave half a `.adp` for `emd pack-ggo` to ship.
        self.connect()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?
            .audio_write(&root.to_string_lossy(), &target, &blob)
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        Ok(target)
    }

    /// Stop showing worktree-relative `rel` because it is no longer on
    /// disk. A no-op for any other file, so a caller may hand this to
    /// every open tab without checking which one it was.
    ///
    /// The preview goes with it: a run already in flight is playing
    /// samples decoded from bytes nothing owns any more.
    pub(crate) fn clear_if_deleted(&mut self, rel: &str, cx: &mut Context<Self>) {
        let showing = match &self.state {
            ViewerState::Ready(open) => open.rel == rel,
            ViewerState::Loading(loading) => loading == rel,
            ViewerState::Error { rel: failed, .. } => failed == rel,
            _ => false,
        };
        if !showing {
            return;
        }
        self.stop_preview(cx);
        // A load still in flight would otherwise land on top of this.
        self.load_generation += 1;
        self.bake_generation += 1;
        self.state = ViewerState::Deleted(rel.to_string());
        cx.notify();
    }

    /// Record where this source's import went, so the next one opens on
    /// the same directory. A sidecar that cannot be written is logged
    /// and dropped: the `.adp` is already on disk and re-typing a path
    /// is not worth failing a successful import over.
    fn remember_import_target(&self, target: &str) {
        let (Some(root), Some(source)) = (self.project_root.as_ref(), self.open_rel()) else {
            return;
        };
        let meta = editor_meta::EditorMeta {
            import_target: Some(target.to_string()),
        };
        if let Err(e) = editor_meta::save(root, &source, &meta) {
            log::error!("GGO: could not remember the import target for {source}: {e}");
        }
    }

    /// The worlds an overwrite of worktree-relative `target` changes the
    /// sound of. Empty for a target outside an emerald asset root, which
    /// no world can name a stem in.
    fn overwrite_cascade(&self, target: &str) -> Vec<String> {
        match self.project_root.as_ref() {
            Some(root) => audio_delete_cascade(root, target),
            None => Vec::new(),
        }
    }

    pub(crate) fn import_impl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.import_target(cx);
        let confirm = if self.import_would_overwrite(cx) {
            ggo_common::confirm_destructive_cascade(
                &format!("Overwrite {target}?"),
                &self.overwrite_cascade(&target),
                "Overwrite",
                false,
                window,
                cx,
            )
        } else {
            Task::ready(true)
        };
        cx.spawn_in(window, async move |this, cx| {
            if !confirm.await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                match this.write_import(cx) {
                    Ok(rel) => {
                        this.remember_import_target(&rel);
                        if let Some(open) = this.open_mut() {
                            open.error = None;
                        }
                        if let Some(workspace) = this.workspace.as_ref().and_then(|w| w.upgrade()) {
                            workspace.update(cx, |workspace, cx| {
                                open_audio_item(workspace, rel, window, cx)
                            });
                        }
                    }
                    Err(e) => {
                        if let Some(open) = this.open_mut() {
                            open.error = Some(format!("import failed: {e:#}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --------------------------------------------------- test-support hooks

    /// A file is decoded and the transport is on screen. `test-support`
    /// only, for `ggo_smoke`'s audio journey -- `state` is crate-private,
    /// so the tab is the only way in from another crate. Read-only.
    #[cfg(feature = "test-support")]
    pub fn test_is_ready(&self) -> bool {
        matches!(self.state, ViewerState::Ready(_))
    }

    /// The bake behind the Baked-mode transport has landed. `PlayStop`
    /// before this point is the documented "still baking" no-op rather
    /// than a preview, so a journey has to wait for it. `test-support`
    /// only.
    #[cfg(feature = "test-support")]
    pub fn test_is_baked(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.baked.is_some())
    }

    /// A preview run is live -- exactly the `self.preview.is_some()` the
    /// transport button reads to decide between Play and Stop. Says
    /// nothing about whether the preview THREAD has reached the end of
    /// the clip: that only becomes visible to the panel when the playhead
    /// task ticks. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_is_playing(&self) -> bool {
        self.preview.is_some()
    }

    /// The loop flag the next (or current) preview runs with.
    /// `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_is_looping(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.looping)
    }

    /// The bake / import / playback problem shown under the transport, or
    /// the load error for a file that never opened. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_error(&self) -> Option<String> {
        match &self.state {
            ViewerState::Error { message, .. } => Some(message.clone()),
            ViewerState::Ready(open) => open.error.clone(),
            _ => None,
        }
    }

    // --------------------------------------------------------------- render

    fn render_message(&self, message: String, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .debug_selector(|| MESSAGE_SELECTOR.to_string())
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }

    fn render_ready(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_ready is only called in the Ready state");
        };
        let playing = self.preview.is_some();
        let progress = self.preview.as_ref().map(|p| p.progress());
        let secs = open.probe.duration_ms as f32 / 1000.0;
        let header = format!(
            "{} Hz · {} ch · {secs:.2} s{}",
            open.probe.rate_hz,
            open.probe.source_channels,
            if open.is_adp { " · baked" } else { "" }
        );

        let readout = open.readout();
        let audio_label = self.status.state().label(playing);

        let weak = cx.weak_entity();
        // The rates on offer are the daemon's -- the one authority on
        // which rates the baker accepts.
        let rates = open.budget.rates.clone();
        let rate_menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for rate in rates {
                let weak = weak.clone();
                menu = menu.entry(
                    SharedString::from(format!("{} kHz", rate / 1000)),
                    None,
                    move |_window, cx| {
                        weak.update(cx, |this, cx| this.set_rate(rate, cx)).ok();
                    },
                );
            }
            menu
        });
        let loop_weak = cx.weak_entity();
        let looping = open.looping;
        let mode = open.mode;
        let is_adp = open.is_adp;
        let can_import = !is_adp && open.baked.is_some();

        let transport = h_flex()
            .debug_selector(|| TRANSPORT_SELECTOR.to_string())
            .flex_wrap()
            // A wrapped row is a different row, not a wider gap between
            // siblings: the vertical gap is bigger than the horizontal
            // one so a two-row transport reads as two rows rather than
            // one tall smear of controls.
            .gap_x_2()
            .gap_y_3()
            .p_1()
            .items_center()
            .child(
                IconButton::new(
                    "ggo-audio-play",
                    if playing {
                        IconName::Stop
                    } else {
                        IconName::PlayFilled
                    },
                )
                .icon_size(IconSize::Small)
                .tooltip(ui::Tooltip::text(if playing {
                    "Stop (space)"
                } else {
                    "Play (space)"
                }))
                .on_click(cx.listener(|this, _, _, cx| this.play_stop(cx))),
            )
            .child(
                Checkbox::new("ggo-audio-loop", ToggleState::from(looping))
                    .label("Loop")
                    .on_click(move |_toggle, _window, cx| {
                        loop_weak.update(cx, |this, cx| this.toggle_loop(cx)).ok();
                    }),
            )
            .child(
                Button::new("ggo-audio-mode-source", "Source")
                    .toggle_state(mode == Mode::Source)
                    .disabled(is_adp)
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::Source, cx))),
            )
            .child(
                Button::new("ggo-audio-mode-baked", "Baked")
                    .toggle_state(mode == Mode::Baked)
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::Baked, cx))),
            )
            .child(if is_adp {
                Label::new(format!("{} Hz", open.rate))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                DropdownMenu::new(
                    "ggo-audio-rate",
                    format!("{} kHz", open.rate / 1000),
                    rate_menu,
                )
                .into_any_element()
            })
            // `min_w_0` so the spacer is the child that gives way in a
            // narrow row: nothing else in here can shrink, and a spacer
            // that refuses to would push Import off the edge.
            .child(div().flex_1().min_w_0())
            .when(!is_adp, |this| {
                this.child(
                    div()
                        .debug_selector(|| IMPORT_SELECTOR.to_string())
                        .child(
                            Button::new("ggo-audio-import", "Import…")
                                .disabled(!can_import)
                                .tooltip(ui::Tooltip::text(if can_import {
                                    "Choose where the .adp lands"
                                } else {
                                    "The bake has not landed yet"
                                }))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.show_import_card(window, cx)
                                })),
                        ),
                )
            });

        v_flex()
            .id("ggo-audio-ready")
            .size_full()
            // Nothing here shrinks usefully: the waveform has a fixed
            // height and the header, transport and readout wrap instead
            // of getting shorter. Past a certain shortness that chrome no
            // longer fits, and the tab scrolls rather than hiding the
            // transport and the error line below the bottom edge.
            .overflow_y_scroll()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .debug_selector(|| HEADER_SELECTOR.to_string())
                    .flex_wrap()
                    .gap_2()
                    .p_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(open.rel.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(header)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(self.render_waveform_section(open.waveform.clone(), progress, cx))
            .child(transport)
            .child(
                h_flex()
                    .gap_2()
                    .px_1()
                    .child(
                        Label::new(readout)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .children(
                        audio_label.map(|label| {
                            Label::new(label).size(LabelSize::Small).color(Color::Muted)
                        }),
                    ),
            )
            .children(open.error.as_ref().map(|e| {
                div().px_1().child(
                    ggo_common::CopyableText::new("ggo-audio-error-copy", e.clone())
                        .size(LabelSize::Small),
                )
            }))
            .into_any_element()
    }

    /// The waveform's height AS RENDERED: the dragged height, else the
    /// default, never below the floor.
    fn rendered_waveform_height(&self) -> Pixels {
        self.waveform_height
            .unwrap_or(px(WAVEFORM_HEIGHT_PX))
            .max(MIN_WAVEFORM_HEIGHT)
    }

    /// Apply one step of a divider drag. Drops out before `notify` when
    /// the clamped height is the one already in force -- a drag emits a
    /// move event per mouse position, most of which land in the same
    /// pixel row once the floor is biting.
    fn drag_divider(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(canvas) = *self.waveform_bounds.borrow() else {
            return;
        };
        let height = divider_size(position, canvas);
        if self.waveform_height.replace(height) != Some(height) {
            cx.notify();
        }
    }

    /// The eye that collapses the waveform to its title row. The
    /// `-on`/`-off` debug selector is what a rendered test toggles and
    /// reads back (an `IconButton`'s id is not a selector).
    fn visibility_toggle(id: &'static str, visible: bool, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .debug_selector(move || format!("{id}-{}", if visible { "on" } else { "off" }))
            .child(
                IconButton::new(id, if visible { IconName::Eye } else { IconName::EyeOff })
                    .icon_size(IconSize::XSmall)
                    .tooltip(ui::Tooltip::text("Show/hide the waveform"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.waveform_visible = !this.waveform_visible;
                        cx.notify();
                    })),
            )
    }

    /// The grab handle under the waveform. `workspace::dock`'s
    /// resize-handle shape: an occluding strip that starts a
    /// [`DraggedDivider`] drag, which the section's `on_drag_move` turns
    /// into a height. Wrapped in `deferred` by the caller for the same
    /// reason the dock does -- the strip straddles a border, and what
    /// paints after it would otherwise swallow half the grab area.
    fn divider_handle(cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id(WAVEFORM_DIVIDER_SELECTOR)
            .debug_selector(|| WAVEFORM_DIVIDER_SELECTOR.to_string())
            .on_drag(
                DraggedDivider(Divider::Waveform, cx.entity_id()),
                |dragged, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| *dragged)
                },
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
            )
            .occlude()
    }

    /// The waveform, under a title row whose eye hides it. Hidden, the
    /// section is that row and nothing else, so everything below moves
    /// up into the space rather than the tab keeping a gap.
    fn render_waveform_section(
        &self,
        waveform: Arc<Vec<(i16, i16)>>,
        progress: Option<f32>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let visible = self.waveform_visible;
        v_flex()
            .relative()
            .flex_none()
            // The drag listener lives on the whole section, not on the
            // handle: a fast drag outruns the 6px strip. `on_drag_move`
            // fires in the CAPTURE phase for the whole window with no
            // hitbox test, so this section sees drags from OTHER audio
            // tabs in other split panes too -- hence the owner filter.
            .on_drag_move(cx.listener(
                |this, event: &gpui::DragMoveEvent<DraggedDivider>, _, cx| {
                    let &DraggedDivider(_, owner) = event.drag(cx);
                    if owner != cx.entity_id() {
                        return;
                    }
                    this.drag_divider(event.event.position, cx);
                },
            ))
            .child(
                h_flex()
                    .debug_selector(|| "ggo-audio-waveform-title".to_string())
                    .flex_none()
                    .px_1()
                    .pt_1()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new("Waveform")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Self::visibility_toggle(
                        "ggo-audio-waveform-visible",
                        visible,
                        cx,
                    )),
            )
            .children(visible.then(|| self.render_waveform(waveform, progress, cx)))
            // With the section hidden there is no boundary to drag.
            .children(visible.then(|| {
                gpui::deferred(
                    Self::divider_handle(cx)
                        .absolute()
                        .bottom(-DIVIDER_SIZE / 2.)
                        .left_0()
                        .w_full()
                        .h(DIVIDER_SIZE)
                        .cursor_row_resize(),
                )
            }))
            .into_any_element()
    }

    fn render_waveform(
        &self,
        waveform: Arc<Vec<(i16, i16)>>,
        progress: Option<f32>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let background = colors.editor_background;
        let wave = colors.text_accent;
        let midline = colors.border;
        let playhead = colors.border_focused;
        let recorded = self.waveform_bounds.clone();
        let canvas = gpui::canvas(
            move |bounds, _window, _cx| {
                *recorded.borrow_mut() = Some(bounds);
            },
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                window.paint_quad(gpui::fill(bounds, background));
                let width: f32 = bounds.size.width.into();
                let height: f32 = bounds.size.height.into();
                let columns = width.max(1.0) as usize;
                let mid_y = bounds.origin.y + px(height / 2.0);
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        point(bounds.origin.x, mid_y),
                        size(bounds.size.width, px(1.)),
                    ),
                    midline,
                ));
                if !waveform.is_empty() {
                    let half = height / 2.0;
                    for x in 0..columns {
                        let i = x * waveform.len() / columns;
                        let (lo, hi) = waveform[i.min(waveform.len() - 1)];
                        let top = mid_y - px(hi as f32 / 32768.0 * half);
                        let bottom = mid_y - px(lo as f32 / 32768.0 * half);
                        let h = (bottom - top).max(px(1.));
                        window.paint_quad(gpui::fill(
                            Bounds::new(
                                point(bounds.origin.x + px(x as f32), top),
                                size(px(1.), h),
                            ),
                            wave,
                        ));
                    }
                }
                if let Some(progress) = progress {
                    let x = bounds.origin.x + px(width * progress.clamp(0.0, 1.0));
                    window.paint_quad(gpui::fill(
                        Bounds::new(point(x, bounds.origin.y), size(px(2.), bounds.size.height)),
                        playhead,
                    ));
                }
            },
        )
        .size_full();
        div()
            .debug_selector(|| WAVEFORM_SELECTOR.to_string())
            .w_full()
            .h(self.rendered_waveform_height())
            // A canvas has no content to set a flex item's automatic
            // minimum, so without this a short pane shrinks the waveform
            // towards zero instead of overflowing the column into its
            // scroll range.
            .flex_none()
            .child(canvas)
            .into_any_element()
    }
}

impl Focusable for AudioPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AudioPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            ViewerState::Empty => self.render_message(
                "Open a .wav, .ogg or .adp from the project panel".to_string(),
                cx,
            ),
            ViewerState::Loading(rel) => self.render_message(format!("decoding {rel}…"), cx),
            ViewerState::Error { rel, message } => {
                self.render_message(format!("{rel}: {message}"), cx)
            }
            ViewerState::Deleted(rel) => {
                self.render_message(format!("{rel} was deleted"), cx)
            }
            ViewerState::Ready(_) => self.render_ready(window, cx),
        };
        div()
            .id("ggo-audio-panel")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &PlayStop, _window, cx| this.play_stop(cx)))
            .on_action(cx.listener(|this, _: &ToggleLoop, _window, cx| this.toggle_loop(cx)))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, WorktreeId};
    use project_panel::ProjectPanel;
    use workspace::item::Item;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    #[test]
    fn claims_exactly_the_audio_extensions() {
        assert!(claims(Path::new("sfx/jump.wav")));
        assert!(claims(Path::new("music/theme.OGG")));
        assert!(claims(Path::new("assets/theme.adp")));
        assert!(!claims(Path::new("assets/hero.png")));
        assert!(!claims(Path::new("notes")));
    }

    #[test]
    fn the_import_target_lands_under_assets_with_the_baked_extension() {
        assert_eq!(
            default_import_target("audio-src/jump.wav"),
            "assets/jump.adp"
        );
        assert_eq!(
            default_import_target("assets/sfx/jump.wav"),
            "assets/sfx/jump.adp"
        );
        assert_eq!(
            default_import_target("assets/theme.ogg"),
            "assets/theme.adp"
        );
        assert_eq!(default_import_target("theme.ogg"), "assets/theme.adp");
    }

    /// A one-second 16 kHz mono PCM16 triangle wave, written as a real
    /// RIFF file under `root/rel`.
    fn write_wav(root: &Path, rel: &str, rate: u32, seconds: u32) {
        let n = (rate * seconds) as usize;
        let period = 40usize;
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let p = i % period;
                let half = period / 2;
                let v = if p < half {
                    (p as i32 * 16_000 / half as i32) - 8000
                } else {
                    8000 - ((p - half) as i32 * 16_000 / half as i32)
                };
                v as i16
            })
            .collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, out).unwrap();
    }

    async fn ready_item<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
        rel: &str,
    ) -> (Entity<AudioItem>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init(cx);
        });
        let root = root.to_path_buf();
        let rel = rel.to_string();
        let (item, cx) =
            cx.add_window_view(|window, cx| AudioItem::new_for_test(rel, root, window, cx));
        cx.run_until_parked();
        (item, cx)
    }

    /// [`ready_item`] inside a real workspace, so the modal layer the
    /// import card lives in has a host. The `FakeFs` tree mirrors the
    /// real temp dir the panel reads through `root_override`.
    async fn workspace_item<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
        rel: &str,
    ) -> (
        Entity<Workspace>,
        Entity<AudioItem>,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init(cx);
            ggo_common::bind_default_keymap(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(root, serde_json::json!({ "emerald.toml": "" }))
            .await;
        let project = Project::test(fs, [root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let root = root.to_path_buf();
        let rel = rel.to_string();
        let item = workspace.update_in(cx, |workspace, window, cx| {
            let weak = workspace.weak_handle();
            let item =
                cx.new(|cx| AudioItem::new_for_test_in(rel, root, Some(weak), window, cx));
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            item
        });
        cx.run_until_parked();
        (workspace, item, cx)
    }

    /// The Import card: the transport's button raises it, the target
    /// field opens focused and seeded, and Enter writes the `.adp` at
    /// whatever was typed there.
    #[gpui::test]
    async fn test_the_import_card_opens_focused_and_enter_writes_the_typed_target(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (workspace, item, cx) = workspace_item(cx, dir.path(), "audio-src/jump.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());

        let button = cx
            .debug_bounds("ggo-audio-import")
            .expect("the transport's Import button");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<ImportModal>(cx)
                    .is_some()),
            "the Import button raises the card"
        );
        let (seeded, focused) = cx.update(|window, cx| {
            let panel = panel.read(cx);
            (
                panel.import_target(cx),
                panel.import_target.focus_handle(cx).is_focused(window),
            )
        });
        assert_eq!(seeded, "assets/jump.adp", "the target field opens seeded");
        assert!(focused, "and focused, ready to be typed over");

        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("assets/typed.adp");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert!(
            dir.path().join("assets/typed.adp").is_file(),
            "Enter imports at the typed target"
        );
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<ImportModal>(cx)
                    .is_none()),
            "and the card closes behind it"
        );
    }

    /// Escape on the card writes nothing.
    #[gpui::test]
    async fn test_escape_on_the_import_card_writes_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (workspace, _item, cx) = workspace_item(cx, dir.path(), "audio-src/jump.wav").await;

        let button = cx
            .debug_bounds("ggo-audio-import")
            .expect("the transport's Import button");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<ImportModal>(cx)
                    .is_some()),
            "the card is up"
        );

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<ImportModal>(cx)
                    .is_none()),
            "Escape dismisses the card"
        );
        assert!(
            !dir.path().join("assets/jump.adp").exists(),
            "and writes nothing"
        );
    }

    /// The overwrite confirm names the worlds that play the stem being
    /// replaced: a re-import is silent about the worlds it changes the
    /// sound of, and those are exactly the files the user cannot see
    /// from the audio tab.
    #[gpui::test]
    async fn test_the_overwrite_confirm_names_the_worlds_that_play_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ggo_common::EMERALD_MANIFEST), "").unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/jump.adp"), b"stale").unwrap();
        std::fs::write(
            dir.path().join("assets/arena.wrld.toml"),
            "[[entity]]\nSfx = { stem = \"jump\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("assets/quiet.wrld.toml"),
            "[[entity]]\nSprite = { stem = \"hero\" }\n",
        )
        .unwrap();

        let (_workspace, item, cx) = workspace_item(cx, dir.path(), "audio-src/jump.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());
        panel.update_in(cx, |panel, window, cx| panel.import_impl(window, cx));
        cx.run_until_parked();

        let (message, detail) = cx.pending_prompt().expect("the overwrite confirm");
        assert_eq!(message, "Overwrite assets/jump.adp?");
        assert!(
            detail.contains("arena plays this audio"),
            "the cascade must name the world that plays the stem: {detail}"
        );
        assert!(
            !detail.contains("quiet"),
            "and only that world: {detail}"
        );

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(
            std::fs::read(dir.path().join("assets/jump.adp")).unwrap(),
            b"stale",
            "Cancel writes nothing"
        );
    }

    fn ready(panel: &AudioPanel) -> &Open {
        match &panel.state {
            ViewerState::Ready(open) => open,
            ViewerState::Error { rel, message } => {
                panic!("expected Ready, got error {rel}: {message}")
            }
            _ => panic!("expected Ready"),
        }
    }

    /// Opening a source file decodes it, bakes it at emerald's default
    /// rate for the container, and prefills the import target; changing
    /// the rate re-bakes with the new rate in the header.
    #[gpui::test]
    async fn test_a_wav_opens_baked_at_the_default_rate_and_rebakes_on_rate_change(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 32_000, 1);
        let (item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());

        panel.read_with(cx, |panel, cx| {
            let open = ready(panel);
            // The daemon's probe is what the tab shows; the local PCM
            // exists only to feed a Source-mode preview.
            assert_eq!(open.probe.rate_hz, 32_000);
            assert_eq!(open.probe.sample_count, 32_000);
            assert!(
                open.decoded.is_some(),
                "a source file keeps PCM for the Source preview"
            );
            assert_eq!(open.rate, 16_000, "wav defaults to the SFX rate");
            assert!(!open.is_adp);
            let blob = open.baked.as_ref().expect("bake landed");
            let (header, _) = ggo_asset_formats::parse_adp(blob).unwrap();
            assert_eq!(header.rate_hz, 16_000);
            assert_eq!(header.block_count, 16_000 / 120 + 1);
            assert_eq!(panel.import_target(cx), "assets/jump.adp");
            assert_eq!(item.read(cx).tab_content_text(0, cx).as_ref(), "jump.wav");
        });

        panel.update(cx, |panel, cx| panel.set_rate(8_000, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            let (header, _) = ggo_asset_formats::parse_adp(open.baked.as_ref().unwrap()).unwrap();
            assert_eq!(header.rate_hz, 8_000);
            assert_eq!(header.block_count, 8_000 / 120 + 1);
        });
    }

    /// Import writes the baked blob to the target, the target then counts
    /// as an overwrite, and the written file opens as a read-only `.adp`
    /// tab whose rate is the header's.
    #[gpui::test]
    async fn test_import_writes_the_adp_and_it_reopens_as_baked(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());

        panel.update(cx, |panel, cx| {
            assert!(!panel.import_would_overwrite(cx));
            let rel = panel.write_import(cx).expect("import writes");
            assert_eq!(rel, "assets/jump.adp");
            assert!(panel.import_would_overwrite(cx), "the target now exists");
        });
        let written = std::fs::read(dir.path().join("assets/jump.adp")).unwrap();
        panel.read_with(cx, |panel, _| {
            assert_eq!(&written, ready(panel).baked.as_ref().unwrap().as_ref());
        });

        let (adp_item, cx) = cx.add_window_view(|window, cx| {
            AudioItem::new_for_test(
                "assets/jump.adp".into(),
                dir.path().to_path_buf(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let adp_panel = adp_item.read_with(cx, |item, _| item.panel().clone());
        adp_panel.update(cx, |panel, cx| {
            let open = ready(panel);
            assert!(open.is_adp);
            assert_eq!(open.rate, 16_000);
            assert!(open.baked.is_some(), "the file is its own bake");
            assert_eq!(open.probe.sample_count, (16_000 / 120 + 1) * 120);
            assert!(
                open.decoded.is_none(),
                "a .adp previews from its blob, so nothing decodes it here"
            );
            let err = panel.write_import(cx).unwrap_err();
            assert!(err.to_string().contains("already a .adp"), "{err}");
        });
    }

    #[gpui::test]
    async fn test_a_missing_file_is_an_error_state_not_a_panic(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (item, cx) = ready_item(cx, dir.path(), "audio-src/nope.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());
        panel.read_with(cx, |panel, _| match &panel.state {
            ViewerState::Error { rel, message } => {
                assert_eq!(rel, "audio-src/nope.wav");
                assert!(message.contains("nope.wav"), "{message}");
            }
            _ => panic!("expected Error state"),
        });
    }

    async fn routed_project(cx: &mut TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({ "sfx": { "jump.wav": "", "theme.ogg": "" }, "notes.txt": "" }),
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

    /// The registered predicate claims audio files (so the project panel
    /// opens no text buffer for them) and adds ONE tab per file; a re-click
    /// activates instead of duplicating; anything else is declined.
    #[gpui::test]
    async fn test_audio_click_opens_one_center_tab_per_file(cx: &mut TestAppContext) {
        let project = routed_project(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);

        for rel in ["sfx/jump.wav", "sfx/jump.wav", "sfx/theme.ogg"] {
            let claimed = workspace.update_in(cx, |workspace, window, cx| {
                workspace.intercept_path_open(&project_path(worktree_id, rel), window, cx)
            });
            assert!(claimed, "{rel} must be claimed");
            cx.run_until_parked();
        }
        let mut items: Vec<_> = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<AudioItem>(cx)
                .map(|item| item.read(cx).rel().to_string())
                .collect()
        });
        items.sort();
        assert_eq!(
            items,
            vec!["sfx/jump.wav", "sfx/theme.ogg"],
            "one tab per file"
        );

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "notes.txt"), window, cx)
        });
        assert!(!claimed, "everything else opens the normal way");
    }

    /// The last import target is remembered per SOURCE file: a second
    /// import of the same `.wav` opens on where the first one went, not
    /// back on the flat `assets/` default.
    #[gpui::test]
    async fn test_the_import_target_is_remembered_per_source(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (_workspace, item, cx) = workspace_item(cx, dir.path(), "audio-src/jump.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.import_target(cx)),
            "assets/jump.adp",
            "a source nothing has been imported from opens on the default"
        );

        panel.update_in(cx, |panel, window, cx| {
            panel
                .import_target
                .update(cx, |editor, cx| editor.set_text("assets/sfx/jump.adp", window, cx));
            panel.import_impl(window, cx);
        });
        cx.run_until_parked();
        assert!(
            dir.path().join("assets/sfx/jump.adp").is_file(),
            "the import lands where it was told to"
        );
        assert_eq!(
            editor_meta::load(dir.path(), "audio-src/jump.wav")
                .import_target
                .as_deref(),
            Some("assets/sfx/jump.adp"),
            "and the sidecar records it"
        );

        panel.update(cx, |panel, cx| {
            panel.seed_import_target("audio-src/jump.wav", cx);
        });
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.import_target(cx)),
            "assets/sfx/jump.adp",
            "the seed prefers the sidecar over the default"
        );
    }

    // ------------------------------------------------- project-panel delete

    /// A real emerald project on the real filesystem behind a real
    /// workspace with a real [`ProjectPanel`]: the `FakeFs` tree mirrors
    /// the temp dir so the worktree has entries to select, while the
    /// interceptor reads the actual world files through `std::fs`.
    async fn delete_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Workspace>,
        Entity<Project>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        std::fs::write(root.join(ggo_common::EMERALD_MANIFEST), "").unwrap();
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/jump.adp"), b"blob").unwrap();
        std::fs::write(root.join("assets/lonely.adp"), b"blob").unwrap();
        std::fs::write(
            root.join("assets/arena.wrld.toml"),
            "[[entity]]\nSfx = { stem = \"jump\" }\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        write_wav(root, "audio-src/jump.wav", 16_000, 1);
        std::fs::write(root.join("audio-src/theme.ogg"), b"").unwrap();

        cx.update(|cx| {
            AppState::test(cx);
            project_panel::init(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            serde_json::json!({
                "emerald.toml": "",
                "assets": {
                    "jump.adp": "",
                    "lonely.adp": "",
                    "arena.wrld.toml": "",
                },
                "notes.txt": "",
                "audio-src": { "jump.wav": "", "theme.ogg": "" },
            }),
        )
        .await;
        let project = Project::test(fs, [root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            let project_panel = ProjectPanel::ggo_test_new(workspace, window, cx);
            workspace.add_panel(project_panel, window, cx);
        });
        let worktree_id = worktree_id(&project, cx);
        cx.run_until_parked();
        (workspace, project, worktree_id, cx)
    }

    /// Select `rel` in the project panel and fire the stock delete action
    /// -- exactly what a user pressing Delete on that row does.
    ///
    /// The selection travels as `project::Event::RevealInProjectPanel`,
    /// and the action is built by name because `project_panel::Delete` is
    /// private to that crate -- which is also proof the dispatch goes
    /// through the real registered handler.
    async fn delete_from_project_panel(
        project: &Entity<Project>,
        worktree_id: WorktreeId,
        rel: &str,
        cx: &mut gpui::VisualTestContext,
    ) {
        // The fake worktree scans lazily, so every ancestor directory of
        // `rel` has to be expanded before the entry exists to be selected.
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

    /// Deleting a `.adp` from the project panel raises THIS crate's
    /// confirm -- the one naming the worlds that play it -- and
    /// confirming unlinks the file.
    #[gpui::test]
    async fn test_deleting_an_adp_names_the_worlds_that_play_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, project, worktree_id, cx) = delete_workspace(cx, dir.path()).await;

        delete_from_project_panel(&project, worktree_id, "assets/jump.adp", cx).await;
        let (message, detail) = cx.pending_prompt().expect("the delete confirm");
        assert_eq!(message, "Delete the audio assets/jump.adp?");
        assert!(
            detail.contains("arena plays this audio"),
            "the cascade must name the world that plays it: {detail}"
        );
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(
            !dir.path().join("assets/jump.adp").exists(),
            "confirming unlinks the file"
        );
    }

    /// A `.adp` nothing plays still gets this crate's confirm (never the
    /// stock one), and Cancel leaves the file alone.
    #[gpui::test]
    async fn test_cancelling_an_adp_delete_leaves_it_alone(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, project, worktree_id, cx) = delete_workspace(cx, dir.path()).await;

        delete_from_project_panel(&project, worktree_id, "assets/lonely.adp", cx).await;
        let (message, detail) = cx.pending_prompt().expect("the delete confirm");
        assert_eq!(message, "Delete the audio assets/lonely.adp?");
        assert!(!detail.contains("plays this audio"), "{detail}");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(
            dir.path().join("assets/lonely.adp").exists(),
            "Cancel leaves the file alone"
        );
    }

    /// A delete that goes through clears the tab showing the file: a
    /// viewer still painting the waveform of a `.adp` that is no longer
    /// on disk is a tab the user can play, re-import and re-bake from
    /// bytes nothing owns any more.
    #[gpui::test]
    async fn test_deleting_an_adp_clears_the_tab_showing_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, project, worktree_id, cx) = delete_workspace(cx, dir.path()).await;
        // The fixture's placeholder bytes would not decode; the tab has
        // to be showing a REAL bake for "stops showing it" to mean
        // anything.
        let decoded = ggo_audio::Decoded {
            samples: vec![1_000; 16_000],
            rate_hz: 16_000,
            source_channels: 1,
        };
        std::fs::write(
            dir.path().join("assets/jump.adp"),
            ggo_audio::bake(&decoded, 16_000),
        )
        .unwrap();

        let root = dir.path().to_path_buf();
        let item = workspace.update_in(cx, |workspace, window, cx| {
            let weak = workspace.weak_handle();
            let item = cx.new(|cx| {
                AudioItem::new_for_test_in(
                    "assets/jump.adp".to_string(),
                    root,
                    Some(weak),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            item
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(WAVEFORM_SELECTOR).is_some(),
            "the tab is showing the file before the delete"
        );

        delete_from_project_panel(&project, worktree_id, "assets/jump.adp", cx).await;
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();

        assert!(
            !dir.path().join("assets/jump.adp").exists(),
            "the file is gone"
        );
        assert!(
            cx.debug_bounds(WAVEFORM_SELECTOR).is_none(),
            "and the tab stops rendering its decoded data"
        );
        assert!(
            cx.debug_bounds(MESSAGE_SELECTOR).is_some(),
            "the tab says so where the viewer was"
        );
        item.read_with(cx, |item, cx| {
            match &item.panel_entity().read(cx).state {
                ViewerState::Deleted(rel) => assert_eq!(rel, "assets/jump.adp"),
                _ => panic!("expected the Deleted state"),
            }
        });
    }

    /// Only a single `.adp` under an asset root is claimed: anything else
    /// falls through to upstream's own delete, and a multi-selection
    /// holding one is claimed WHOLE and explained rather than split.
    #[gpui::test]
    async fn test_the_delete_interceptor_claims_only_a_single_adp(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _project, worktree_id, cx) = delete_workspace(cx, dir.path()).await;

        let intercept = |rels: &[&str], cx: &mut gpui::VisualTestContext| {
            let paths: Vec<ProjectPath> = rels
                .iter()
                .map(|rel| project_path(worktree_id, rel))
                .collect();
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.intercept_delete(&paths, window, cx)
            })
        };

        assert!(
            !intercept(&["notes.txt"], cx),
            "a non-audio file is upstream's"
        );
        assert!(
            !intercept(&["assets/arena.wrld.toml"], cx),
            "and so is a world"
        );
        assert!(
            intercept(&["assets/jump.adp", "assets/lonely.adp"], cx),
            "a multi-selection holding a .adp is claimed whole"
        );
        cx.run_until_parked();
        let (message, detail) = cx.pending_prompt().expect("the explanation");
        assert_eq!(message, "Can't delete these together");
        assert!(
            detail.contains("assets/jump.adp") && detail.contains("assets/lonely.adp"),
            "the explanation names them: {detail}"
        );
        cx.simulate_prompt_answer("OK");
        cx.run_until_parked();
        assert!(
            dir.path().join("assets/jump.adp").exists(),
            "and nothing is unlinked"
        );
    }

    // ------------------------------------------------------ context menu

    /// The entries the project panel offers, by extension: Import on a
    /// source container, Delete on the baked file, nothing anywhere else.
    #[gpui::test]
    async fn test_the_context_menu_offers_import_on_sources_and_delete_on_the_baked(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _project, worktree_id, cx) = delete_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };
        assert_eq!(contributed("audio-src/jump.wav", false, cx), 1, "Import…");
        assert_eq!(contributed("audio-src/theme.ogg", false, cx), 1, "Import…");
        assert_eq!(contributed("assets/jump.adp", false, cx), 1, "Delete Audio");
        assert_eq!(contributed("notes.txt", false, cx), 0, "nothing else");
        assert_eq!(
            contributed("assets/arena.wrld.toml", false, cx),
            0,
            "and not a world"
        );
        assert_eq!(contributed("assets", true, cx), 0, "and not a directory");
    }

    /// The Import entry opens the tab for the clicked source and raises
    /// the card over it, target already filled in.
    #[gpui::test]
    async fn test_the_import_entry_opens_the_tab_and_raises_the_card(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _project, _worktree_id, cx) = delete_workspace(cx, dir.path()).await;

        let handler = import_entry_handler(workspace.downgrade(), "audio-src/jump.wav".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();

        let opened = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<AudioItem>(cx)
                .map(|item| item.read(cx).rel().to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            opened,
            vec!["audio-src/jump.wav".to_string()],
            "the entry opens the clicked source's tab"
        );
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<ImportModal>(cx)
                    .is_some()),
            "and raises the import card over it"
        );
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<AudioItem>(cx)
                .next()
                .expect("the tab")
                .read(cx)
                .panel_entity()
                .clone()
        });
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.import_target(cx)),
            "assets/jump.adp",
            "with the target prefilled, whatever the decode did"
        );
        assert!(
            cx.update(|window, cx| panel
                .read(cx)
                .import_target
                .focus_handle(cx)
                .is_focused(window)),
            "and the field focused, the same as the transport route"
        );
    }

    /// The Delete entry takes the same cascade route the interceptor does.
    #[gpui::test]
    async fn test_the_delete_entry_confirms_with_the_cascade(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _project, _worktree_id, cx) = delete_workspace(cx, dir.path()).await;

        let handler = delete_entry_handler(
            workspace.downgrade(),
            dir.path().to_path_buf(),
            "assets/jump.adp".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        let (message, detail) = cx.pending_prompt().expect("the delete confirm");
        assert_eq!(message, "Delete the audio assets/jump.adp?");
        assert!(detail.contains("arena plays this audio"), "{detail}");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(!dir.path().join("assets/jump.adp").exists());
    }

    // ------------------------------------------------ layout / overflow

    /// Resize the window and let the tab redraw at the new size.
    fn resize(cx: &mut gpui::VisualTestContext, width: f32, height: f32) {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
    }

    fn wheel(cx: &mut gpui::VisualTestContext, at: gpui::Point<Pixels>, dx: f32, dy: f32) {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: at,
            delta: gpui::ScrollDelta::Pixels(point(px(dx), px(dy))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::default(),
        });
        cx.run_until_parked();
    }

    /// The waveform section collapses to its title row, freeing the
    /// space for the chrome below it, and the eye is the way back.
    #[gpui::test]
    async fn test_the_waveform_can_be_hidden_to_its_title(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (_item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;
        resize(cx, 900., 800.);

        assert!(
            cx.debug_bounds("ggo-audio-waveform").is_some(),
            "the waveform starts visible"
        );
        let transport_before = cx
            .debug_bounds(TRANSPORT_SELECTOR)
            .expect("transport bounds recorded at paint");

        let eye = cx
            .debug_bounds("ggo-audio-waveform-visible-on")
            .expect("the waveform section's eye");
        cx.simulate_click(eye.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("ggo-audio-waveform").is_none(),
            "hiding drops the canvas"
        );
        assert!(
            cx.debug_bounds("ggo-audio-divider-waveform").is_none(),
            "and the handle that would resize a hidden section"
        );
        assert!(
            cx.debug_bounds("ggo-audio-waveform-visible-off").is_some(),
            "the title row stays, showing the way back"
        );
        let transport_hidden = cx
            .debug_bounds(TRANSPORT_SELECTOR)
            .expect("transport bounds with the waveform hidden");
        assert!(
            transport_hidden.origin.y < transport_before.origin.y - px(100.),
            "the chrome below takes the freed space: {transport_before:?} -> \
             {transport_hidden:?}"
        );

        let eye = cx
            .debug_bounds("ggo-audio-waveform-visible-off")
            .expect("the waveform eye, crossed out");
        cx.simulate_click(eye.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("ggo-audio-waveform").is_some(),
            "toggling back restores the waveform"
        );
    }

    /// The divider under the waveform sizes it: dragging down makes it
    /// taller by what the pointer moved, and it cannot be dragged below
    /// the floor that keeps an outline readable.
    #[gpui::test]
    async fn test_the_waveform_divider_resizes_it_down_to_a_floor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (_item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;
        resize(cx, 900., 800.);

        let before = cx
            .debug_bounds("ggo-audio-waveform")
            .expect("waveform bounds recorded at paint");
        assert_eq!(
            before.size.height,
            px(WAVEFORM_HEIGHT_PX),
            "the default height is unchanged"
        );
        let handle = cx
            .debug_bounds("ggo-audio-divider-waveform")
            .expect("the waveform divider handle is painted");
        assert!(
            (handle.center().y - before.bottom()).abs() <= DIVIDER_SIZE,
            "the handle must straddle the waveform's bottom edge: {handle:?} \
             under {before:?}"
        );

        let drag_to = |cx: &mut gpui::VisualTestContext, from: gpui::Point<Pixels>, y: Pixels| {
            let target = point(from.x, y);
            cx.simulate_mouse_move(from, None, gpui::Modifiers::default());
            cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::default());
            cx.simulate_mouse_move(target, gpui::MouseButton::Left, gpui::Modifiers::default());
            cx.simulate_mouse_move(target, gpui::MouseButton::Left, gpui::Modifiers::default());
            cx.simulate_mouse_up(target, gpui::MouseButton::Left, gpui::Modifiers::default());
            cx.run_until_parked();
        };

        drag_to(cx, handle.center(), before.bottom() + px(80.));
        let taller = cx
            .debug_bounds("ggo-audio-waveform")
            .expect("waveform bounds after the drag");
        assert!(
            (taller.size.height - before.size.height - px(80.)).abs() < px(2.),
            "dragging the divider 80px down must make the waveform 80px \
             taller: before {:?}, after {:?}",
            before.size,
            taller.size
        );

        let handle = cx
            .debug_bounds("ggo-audio-divider-waveform")
            .expect("the handle follows the edge it drags");
        drag_to(cx, handle.center(), before.origin.y - px(400.));
        let floored = cx
            .debug_bounds("ggo-audio-waveform")
            .expect("waveform bounds after the upward drag");
        assert_eq!(
            floored.size.height, MIN_WAVEFORM_HEIGHT,
            "a drag past the top clamps to the floor, not to nothing"
        );
    }

    /// Class B: the transport is Play, Loop, two mode buttons, the rate
    /// dropdown, the import-target editor and the Import button. In a
    /// narrow tab a single row pushed Import off the edge with no way to
    /// reach it; it must wrap instead.
    #[gpui::test]
    async fn test_b_the_transport_wraps_when_the_tab_is_narrow(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (_item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;

        resize(cx, 1600., 700.);
        let wide = cx
            .debug_bounds(TRANSPORT_SELECTOR)
            .expect("transport bounds recorded at paint");

        resize(cx, 360., 700.);
        let narrow = cx
            .debug_bounds(TRANSPORT_SELECTOR)
            .expect("transport bounds recorded at paint");

        assert!(
            narrow.size.height >= wide.size.height * 2.,
            "a 360px-wide transport must wrap onto at least two rows: \
             one row is {wide:?}, narrow is {narrow:?}"
        );
    }

    /// Class C: nothing in this tab scrolled, so in a short pane the
    /// waveform was squeezed towards nothing and the transport, readout
    /// and error line fell off the bottom unreachable. The waveform keeps
    /// its height and the tab scrolls instead.
    #[gpui::test]
    async fn test_c_a_short_tab_scrolls_its_chrome_into_reach(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (_item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;
        resize(cx, 500., 140.);

        let header = cx
            .debug_bounds(HEADER_SELECTOR)
            .expect("header bounds recorded at paint");
        let before = cx
            .debug_bounds(TRANSPORT_SELECTOR)
            .expect("transport bounds recorded at paint");
        assert!(
            before.origin.y >= px(WAVEFORM_HEIGHT_PX),
            "the waveform must keep its full height rather than being \
             crushed by a short pane: transport at {before:?}"
        );

        wheel(cx, header.center(), 0., -80.);

        let after = cx
            .debug_bounds(TRANSPORT_SELECTOR)
            .expect("transport bounds after the scroll");
        assert!(
            after.origin.y < before.origin.y,
            "a downward wheel must bring the transport up into view: \
             before {before:?}, after {after:?}"
        );
    }
}
