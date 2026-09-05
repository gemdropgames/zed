//! Shared helpers for GGO fork panels: the RGBA->BGRA `RenderImage`
//! bridge (gpui's `RenderImage` frames are BGRA, see `gpui/src/assets.rs`'s
//! "A cached and processed image, in BGRA format"; worldlib composes
//! straight-alpha RGBA, so only a channel swap is needed, no alpha
//! unpremultiply), the unsaved-document close guard shared by the world and
//! sprite panels, the destructive-action confirmation every GGO file op
//! goes through, the file-explorer glue every panel's
//! `PathOpenInterceptor` and `ContextMenuContributor` needs, and the
//! emerald-project discovery + child-process runner the panels that shell
//! out to `emd`/`ggo-diag` share. Deliberately depends on no worldlib, so
//! any GGO panel can use it without pulling in world-doc types -- which is
//! why [`run_capture`] hands back raw captured lines rather than
//! worldlib's `EmdRunOutcome`; the one caller that wants a parsed
//! `emd-json:` trailer does that mapping on its own side
//! (`ggo_emerald_panel::runner`). It DOES depend on `workspace` (the
//! routing helpers below take a `Workspace`); that is not a cycle --
//! `workspace` knows nothing about this crate, it only exposes the
//! `Panel::prepare_to_close`, `PathOpenInterceptor` and
//! `ContextMenuContributor` extension points these helpers plug into.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use smol::process::Command;

use gpui::{
    Action, App, AppContext as _, Context, Entity, PromptLevel, RenderImage, Task, WeakEntity,
    Window,
};
use schemars::JsonSchema;
use serde::Deserialize;

use image::Frame;
use project::ProjectPath;
use workspace::Workspace;
use workspace::dock::Panel;

/// Replay a tileset's recorded import from its (changed) source. Lives
/// here rather than in the import panel so the tileset panel -- which the
/// import panel depends on -- can dispatch it without a crate cycle.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = ggo_common)]
pub struct ReimportTileset {
    /// The `.til`'s worktree-relative path (the import record's sidecar key).
    pub til_rel: String,
}

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

/// Nearest-neighbour fit of an RGBA image into a `size x size` square,
/// letterboxed with transparent pixels -- the project panel's thumbnail.
pub fn thumbnail_rgba(rgba: &[u8], w: usize, h: usize, size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size * size * 4];
    if w == 0 || h == 0 || size == 0 || rgba.len() < w * h * 4 {
        return out;
    }
    let scale = (w.max(h) as f32 / size as f32).max(1.0 / size as f32);
    let (fit_w, fit_h) = (
        ((w as f32 / scale).round() as usize).clamp(1, size),
        ((h as f32 / scale).round() as usize).clamp(1, size),
    );
    let (ox, oy) = ((size - fit_w) / 2, (size - fit_h) / 2);
    for y in 0..fit_h {
        let sy = (y * h / fit_h).min(h - 1);
        for x in 0..fit_w {
            let sx = (x * w / fit_w).min(w - 1);
            let src = (sy * w + sx) * 4;
            let dst = ((oy + y) * size + ox + x) * 4;
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    out
}

/// Bind the default Linux keymap asset -- what the app does at startup and
/// what a panel test needs before it can simulate keystrokes, now that the
/// GGO bindings live in `assets/keymaps/` rather than in each panel's
/// `init`. Bindings whose actions are not linked into the test binary are
/// skipped (`load_asset_allow_partial_failure`).
pub fn bind_default_keymap(cx: &mut App) {
    match settings::KeymapFile::load_asset_allow_partial_failure("keymaps/default-linux.json", cx) {
        Ok(bindings) => cx.bind_keys(bindings),
        Err(e) => log::error!("GGO: default keymap failed to load: {e}"),
    }
}

/// Every panel key context the fork declares in the keymap assets. The
/// test below is the tripwire: delete a block from the JSON and it fails
/// here rather than in whichever panel test happened to use that key.
#[cfg(any(test, feature = "test-support"))]
pub const GGO_KEY_CONTEXTS: &[&str] = &[
    "GgoTilesetPanel",
    "GgoImportPanel",
    "GgoImportPanel && !Editor",
    "GgoSpritePanel",
    "GgoSpritePanel && not_editing",
    "GgoSpritePanel > Editor",
    "GgoWorldPanel",
    "GgoWorldPanel > Editor",
    "GgoEmuPanel",
    "GgoAudioPanel",
    "GgoEmeraldPanel > Editor",
];

// ------------------------------------------------------ shared ~/.ggo paths

const DOT_GGO: &str = ".ggo";
/// `ggo-uartd`'s dump directory under `~/.ggo/`, as that daemon's
/// `control::faults_dir` lays it out -- see [`default_faults_dir`].
const UARTD_DIR: &str = "uartd";
const FAULTS_DIR: &str = "faults";
/// `ggo-diag`'s consolidated per-run logs under `~/.ggo/` -- see
/// [`default_diag_logs_dir`].
const DIAG_DIR: &str = "diag";
const LOGS_DIR: &str = "logs";

/// `ggo-uartd`'s dump directory, `~/.ggo/uartd/faults` -- the layout that
/// daemon's `control::faults_dir` writes, resolved from the same `DOT_GGO`
/// constant [`default_diag_logs_dir`] uses so nothing here can disagree
/// about which directory `~/.ggo` is.
///
/// Read-only from the fork's side: the dumps belong to the daemon, and the
/// panels only import them and point a user at one. Shared by
/// `ggo_charts_panel` (resolves a dump's raw path) and `ggo_reports_panel`
/// (imports the directory on every load) -- both had their own copy of
/// this join, and both take a test override, so a drift between them
/// would never have failed a test.
///
/// `ggo_emu_mcp` deliberately keeps its own copy: it resolves the directory
/// under a caller-supplied `ggo_dir` rather than under `HOME`, and depends
/// on nothing gpui-shaped.
pub fn default_faults_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(DOT_GGO)
            .join(UARTD_DIR)
            .join(FAULTS_DIR),
    )
}

/// `ggo-diag`'s consolidated run logs, `~/.ggo/diag/logs` -- the directory
/// that tool writes one text log per run into, and the file
/// `ggo_charts_panel`'s header hands out for a selected run (see
/// `ggo_emu_remote::diag_log_path`).
///
/// Resolved from the same `DOT_GGO` constant [`default_faults_dir`] uses.
/// Before the PostgreSQL migration the panel derived this from the parent
/// of `~/.ggo/diag.db`; that file is gone, and the directory is not, so it
/// is named here directly rather than inferred from a database path.
pub fn default_diag_logs_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(DOT_GGO)
            .join(DIAG_DIR)
            .join(LOGS_DIR),
    )
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

/// [`destructive_detail`] with a CASCADE list folded in ahead of it: the
/// other things that break if the user says yes, one per line.
///
/// Separate lines rather than a sentence because the list is open-ended (a
/// system can be in every schedule; a component can be in every world) and
/// a prompt that runs the names into prose stops being readable at three
/// of them. Empty `cascade` reduces to exactly [`destructive_detail`], so
/// the two prompts stay one prompt with one detail convention rather than
/// drifting into two.
pub fn cascade_detail(cascade: &[String], unsaved: bool) -> String {
    if cascade.is_empty() {
        return destructive_detail(unsaved);
    }
    format!("{}\n\n{}", cascade.join("\n"), destructive_detail(unsaved))
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
    confirm_destructive_cascade(message, &[], confirm_label, unsaved, window, cx)
}

/// [`confirm_destructive`] for an action with a CASCADE -- the other things
/// that break if it goes ahead (the schedules that reference the system
/// being removed, the worlds that place the component being removed).
///
/// One prompt path, not two: `confirm_destructive` delegates here with an
/// empty list, so a caller can never reach a destructive confirm that
/// forgot the "this cannot be undone" half, and the cascade lands in the
/// prompt's DETAIL rather than its title (see [`cascade_detail`]).
pub fn confirm_destructive_cascade(
    message: &str,
    cascade: &[String],
    confirm_label: &str,
    unsaved: bool,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    let answer = window.prompt(
        PromptLevel::Info,
        message,
        Some(&cascade_detail(cascade, unsaved)),
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

// ------------------------------------------------------- the cart-run hook

/// "Run this project-relative `.cart`", as a registered hook rather than a
/// direct call. Returns `true` when something claimed it.
///
/// It exists for exactly one reason: **the dependency edge only goes one
/// way.** `ggo_emu_panel` already depends on `ggo_charts_panel` (F5.2/S4's
/// "Re-run (perf)" hands the run it just ingested to that panel via
/// `ChartsPanel::open_run`), so `ggo_charts_panel` cannot name `EmuPanel`
/// to send a run back the other way -- that would be a crate cycle. This is
/// the same shape upstream's own fork-local extension points use
/// (`workspace::register_path_open_interceptor` /
/// `register_context_menu_contributor`): a plain `fn` pointer in a global,
/// registered by the provider's `init`, called by name-less consumers.
///
/// The hook takes the run's *rel path*, which is the identity the emu pane
/// already runs carts by (`EmuPanel::rerun`) and the identity the charts
/// panel already has (`ggo_emu_panel::ingest` writes it into `run.label`).
/// No perf-db-name -> stored-`.cart` matching is involved, so ggo-ide's
/// `rerun::matches_stored` (which reaches into that app's own cart library)
/// stays where F5.4 R1 left it.
pub type CartRunner = fn(&mut Workspace, &str, &mut Window, &mut Context<Workspace>) -> bool;

#[derive(Default)]
struct CartRunners(Vec<CartRunner>);

impl gpui::Global for CartRunners {}

/// Register a [`CartRunner`]. Called once by `ggo_emu_panel::init`.
pub fn register_cart_runner(cx: &mut App, runner: CartRunner) {
    cx.default_global::<CartRunners>().0.push(runner);
}

/// Offer `rel` to every registered [`CartRunner`], stopping at the first
/// that claims it. `false` -- always, with an empty registry -- means
/// nothing can run a cart in this app, which is the honest answer for a
/// caller to surface rather than a silent no-op: a charts panel opened in a
/// build without the emulator pane has nowhere to send a Re-run.
pub fn run_cart(
    workspace: &mut Workspace,
    rel: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    // Copied out first, for the same reason `Workspace::intercept_path_open`
    // does it: the runners take `&mut Workspace`, which cannot be held
    // across a borrow of the global.
    let runners = match cx.try_global::<CartRunners>() {
        Some(registry) if !registry.0.is_empty() => registry.0.clone(),
        _ => return false,
    };
    runners.iter().any(|run| run(workspace, rel, window, cx))
}

/// A handler that can flash the open project to an attached board and
/// boot it. Same registry pattern -- and the same reason -- as
/// [`WorldEmulator`]: the flashing lives in `ggo_emu_panel`, which
/// already depends on `ggo_world_panel`, so the world panel's flash
/// button cannot call it directly. The `bool` is `rebuild_gateware`:
/// place-and-route a fresh bitstream instead of flashing the cached one.
///
/// The `Option<&str>` is the world stem to boot (`worlds/arena`), which
/// is the whole point of flashing from the IDE: without it the cart boots
/// the project's `default_world` and the board shows a different world
/// than the one being edited. `None` keeps that default -- the honest
/// answer for a caller that has no world open.
pub type BoardFlasher =
    fn(&mut Workspace, Option<&str>, bool, &mut Window, &mut Context<Workspace>) -> bool;

#[derive(Default)]
struct BoardFlashers(Vec<BoardFlasher>);

impl gpui::Global for BoardFlashers {}

/// Register a [`BoardFlasher`]. Called once by `ggo_emu_panel::init`.
pub fn register_board_flasher(cx: &mut App, flasher: BoardFlasher) {
    cx.default_global::<BoardFlashers>().0.push(flasher);
}

/// Ask the registered flasher to put the open project on the board,
/// booting `world` (a stem like `worlds/arena`) rather than the project's
/// `default_world`. `false` means no emulator pane exists in this build
/// -- reported, not swallowed, exactly as [`run_cart`] explains.
pub fn flash_to_board(
    workspace: &mut Workspace,
    world: Option<&str>,
    rebuild_gateware: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let flashers = match cx.try_global::<BoardFlashers>() {
        Some(registry) if !registry.0.is_empty() => registry.0.clone(),
        _ => return false,
    };
    flashers
        .iter()
        .any(|flash| flash(workspace, world, rebuild_gateware, window, cx))
}

/// A flash button's tooltip: `base`, plus which world the board will boot
/// when one is known.
///
/// Lives here rather than in either panel because BOTH flash surfaces
/// (the world panel's menu and the emulator's toolbar/hardware page) have
/// to say the same thing, and `ggo_world_panel` cannot see
/// `ggo_emu_panel`. Silent when no world is known: the cart then boots
/// whatever `default_world` the project names, which this fork has no
/// business guessing at in a tooltip.
pub fn flash_tooltip(base: &str, world: Option<&str>) -> String {
    match world {
        Some(world) => format!("{base} — boots {world}"),
        None => base.to_string(),
    }
}

/// A handler that can build and boot a world into an emulator pane. Same
/// registry pattern as [`CartRunner`], for the same reason: `ggo_emu_panel`
/// already depends on `ggo_world_panel`, so the world panel's Emulate
/// button cannot call it directly.
pub type WorldEmulator = fn(&mut Workspace, &str, &mut Window, &mut Context<Workspace>) -> bool;

#[derive(Default)]
struct WorldEmulators(Vec<WorldEmulator>);

impl gpui::Global for WorldEmulators {}

/// Register a [`WorldEmulator`]. Called once by `ggo_emu_panel::init`.
pub fn register_world_emulator(cx: &mut App, emulator: WorldEmulator) {
    cx.default_global::<WorldEmulators>().0.push(emulator);
}

/// Offer `rel` (a worktree-relative world file) to every registered
/// [`WorldEmulator`], stopping at the first that claims it. `false` means
/// no emulator pane exists in this build -- see [`run_cart`] on why that
/// is reported rather than swallowed.
pub fn emulate_world(
    workspace: &mut Workspace,
    rel: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let emulators = match cx.try_global::<WorldEmulators>() {
        Some(registry) if !registry.0.is_empty() => registry.0.clone(),
        _ => return false,
    };
    emulators
        .iter()
        .any(|emulate| emulate(workspace, rel, window, cx))
}

// ------------------------------------------------- viewer link (world <-> emulator)

/// Payloads the cart may send between two host polls before the oldest
/// backlog is worth more than the newest: a frame's worth of entity diffs
/// plus statuses is a handful of datagrams; 256 is generous.
pub const LINK_INBOUND_CAPACITY: usize = 256;

/// The transport-neutral rendezvous between a world view and the emulator
/// running its viewer cart. Lives here, not in `ggo_emu_panel`, so the
/// world panel can hold one without depending on the emulator crate --
/// the same reason [`WorldEmulator`] is a registry.
///
/// The two queues are bounded differently on purpose: `outbound` is
/// unbounded because a host edit is lossless -- there is no later message
/// that re-states it -- and the emulator drains the whole queue every
/// frame, so it cannot grow. `inbound` is capped at
/// [`LINK_INBOUND_CAPACITY`] because cart datagrams are re-published
/// whenever they still differ, so a dropped one costs a frame of staleness
/// rather than a lost fact.
///
/// Every `Mutex` here takes the poison rather than unwrapping it: no lock
/// is held across code that can panic, so a poisoned lock means an
/// unrelated thread died while the queue itself stayed consistent, and
/// tearing down the viewer's link over that would be the worse answer.
pub struct LinkEndpoint {
    /// Host -> cart, already ggo-wire framed. Drained by the emulator thread.
    outbound: Mutex<VecDeque<Vec<u8>>>,
    /// Cart -> host CHANNEL_APP payloads, decoded on the emulator thread.
    inbound_tx: async_channel::Sender<Vec<u8>>,
    inbound_rx: async_channel::Receiver<Vec<u8>>,
    /// Latest presented frame: (cart frame number, BGRA image). Written by
    /// the emu panel on the UI thread.
    ///
    /// **The writer owns dropping the image it replaces.** `RenderImage` has
    /// no `Drop` that frees its texture-atlas entry; that only happens on an
    /// explicit `Window::drop_image`. So a writer must take the previous
    /// `Arc` out of this slot and `window.drop_image(previous)` before
    /// storing the new one (the emu panel does this in its per-frame
    /// `on_frame`) -- overwriting at 60 Hz without it leaks an atlas entry
    /// per frame. See `livekit_client`'s `remote_video_track_view.rs` for
    /// the same dance. Readers only clone the `Arc`; they must never drop
    /// the image.
    pub frame: Mutex<Option<(u32, Arc<gpui::RenderImage>)>>,
    tick_tx: async_channel::Sender<()>,
    tick_rx: async_channel::Receiver<()>,
    state: Mutex<ViewerState>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ViewerState {
    Building,
    Running,
    Stopped(String),
}

impl LinkEndpoint {
    pub fn new() -> Arc<Self> {
        let (inbound_tx, inbound_rx) = async_channel::bounded(LINK_INBOUND_CAPACITY);
        let (tick_tx, tick_rx) = async_channel::bounded(1);
        Arc::new(Self {
            outbound: Mutex::new(VecDeque::new()),
            inbound_tx,
            inbound_rx,
            frame: Mutex::new(None),
            tick_tx,
            tick_rx,
            state: Mutex::new(ViewerState::Building),
        })
    }

    /// Host side: queue one already-framed wire message for the cart.
    pub fn send_wire(&self, bytes: Vec<u8>) {
        self.outbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(bytes);
    }

    /// Emulator thread: take everything queued since the last drain.
    pub fn take_outbound(&self) -> Vec<Vec<u8>> {
        self.outbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }

    /// Emulator thread. Full = the host has not polled for a while; the
    /// cart's newest datagram is dropped, exactly what the wire would do.
    /// The cart republishes anything that still differs on its next frame.
    pub fn push_inbound(&self, payload: Vec<u8>) {
        if self.inbound_tx.try_send(payload).is_err() {
            log::debug!("viewer link inbound full; dropping a datagram");
        }
    }

    /// Host side: take every payload the cart has sent since the last poll.
    pub fn try_recv_inbound(&self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.inbound_rx.try_recv().ok()).collect()
    }

    /// Emu panel, once per presented frame. Never blocks.
    pub fn tick(&self) {
        // bounded(1): a pending wake already covers this frame.
        let _ = self.tick_tx.try_send(()); // ponytail: coalescing by design, a full channel is the success case
    }

    /// Host side: the wake stream to await between polls.
    pub fn ticks(&self) -> async_channel::Receiver<()> {
        self.tick_rx.clone()
    }

    pub fn set_state(&self, state: ViewerState) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
        self.tick();
    }

    pub fn state(&self) -> ViewerState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// A handler that can boot the viewer cart for a world into an emulator
/// pane, wired to `endpoint`. Same registry pattern (and same reason) as
/// [`WorldEmulator`].
pub type ViewerBooter =
    fn(&mut Workspace, &str, Arc<LinkEndpoint>, &mut Window, &mut Context<Workspace>) -> bool;

#[derive(Default)]
struct ViewerBooters(Vec<ViewerBooter>);

impl gpui::Global for ViewerBooters {}

/// Register a [`ViewerBooter`]. Called once by `ggo_emu_panel::init`.
pub fn register_viewer_booter(cx: &mut App, booter: ViewerBooter) {
    cx.default_global::<ViewerBooters>().0.push(booter);
}

/// Ask the registered booters to boot the viewer cart for the emerald
/// project containing `world_rel`; returns the endpoint the caller keeps
/// polling, or `None` when no booter claimed it (no emulator pane in this
/// build -- the world view then stays in its design mode).
pub fn boot_viewer(
    workspace: &mut Workspace,
    world_rel: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Arc<LinkEndpoint>> {
    let booters = match cx.try_global::<ViewerBooters>() {
        Some(registry) if !registry.0.is_empty() => registry.0.clone(),
        _ => return None,
    };
    let endpoint = LinkEndpoint::new();
    booters
        .iter()
        .any(|boot| boot(workspace, world_rel, endpoint.clone(), window, cx))
        .then_some(endpoint)
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
/// Reveals + focuses the panel BEFORE running `action`: every entry that
/// comes through here opens a form or a view inside the panel, and with the
/// dock closed (the default) that state would be set invisibly -- to the
/// user, a menu entry that "does nothing". The reveal happens in its own
/// `Workspace` update that finishes before `action` runs, so a panel method
/// that reads the workspace back (`refresh_worlds`, `refresh_root`) is
/// still fine from `action` -- unlike `ggo_emu_panel`'s handler, which runs
/// its action inside the update and has to defer those reads.
///
/// SAFE TO CALL HERE, unlike from a contributor: contributors run while
/// `ProjectPanel` is leased (see `Workspace::context_menu_contributions`)
/// and panic if they touch a panel; handlers run after the lease is
/// released.
/// `rel` (a `/`-separated worktree-relative dir) as the `ProjectPath` the
/// project panel's inline new-entry hook takes. `None` for a rel that is
/// not a valid relative path (absolute, `..`, …).
pub fn inline_project_path(
    worktree_id: project::WorktreeId,
    rel: &str,
) -> Option<project::ProjectPath> {
    Some(project::ProjectPath {
        worktree_id,
        path: path::rel_path::RelPath::from_unix_str(rel).ok()?.into(),
    })
}

pub fn panel_entry_handler<P: Panel>(
    workspace: WeakEntity<Workspace>,
    action: impl Fn(&Entity<P>, &mut Window, &mut App) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        let Some(panel) = workspace.update(cx, |workspace, cx| {
            let panel = workspace.focus_panel::<P>(window, cx)?;
            // A zoomed center item would otherwise hide the dock we just
            // focused.
            workspace.reveal_panel::<P>(window, cx);
            Some(panel)
        }) else {
            return;
        };
        action(&panel, window, cx);
    }
}

// -------------------------------------------------- emerald project roots

/// The file that marks an emerald project root.
pub const EMERALD_MANIFEST: &str = "emerald.toml";

/// The emerald project root for `start`: the nearest ancestor directory
/// (INCLUSIVE of `start` itself) holding [`EMERALD_MANIFEST`], mirroring
/// emerald's own `Project::discover`.
///
/// This is `emd`'s working directory, and it is also the directory
/// `manifests/` and `assets/` live under. Lives here because FIVE panels
/// had grown their own copy of this five-line walk plus its `emerald.toml`
/// constant (`ggo_emerald_panel::runner`, `ggo_world_panel::loader`,
/// `ggo_import_panel`, `ggo_map_panel`, `ggo_sprite_panel`) -- the shape
/// the fork's single-source-of-truth rule exists to prevent.
pub fn emerald_project_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(EMERALD_MANIFEST).is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

// ------------------------------------------------------- child processes

/// Env var naming a non-default `emd` binary.
///
/// Same convention as `.zed/tasks.json`'s `GGO_*` variables (which spawn
/// `emd` by bare name against `PATH`, with every per-user value read from
/// the environment at spawn time) and the same *variable* ggo-ide's
/// `pages::emerald` reads (`EMD_BIN_ENV`). Deliberately NOT ggo-ide's
/// `emd_path` DB setting: that one exists because ggo-ide has a settings
/// page writing `~/.ggo/ggo_ide.db`, and this fork has no such surface --
/// an env var is the whole story here, and it is documented as such.
pub const EMD_BIN_ENV: &str = "GGO_EMD";

/// Bare-name fallback for the `emd` binary, resolved against `PATH` --
/// what `.zed/tasks.json` and ggo-ide both default to.
pub const DEFAULT_EMD_BIN: &str = "emd";

/// `emd`'s machine-readable-output flag. Implies `--quiet`, and is what
/// makes the `emd-json:` trailer (`ggo_worldlib::emerald::EMD_JSON_PREFIX`)
/// appear at all, so every `emd` request built here carries it.
pub const JSON_FLAG: &str = "--json";

/// The `emd` binary to spawn: [`EMD_BIN_ENV`] when set and non-blank, else
/// [`DEFAULT_EMD_BIN`].
pub fn emd_bin() -> String {
    resolve_bin(std::env::var(EMD_BIN_ENV).ok(), DEFAULT_EMD_BIN)
}

/// Env var naming a non-default standalone `ggo-emu` binary -- the same
/// convention as [`EMD_BIN_ENV`], for the same reason (no settings
/// surface in this fork).
pub const GGO_EMU_BIN_ENV: &str = "GGO_EMU";

/// Bare-name fallback for the standalone `ggo-emu` binary, resolved
/// against `PATH`.
pub const DEFAULT_GGO_EMU_BIN: &str = "ggo-emu";

/// The standalone `ggo-emu` binary to spawn: [`GGO_EMU_BIN_ENV`] when set
/// and non-blank, else [`DEFAULT_GGO_EMU_BIN`].
pub fn ggo_emu_bin() -> String {
    resolve_bin(std::env::var(GGO_EMU_BIN_ENV).ok(), DEFAULT_GGO_EMU_BIN)
}

/// A configured binary override, with blank treated as unset (the same
/// filter ggo-ide's `resolve_emd_bin` applies to its stored setting), so an
/// accidentally-empty export doesn't turn into a spawn of `""`. Split out
/// from [`emd_bin`] so it can be tested without mutating a process-global
/// the rest of the suite shares.
fn resolve_bin(configured: Option<String>, default: &str) -> String {
    configured
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// One child-process invocation, fully resolved: which binary, in which
/// working directory, with which argv.
///
/// Built on the UI thread (so the env read and the project-root walk
/// happen where the panel can still report a problem) and moved into a
/// background task, which is why it owns everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRequest {
    pub bin: String,
    /// The child's working directory. For `emd` this is the emerald
    /// project root (`emd` discovers the project from its cwd, so this is
    /// what decides which project is written to); for `ggo-diag` it is the
    /// GGO repo checkout, which that CLI's `detect_repo()` walks up from.
    pub cwd: PathBuf,
    pub args: Vec<String>,
}

impl ProcRequest {
    /// An arbitrary binary's invocation, verbatim.
    pub fn new(bin: impl Into<String>, cwd: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            bin: bin.into(),
            cwd: cwd.into(),
            args,
        }
    }

    /// An `emd` invocation for `args` (as built by
    /// `ggo_worldlib::emerald`'s argv builders) in `cwd`, with
    /// [`JSON_FLAG`] appended.
    ///
    /// The flag is appended HERE rather than in the argv builders because
    /// it is a property of how this host consumes the run (it wants the
    /// trailer), not of the command being run -- the builders are shared
    /// with ggo-ide, whose streaming console wants the same flag for the
    /// same reason but adds it at its own call sites.
    pub fn emd(cwd: impl Into<PathBuf>, args: Vec<String>) -> Self {
        let mut args = args;
        if !args.iter().any(|a| a == JSON_FLAG) {
            args.push(JSON_FLAG.to_string());
        }
        Self::new(emd_bin(), cwd, args)
    }

    /// The invocation as a human would type it -- what a panel shows
    /// while a run is in flight, and what a spawn failure is reported
    /// against.
    pub fn command_line(&self) -> String {
        let mut line = self.bin.clone();
        for arg in &self.args {
            line.push(' ');
            // The one arg that can legitimately be empty is `--module ""`
            // (`mutation_module_args`' "the shared bucket, precisely"),
            // which would otherwise render as a trailing space.
            if arg.is_empty() {
                line.push_str("\"\"");
            } else {
                line.push_str(arg);
            }
        }
        line
    }
}

/// A finished child process: whether it succeeded, and its transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcCapture {
    pub ok: bool,
    /// stdout's lines followed by stderr's -- see [`run_capture`] for why
    /// that order is load-bearing.
    pub lines: Vec<String>,
}

impl ProcCapture {
    /// The transcript as one blob, for a status line or a console.
    pub fn transcript(&self) -> String {
        self.lines.join("\n")
    }
}

/// The injection seam: anything that can turn a [`ProcRequest`] into a
/// [`ProcCapture`].
///
/// A boxed `Fn` rather than a trait because every implementation is a
/// single function -- [`system_proc_runner`] below, and recording fakes in
/// tests -- and `Arc` because a panel holds one and each spawned run needs
/// its own handle. `Send + Sync` because the call happens on
/// `cx.background_spawn`'s thread, never on the UI thread.
pub type ProcRunner = Arc<dyn Fn(ProcRequest) -> ProcCapture + Send + Sync>;

/// The production runner: really spawn the child.
pub fn system_proc_runner() -> ProcRunner {
    Arc::new(|request| run_capture(&request))
}

/// Spawn `request` and wait for it. **Blocking** -- callers run this on
/// `cx.background_spawn`, never on the UI thread.
///
/// The captured transcript is stdout followed by stderr, and that order is
/// load-bearing for `emd`: it prints its `emd-json:` trailer to **stdout on
/// success and stderr on failure** (verified against `emd 0.2.0`), and
/// `ggo_worldlib::emerald::parse_emd_trailer` scans for the LAST trailer
/// line, so stderr-last is what makes a failure's trailer -- the one
/// carrying `error` -- the one that wins.
///
/// A spawn failure (nothing on `PATH`, a bad binary override) is reported
/// as a non-ok capture naming the command, not as a panic or a silent
/// no-op: "it isn't installed" is the single most likely first-run failure
/// and it has to reach the panel as text.
///
/// No timeout of its OWN, deliberately: this function cannot pick one,
/// because the only clock a GGO panel is allowed to race against is
/// `gpui::BackgroundExecutor::timer` (this checkout's `clippy.toml`
/// disallows `smol::Timer::after` outright -- "introduces non-determinism
/// in tests"), and an executor is exactly what a plain synchronous helper
/// does not have. What it provides instead is [`run_capture_async`], whose
/// child dies when the future is dropped -- so a caller that DOES have an
/// executor (`ggo_emerald_panel`'s `cargo check`-backed mutations) can
/// impose its own budget by racing the future against a timer and dropping
/// it, and the child goes with it -- the whole process tree, in fact: see
/// [`run_capture_async`].
///
/// The child is `smol::process::Command`, not `std::process::Command`:
/// this checkout's `clippy.toml` disallows the latter's `output`/`spawn`/
/// `status` outright ("can block the current thread for an unknown
/// duration"). `smol::block_on` around it keeps this function's signature
/// synchronous -- which is what lets [`ProcRunner`] stay a plain `Fn` with
/// a one-line fake -- and blocking is correct here precisely because the
/// only callers are inside `cx.background_spawn`.
pub fn run_capture(request: &ProcRequest) -> ProcCapture {
    smol::block_on(run_capture_async(request))
}

/// A sink for a child's output lines, called as they arrive.
pub type LineSink = Box<dyn FnMut(&str) + Send>;

/// [`ProcRunner`]'s streaming twin: the same `ProcCapture` at the end,
/// plus every line handed over as it lands. Injectable for the same
/// reason the capture runner is -- a test scripts the transcript instead
/// of spawning a board-flashing pipeline.
/// It returns a FUTURE, not a `ProcCapture`, and that is the whole
/// point: a blocking signature has to be driven on some pool thread as
/// one uninterruptible poll, so dropping the caller's task cannot reach
/// the child. Awaiting the future means dropping the task drops the
/// future, which runs `kill_on_drop` -- the cancel button.
pub type ProcStreamer =
    Arc<dyn Fn(ProcRequest, LineSink) -> smol::future::Boxed<ProcCapture> + Send + Sync>;

/// The real streamer: [`run_streaming_async`] on this machine.
pub fn system_proc_streamer() -> ProcStreamer {
    Arc::new(|request, on_line| {
        Box::pin(async move { run_streaming_async(&request, on_line).await })
    })
}

/// [`run_streaming_async`] on the current thread. Tests and other
/// synchronous callers only -- a UI must await the async form, or a
/// cancel cannot reach the child.
pub fn run_streaming(request: &ProcRequest, on_line: LineSink) -> ProcCapture {
    smol::block_on(run_streaming_async(request, on_line))
}

/// Run `request`, calling `on_line` for each stdout/stderr line as it
/// arrives, and return the same capture [`run_capture_async`] would.
///
/// A flash pipeline is minutes of work; captured-only output means a UI
/// that looks hung until it finishes. Both streams are read concurrently
/// (a child that fills one pipe while nobody drains the other deadlocks)
/// and interleaved into one transcript in arrival order -- unlike
/// [`run_capture_async`], whose stdout-then-stderr order is load-bearing
/// for `emd`'s error reporting but useless for live progress.
///
/// **Dropping this future kills the child**, exactly as
/// [`run_capture_async`] does and for the same reason: that is what makes
/// a cancel button possible.
pub async fn run_streaming_async(request: &ProcRequest, on_line: LineSink) -> ProcCapture {
    use std::sync::Mutex;

    let mut command = session_command(request);
    let child = command
        // Null, not inherit: a `setsid` child has no controlling terminal,
        // so a prompt on an inherited stdin (an https `git pull` asking
        // for credentials) would not stop on SIGTTIN -- it would silently
        // compete with the user's shell for keystrokes and hang the run.
        // EOF makes it fail fast instead. On the ASYNC command: stdio set
        // on the std one is dropped by the `From` conversion.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let line = format!("running `{}`: {e}", request.command_line());
            let mut on_line = on_line;
            on_line(&line);
            return ProcCapture {
                ok: false,
                lines: vec![line],
            };
        }
    };
    // Declared after `child` on purpose: locals drop in reverse order, so
    // on cancellation the guard signals the group while every member --
    // the leader included -- is still alive.
    let group = ProcessGroupGuard::new(child.id());

    // `on_line` and the transcript are shared by the two drain futures
    // below. They interleave within this one future and no lock is held
    // across an await, so this is never contended -- it is a `Mutex`
    // rather than a `RefCell` only so the whole future stays `Send`,
    // which is what lets a caller await it wherever it likes.
    let shared = Mutex::new((on_line, Vec::<String>::new()));
    let take = |line: Result<String, std::io::Error>| {
        // A child's output is not guaranteed UTF-8 and a read can fail
        // mid-stream; a lossy transcript beats none (`capture_lines`
        // makes the same call).
        let line = match line {
            Ok(line) => line,
            Err(e) => format!("reading output: {e}"),
        };
        let mut shared = match shared.lock() {
            Ok(shared) => shared,
            // A panicking sink poisons the lock; the transcript is best
            // effort, so drop the line rather than propagate a panic.
            Err(_) => return,
        };
        (shared.0)(&line);
        shared.1.push(line);
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // BOTH pipes must be drained concurrently -- a child that fills one
    // while nobody reads the other blocks forever -- and the merge has to
    // END when both do. `StreamExt::race` does NOT: with both streams at
    // EOF neither of its arms matches and it falls through to
    // `Poll::Pending` for good. `zip` of two self-terminating drains is
    // the combinator that actually finishes.
    let drain_out = drain_split_lines(stdout, &take);
    let drain_err = drain_split_lines(stderr, &take);
    smol::future::zip(drain_out, drain_err).await;

    let (mut on_line, mut lines) = match shared.into_inner() {
        Ok(shared) => shared,
        Err(poisoned) => poisoned.into_inner(),
    };
    let ok = match child.status().await {
        Ok(status) => {
            // Only a reaped child disarms: after this the group id is free
            // for reuse and must never be signalled. A failed wait leaves
            // the guard armed -- the tree may still be up, and killing it
            // is the whole point.
            group.disarm();
            status.success()
        }
        Err(e) => {
            let line = format!("waiting on `{}`: {e}", request.command_line());
            on_line(&line);
            lines.push(line);
            false
        }
    };
    ProcCapture { ok, lines }
}

/// Drain `reader`, handing every line to `take` as it lands. Lines are
/// terminated by `\n` OR `\r`: `git --progress` and friends redraw a
/// progress line in place with bare carriage returns and only ever end
/// it with a newline, so a `\n`-only split would sit silent through an
/// entire clone -- the exact minutes a live console exists for. Blank
/// lines are dropped (each `\r\n` would otherwise produce one).
async fn drain_split_lines<R: smol::io::AsyncRead + Unpin>(
    reader: Option<R>,
    take: &impl Fn(Result<String, std::io::Error>),
) {
    use smol::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return;
    };
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                for &byte in &chunk[..read] {
                    if byte == b'\n' || byte == b'\r' {
                        if !pending.is_empty() {
                            take(Ok(String::from_utf8_lossy(&pending).into_owned()));
                            pending.clear();
                        }
                    } else {
                        pending.push(byte);
                    }
                }
            }
            Err(e) => {
                take(Err(e));
                break;
            }
        }
    }
    // A child that dies mid-line still gets its last words recorded.
    if !pending.is_empty() {
        take(Ok(String::from_utf8_lossy(&pending).into_owned()));
    }
}

/// The runner's command, with the child put in its own session so the
/// whole tree is one process group. `kill_on_drop` reaches only the
/// direct child, and the children this runs -- ggo-diag above all --
/// spawn their own (sbt, nextpnr, fujprog): cancelling a flash by
/// killing just ggo-diag orphans a place-and-route that keeps running
/// for twenty more minutes, or a fujprog mid-write. `pre_exec(setsid)`
/// runs in the child before exec, so unlike a parent-side `setpgid`
/// there is no race to lose -- the same pattern as
/// `util::set_pre_exec_to_start_new_session`.
fn session_command(request: &ProcRequest) -> Command {
    let mut command = std::process::Command::new(&request.bin);
    command.args(&request.args).current_dir(&request.cwd);
    // safety: setsid is on the async-signal-safe list. Its failure is
    // load-bearing -- the guard's whole premise is pid == pgid -- so a
    // child that could not move into its own session must not run at all.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Command::from(command)
}

/// TERMs a child's whole process group on drop, unless disarmed -- how a
/// dropped runner future takes the child's OWN children down with it.
/// Only meaningful for a child spawned via [`session_command`], whose
/// pid IS its pgid. SIGTERM rather than SIGKILL so sbt and friends get
/// to die cleanly; the leader still gets `kill_on_drop`'s SIGKILL as a
/// backstop. The limit: a grandchild that IGNORES SIGTERM survives --
/// `Drop` cannot sleep and escalate -- so the promise holds for
/// well-behaved children (sbt, nextpnr, fujprog all are), not for
/// arbitrary ones.
struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl ProcessGroupGuard {
    #[cfg(unix)]
    fn new(pid: u32) -> Self {
        Self {
            pgid: Some(pid as i32),
        }
    }

    #[cfg(not(unix))]
    fn new(_pid: u32) -> Self {
        Self {}
    }

    /// The child exited and reaped its own children; from here the group
    /// id may be reused by an unrelated process, so the drop must not
    /// signal it.
    fn disarm(#[cfg_attr(not(unix), allow(unused_mut))] mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            if unsafe { libc::killpg(pgid, libc::SIGTERM) } == -1 {
                let error = std::io::Error::last_os_error();
                // ESRCH is the group having already exited -- normal. Any
                // other failure means the cancel did NOT happen, which is
                // the orphaned-nextpnr bug back with no diagnostics.
                if error.raw_os_error() != Some(libc::ESRCH) {
                    log::warn!("cancelling process group {pgid}: {error}");
                }
            }
        }
    }
}

/// [`run_capture`]'s async twin, and the only version that can be given a
/// deadline: **dropping this future kills the child's whole process
/// tree.** The child is spawned into its own session ([`session_command`])
/// and a dropped future signals that group -- so an `emd` that shelled out
/// to `cargo check` takes the `cargo` down with it instead of leaving it
/// reparented, compiling, and holding the project's `target/` lock.
/// `kill_on_drop` still SIGKILLs the leader as a backstop.
///
/// Everything else -- the stdout-then-stderr capture order that lets a
/// failing `emd`'s stderr trailer win, and the "a binary that cannot be
/// spawned is a non-ok capture naming the command" rule -- is
/// [`run_capture`]'s contract, unchanged; this is where both are actually
/// implemented.
pub async fn run_capture_async(request: &ProcRequest) -> ProcCapture {
    let mut command = session_command(request);
    let child = command
        // Null for the same reason as `run_streaming_async`: an inherited
        // stdin in a terminal-less session hangs on the first prompt.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let output = match child {
        Ok(child) => {
            let pid = child.id();
            // `output()` takes the child by value, so the guard has to be
            // created after the future that owns it: locals drop in
            // reverse order, and the guard must fire while the leader is
            // still alive -- a reaped leader's group id is up for reuse.
            let running = child.output();
            let group = ProcessGroupGuard::new(pid);
            let output = running.await;
            if output.is_ok() {
                group.disarm();
            }
            output
        }
        Err(e) => Err(e),
    };
    let output = match output {
        Ok(output) => output,
        Err(e) => {
            return ProcCapture {
                ok: false,
                lines: vec![format!("running `{}`: {e}", request.command_line())],
            };
        }
    };
    let mut lines = capture_lines(&output.stdout);
    lines.extend(capture_lines(&output.stderr));
    ProcCapture {
        ok: output.status.success(),
        lines,
    }
}

/// Captured bytes as lines, lossily decoded (a child's output is not
/// guaranteed UTF-8 and a mojibake transcript beats no transcript).
fn capture_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

/// The last line of a failed capture that `skip` does not recognise as
/// progress.
///
/// [`failure_reason`] takes the last non-blank line, which is right for
/// `run_capture` (stderr is appended AFTER stdout, so the error is last)
/// but wrong for a streamed run: the transcript is in arrival order and
/// a CLI's last word is usually its verdict banner, not what went wrong.
pub fn failure_line(capture: &ProcCapture, skip: impl Fn(&str) -> bool) -> String {
    capture
        .lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty() && !skip(line))
        .cloned()
        .unwrap_or_else(|| failure_reason(capture))
}

/// The last non-blank line of a failed capture -- with `--json` implying
/// `--quiet`, `emd`'s output on failure is the error report, and the last
/// line is the actual message (earlier ones are cargo's progress noise).
pub fn failure_reason(capture: &ProcCapture) -> String {
    capture
        .lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "no output".to_string())
}

/// Spawn `request` detached: no wait, no capture, and the child outlives
/// the caller -- for launching an external app (the standalone emulator)
/// rather than running a tool to completion. No `kill_on_drop`: dropping
/// the [`smol::process::Child`] leaves the process running, which is the
/// point. A spawn failure is reported as text naming the command, same as
/// [`run_capture`]'s rule.
pub fn spawn_detached(request: &ProcRequest) -> Result<(), String> {
    Command::new(&request.bin)
        .args(&request.args)
        .current_dir(&request.cwd)
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("running `{}`: {e}", request.command_line()))
}

/// The injection seam for [`spawn_detached`], shaped like [`ProcRunner`]
/// and existing for the same reason: a panel holds one and tests substitute
/// a recorder.
pub type DetachedLauncher = Arc<dyn Fn(ProcRequest) -> Result<(), String> + Send + Sync>;

/// The production launcher: really spawn the child, detached.
pub fn system_detached_launcher() -> DetachedLauncher {
    Arc::new(|request| spawn_detached(&request))
}

// ------------------------------------------------- world -> cartridge

/// Where a world's built cartridge is written, relative to the emerald
/// project root. Under `target/` because that is the directory every
/// emerald project already gitignores (`emd new`'s scaffold) and the one
/// `emd pack-ggo` itself stages its intermediate card into
/// (`target/emd-ggo-card`), so a build here adds nothing new to ignore.
pub const PACK_OUT_DIR: &str = "target/ggo-emulate";

/// `emd pack-ggo`'s argv for booting `world_stem`, writing to `out`.
///
/// This is the fork's port of ggo-ide's world-Emulate invocation
/// (`pages/world` -> `Message::EmulateWorld` -> `EmuMsg::BuildAndRunWorld`
/// -> `backend::emubuild::build_full_system_with_world`), reduced to the
/// half this fork's emulator can consume. Two deliberate differences from
/// that function, both forced by what `ggo_emu_panel` actually is:
///
/// - **`--world <stem>` instead of `EMERALD_DEFAULT_WORLD=<stem>` on the
///   child's environment.** `emd` gained the flag as documented sugar for
///   exactly that env var (`emd build --help`: "Sets
///   `EMERALD_DEFAULT_WORLD` on the cargo build"), and a flag is
///   inspectable in a [`ProcRequest`] where an environment is not -- which
///   is what lets the tests assert the boot world without spawning
///   anything.
/// - **A cartridge, not a full system image.** ggo-ide builds GemOS +
///   a FAT card and boots the whole SoC; the in-pane emulator is
///   `ggo-emu`'s CART mode, so the artifact it can run is a cartridge.
///   `pack-ggo` (not `pack`) because a world needs its ASSETS: the `.ggo`
///   carries the compiled GGO2 asset section, a bare `.cart` does not.
pub fn world_pack_args(out: &Path, world_stem: &str) -> Vec<String> {
    vec![
        "pack-ggo".to_string(),
        "--out".to_string(),
        out.to_string_lossy().into_owned(),
        "--world".to_string(),
        world_stem.to_string(),
    ]
}

/// The file name a world's cartridge is built under: the world's full
/// assets-relative stem with `/` flattened to `-`, so `worlds/main` and
/// `worlds/boss/main` cannot collide in one output directory.
pub fn pack_out_name(world_stem: &str) -> String {
    format!("{}.ggo", world_stem.replace('/', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flash button says which world the board will boot -- the answer
    /// a flash cannot show until it has already booted the wrong one.
    #[test]
    fn a_flash_tooltip_names_the_world_when_there_is_one() {
        assert_eq!(
            flash_tooltip("Flash this project to the board", Some("worlds/arena")),
            "Flash this project to the board — boots worlds/arena"
        );
        assert_eq!(
            flash_tooltip("Flash this project to the board", None),
            "Flash this project to the board",
            "with no world named, the project's default_world stands and \
             the tooltip does not guess at it"
        );
    }

    /// The streaming runner hands every line over as it arrives AND
    /// returns the same capture the blocking one would.
    #[test]
    fn run_streaming_reports_lines_then_the_capture() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = seen.clone();
            Box::new(move |line: &str| seen.lock().unwrap().push(line.to_string())) as LineSink
        };
        let request = ProcRequest::new(
            "sh",
            std::env::temp_dir(),
            vec![
                "-c".to_string(),
                "echo one; echo two >&2; echo three".to_string(),
            ],
        );
        let capture = run_streaming(&request, sink);
        assert!(capture.ok, "the script exits 0");
        let streamed = seen.lock().unwrap().clone();
        assert_eq!(streamed.len(), 3, "every line was streamed: {streamed:?}");
        assert!(streamed.contains(&"one".to_string()));
        assert!(streamed.contains(&"two".to_string()), "stderr streams too");
        let mut sorted = capture.lines;
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["one", "three", "two"],
            "and lands in the capture"
        );
    }

    /// Progress redraws -- `git --progress` style bare `\r` updates --
    /// stream as they land instead of sitting silent until the final
    /// newline, and `\r\n` produces no phantom blank line.
    #[test]
    fn run_streaming_splits_on_carriage_returns() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = seen.clone();
            Box::new(move |line: &str| seen.lock().unwrap().push(line.to_string())) as LineSink
        };
        let request = ProcRequest::new(
            "sh",
            std::env::temp_dir(),
            vec![
                "-c".to_string(),
                r"printf 'a 1%%\ra 2%%\r\na done\n'".to_string(),
            ],
        );
        let capture = run_streaming(&request, sink);
        assert!(capture.ok);
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec!["a 1%", "a 2%", "a done"],
            "each redraw is its own line, with no blank between \\r and \\n"
        );
    }

    /// Dropping the run future takes the child's OWN children down too.
    /// `kill_on_drop` only signals the direct child; the process-group
    /// guard is what reaches the grandchildren -- without it, cancelling
    /// a flash leaves nextpnr place-and-routing for twenty more minutes.
    #[cfg(unix)]
    #[test]
    fn dropping_the_stream_kills_the_childs_own_children() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let (tx, rx) = mpsc::channel::<String>();
        // The child prints its background child's pid, then waits on it
        // forever -- a stand-in for ggo-diag sitting on nextpnr.
        let request = ProcRequest::new(
            "sh",
            std::env::temp_dir(),
            vec!["-c".to_string(), "sleep 600 & echo $!; wait".to_string()],
        );
        let task = smol::spawn(async move {
            run_streaming_async(
                &request,
                Box::new(move |line: &str| {
                    tx.send(line.to_string()).ok();
                }),
            )
            .await
        });
        let grandchild: i32 = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the child announces its grandchild")
            .trim()
            .parse()
            .expect("a pid");
        assert_eq!(
            unsafe { libc::kill(grandchild, 0) },
            0,
            "the grandchild is alive while the run is"
        );
        drop(task); // the cancel button
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let gone = unsafe { libc::kill(grandchild, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the grandchild outlived the cancel"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A non-zero exit is a failed capture, and a binary that cannot be
    /// spawned reports rather than panicking.
    #[test]
    fn run_streaming_surfaces_failures() {
        let request = ProcRequest::new(
            "sh",
            std::env::temp_dir(),
            vec!["-c".to_string(), "echo nope >&2; exit 3".to_string()],
        );
        let capture = run_streaming(&request, Box::new(|_| {}));
        assert!(!capture.ok, "exit 3 is a failure");
        assert_eq!(failure_reason(&capture), "nope");

        let missing = ProcRequest::new("ggo-not-a-real-binary", std::env::temp_dir(), Vec::new());
        let capture = run_streaming(&missing, Box::new(|_| {}));
        assert!(!capture.ok);
        assert!(
            capture.lines[0].contains("ggo-not-a-real-binary"),
            "the failure names the binary: {:?}",
            capture.lines
        );
    }

    /// The keymap tripwire: every GGO context block must exist in each
    /// platform asset (and the `> Editor` ones in `vim.json` too, or vim's
    /// same-depth tab/enter bindings win the tie).
    #[test]
    fn every_ggo_key_context_is_declared_in_the_keymap_assets() {
        for asset in [
            "assets/keymaps/default-linux.json",
            "assets/keymaps/default-macos.json",
            "assets/keymaps/default-windows.json",
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../..")
                .join(asset);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for context in GGO_KEY_CONTEXTS {
                assert!(
                    text.contains(&format!("\"context\": \"{context}\"")),
                    "{asset} is missing the {context} block"
                );
            }
        }
        let vim = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../..")
                .join("assets/keymaps/vim.json"),
        )
        .expect("vim.json");
        for context in GGO_KEY_CONTEXTS.iter().filter(|c| c.contains("> Editor")) {
            assert!(
                vim.contains(&format!("\"context\": \"{context}\"")),
                "vim.json must re-declare {context} or VimControl wins the tie"
            );
        }
    }

    #[test]
    fn thumbnail_rgba_fits_and_letterboxes() {
        // 4x2 image: left half red, right half blue.
        let mut src = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                src.extend_from_slice(if x < 2 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        let thumb = thumbnail_rgba(&src, 4, 2, 4);
        let px = |x: usize, y: usize| &thumb[(y * 4 + x) * 4..][..4];
        assert_eq!(px(0, 0), [0, 0, 0, 0], "letterbox row is transparent");
        assert_eq!(px(0, 1), [255, 0, 0, 255]);
        assert_eq!(px(3, 2), [0, 0, 255, 255]);
        assert_eq!(px(0, 3), [0, 0, 0, 0]);
        assert_eq!(
            thumbnail_rgba(&[], 0, 0, 2).len(),
            16,
            "degenerate input is a blank square"
        );
    }

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

    /// An empty cascade must reduce to EXACTLY `destructive_detail` -- that
    /// equivalence is what lets `confirm_destructive` delegate to
    /// `confirm_destructive_cascade` instead of being a second prompt path
    /// that could drift.
    #[test]
    fn an_empty_cascade_is_just_the_destructive_detail() {
        assert_eq!(cascade_detail(&[], false), destructive_detail(false));
        assert_eq!(cascade_detail(&[], true), destructive_detail(true));
    }

    /// A cascade goes ABOVE the "cannot be undone" line, one entry per
    /// line, and never replaces it.
    #[test]
    fn a_cascade_is_listed_above_the_undone_warning() {
        let detail = cascade_detail(
            &[
                "Used by 2 schedules: tick, render.".to_string(),
                "Placed in 1 world: worlds/arena.toml.".to_string(),
            ],
            false,
        );
        assert_eq!(
            detail,
            "Used by 2 schedules: tick, render.\n\
             Placed in 1 world: worlds/arena.toml.\n\n\
             This cannot be undone."
        );
        assert!(cascade_detail(&["a".to_string()], true).ends_with(&destructive_detail(true)));
    }

    /// `default_faults_dir` must land on `~/.ggo/uartd/faults` -- the
    /// directory `ggo-uartd` writes and the two panels import from. Both
    /// resolve it through this one function, so a test that pins the
    /// layout here pins it for both.
    #[test]
    fn default_faults_dir_is_dot_ggo_uartd_faults() {
        let path = default_faults_dir().expect("HOME resolves in the test env");
        assert!(path.ends_with(".ggo/uartd/faults"), "{}", path.display());
    }

    /// `default_diag_logs_dir` must land on `~/.ggo/diag/logs` -- the
    /// directory `ggo-diag` writes its per-run logs into, and the one the
    /// charts panel's Copy-log-path button reads.
    #[test]
    fn default_diag_logs_dir_is_dot_ggo_diag_logs() {
        let path = default_diag_logs_dir().expect("HOME resolves in the test env");
        assert!(path.ends_with(".ggo/diag/logs"), "{}", path.display());
        // Both live under the SAME `~/.ggo`, which is the whole reason
        // `DOT_GGO` is a single constant.
        assert_eq!(
            path.parent().and_then(|p| p.parent()),
            default_faults_dir()
                .expect("HOME resolves")
                .parent()
                .and_then(|p| p.parent()),
            "the logs and the dumps live under one ~/.ggo"
        );
    }

    #[test]
    fn dirty_message_names_the_document() {
        assert_eq!(
            dirty_message("worlds/test.toml"),
            "worlds/test.toml contains unsaved edits. Do you want to save it?"
        );
    }

    // ------------------------------------------- unsaved-document guard

    use gpui::TestAppContext;

    /// The minimal panel stand-in [`prepare_to_close_dirty`] needs: just
    /// somewhere for the `save` callback to record that it ran.
    struct GuardedDoc {
        save_calls: usize,
    }

    /// Start the guard on a fresh entity in `cx`'s window, with the `save`
    /// callback counting its calls and returning `save_result`.
    fn start_close(
        cx: &mut gpui::VisualTestContext,
        dirty_name: Option<&str>,
        save_result: bool,
    ) -> (gpui::Entity<GuardedDoc>, Task<bool>) {
        let doc = cx.update(|_, cx| cx.new(|_| GuardedDoc { save_calls: 0 }));
        let close = cx.update(|window, cx| {
            doc.update(cx, |_, cx| {
                prepare_to_close_dirty(
                    dirty_name.map(str::to_string),
                    window,
                    cx,
                    move |doc, _cx| {
                        doc.save_calls += 1;
                        save_result
                    },
                )
            })
        });
        (doc, close)
    }

    /// A clean panel must be invisible to the close flow: no prompt, ready
    /// `true`, and the save callback never runs.
    #[gpui::test]
    async fn test_close_guard_lets_a_clean_document_close_without_prompting(
        cx: &mut TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let (doc, close) = start_close(cx, None, true);
        assert!(!cx.has_pending_prompt(), "a clean document must not prompt");
        assert!(close.await, "a clean document must not block the close");
        doc.read_with(cx, |doc, _| assert_eq!(doc.save_calls, 0));
    }

    /// Cancel vetoes the close and must not write anything.
    #[gpui::test]
    async fn test_close_guard_cancel_vetoes_the_close(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let (doc, close) = start_close(cx, Some("worlds/test.toml"), true);
        assert_eq!(
            cx.pending_prompt().map(|(message, _)| message),
            Some(dirty_message("worlds/test.toml")),
        );
        cx.simulate_prompt_answer("Cancel");
        assert!(!close.await, "Cancel must veto the close");
        doc.read_with(cx, |doc, _| {
            assert_eq!(doc.save_calls, 0, "Cancel must not save")
        });
    }

    /// "Don't Save" allows the close WITHOUT running the save callback.
    #[gpui::test]
    async fn test_close_guard_discard_closes_without_saving(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let (doc, close) = start_close(cx, Some("worlds/test.toml"), true);
        cx.simulate_prompt_answer("Don't Save");
        assert!(close.await, "Don't Save must allow the close");
        doc.read_with(cx, |doc, _| {
            assert_eq!(doc.save_calls, 0, "Don't Save must not write")
        });
    }

    /// Save runs the callback and closes when the write succeeds.
    #[gpui::test]
    async fn test_close_guard_save_success_allows_the_close(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let (doc, close) = start_close(cx, Some("worlds/test.toml"), true);
        cx.simulate_prompt_answer("Save");
        assert!(close.await, "a successful save must allow the close");
        doc.read_with(cx, |doc, _| assert_eq!(doc.save_calls, 1));
    }

    /// The data-loss case: Save whose write FAILS must cancel the close --
    /// otherwise the window goes away with the document neither on disk nor
    /// alive in the panel.
    #[gpui::test]
    async fn test_close_guard_failed_save_vetoes_the_close(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let (doc, close) = start_close(cx, Some("worlds/test.toml"), false);
        cx.simulate_prompt_answer("Save");
        assert!(
            !close.await,
            "a failed save must veto the close, or the unsaved edits are lost"
        );
        doc.read_with(cx, |doc, _| {
            assert_eq!(doc.save_calls, 1, "the save was attempted exactly once")
        });
    }

    /// The entity going away mid-prompt (its panel was dropped) resolves
    /// the guard to `false` -- the fail-safe direction -- rather than
    /// panicking or pretending a save happened.
    #[gpui::test]
    async fn test_close_guard_resolves_false_when_the_entity_is_dropped_mid_prompt(
        cx: &mut TestAppContext,
    ) {
        let cx = cx.add_empty_window();
        let (doc, close) = start_close(cx, Some("worlds/test.toml"), true);
        drop(doc);
        cx.run_until_parked();
        cx.simulate_prompt_answer("Save");
        assert!(
            !close.await,
            "Save against a dropped entity cannot have written anything, so \
             the close must be vetoed"
        );
    }

    // ---------------------------------------------- emerald project roots

    #[test]
    fn emerald_project_root_walks_up_inclusively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(EMERALD_MANIFEST), "").unwrap();
        std::fs::create_dir_all(root.join("assets/tiles")).unwrap();
        assert_eq!(emerald_project_root(root).as_deref(), Some(root));
        assert_eq!(
            emerald_project_root(&root.join("assets/tiles")).as_deref(),
            Some(root)
        );

        let outside = tempfile::tempdir().unwrap();
        assert_eq!(emerald_project_root(outside.path()), None);
    }

    // --------------------------------------------------- child processes

    #[test]
    fn resolve_bin_defaults_and_treats_blank_as_unset() {
        assert_eq!(resolve_bin(None, DEFAULT_EMD_BIN), DEFAULT_EMD_BIN);
        assert_eq!(
            resolve_bin(Some(String::new()), DEFAULT_EMD_BIN),
            DEFAULT_EMD_BIN
        );
        assert_eq!(
            resolve_bin(Some("   ".into()), DEFAULT_EMD_BIN),
            DEFAULT_EMD_BIN
        );
        assert_eq!(
            resolve_bin(Some("/opt/emd".into()), DEFAULT_EMD_BIN),
            "/opt/emd"
        );
    }

    #[test]
    fn emd_request_appends_json_once() {
        let req = ProcRequest::emd(
            "/proj",
            vec!["generate".into(), "module".into(), "x".into()],
        );
        assert_eq!(req.args, ["generate", "module", "x", "--json"]);
        let already = ProcRequest::emd(
            "/proj",
            vec![
                "generate".into(),
                "module".into(),
                "x".into(),
                "--json".into(),
            ],
        );
        assert_eq!(already.args, ["generate", "module", "x", "--json"]);
    }

    /// A non-`emd` request is passed through verbatim -- no `--json`, which
    /// `ggo-diag` does not have.
    #[test]
    fn a_plain_request_is_not_given_the_json_flag() {
        let req = ProcRequest::new("ggo-diag", "/repo", vec!["--launch".into()]);
        assert_eq!(req.args, ["--launch"]);
        assert_eq!(req.bin, "ggo-diag");
    }

    #[test]
    fn command_line_renders_an_empty_arg_as_quotes() {
        let req = ProcRequest::new(
            "emd",
            "/proj",
            vec![
                "component".into(),
                "rm".into(),
                "--module".into(),
                String::new(),
            ],
        );
        assert_eq!(req.command_line(), "emd component rm --module \"\"");
    }

    /// A binary that cannot be spawned must come back as a NON-OK capture
    /// naming the command -- not a panic, and not something a panel could
    /// mistake for success.
    #[test]
    fn a_missing_binary_is_a_non_ok_capture_naming_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let capture = run_capture(&ProcRequest::new(
            "ggo-bin-that-does-not-exist",
            dir.path(),
            vec!["--version".into()],
        ));
        assert!(!capture.ok);
        assert!(
            capture.transcript().contains("ggo-bin-that-does-not-exist"),
            "{}",
            capture.transcript()
        );
    }

    /// stdout is captured BEFORE stderr, which is what makes a failing
    /// `emd` run's trailer (printed to stderr) the last one
    /// `parse_emd_trailer` finds. Exercised through a real child process,
    /// with `sh` standing in.
    #[test]
    fn stderr_is_captured_after_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let capture = run_capture(&ProcRequest::new(
            "sh",
            dir.path(),
            vec![
                "-c".into(),
                "echo 'starting'; echo 'the failure' >&2; exit 1".into(),
            ],
        ));
        assert!(!capture.ok);
        assert_eq!(capture.lines, ["starting", "the failure"]);
    }

    #[test]
    fn a_successful_child_is_an_ok_capture() {
        let dir = tempfile::tempdir().unwrap();
        let capture = run_capture(&ProcRequest::new(
            "sh",
            dir.path(),
            vec!["-c".into(), "echo done".into()],
        ));
        assert!(capture.ok);
        assert_eq!(capture.transcript(), "done");
    }

    /// **The property a timeout is built on**: dropping a
    /// [`run_capture_async`] future KILLS the child, it does not merely
    /// stop reading it.
    ///
    /// Proved against a real process rather than by inspecting the flag: a
    /// child that would create a marker file one second in is spawned (one
    /// poll is enough -- `Command::output()` spawns on first poll), the
    /// future is dropped immediately, and the marker must still be absent
    /// well past the moment it was due. The same script awaited to
    /// completion is the control, so "the marker never appears" cannot
    /// silently mean "the script was wrong".
    #[test]
    fn dropping_a_capture_future_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let script = format!("sleep 1; touch {}", marker.display());
        let request = ProcRequest::new("sh", dir.path(), vec!["-c".into(), script]);

        let mut fut = Box::pin(run_capture_async(&request));
        smol::block_on(smol::future::poll_once(&mut fut));
        drop(fut);
        std::thread::sleep(std::time::Duration::from_millis(2500));
        assert!(
            !marker.exists(),
            "the child outlived the dropped future -- a timeout would leak a \
             running `cargo check`"
        );

        // Control: the very same script, awaited, does create the marker.
        assert!(run_capture(&request).ok);
        assert!(marker.exists(), "the script itself works");
    }

    /// The host queues wire frames while the emulator thread is between
    /// drains; order is the wire's order and a drained frame is never
    /// handed out twice.
    #[test]
    fn link_endpoint_queues_outbound_in_order_and_drains_once() {
        let ep = LinkEndpoint::new();
        ep.send_wire(vec![1, 2]);
        ep.send_wire(vec![3]);
        assert_eq!(ep.take_outbound(), vec![vec![1, 2], vec![3]]);
        assert!(ep.take_outbound().is_empty());
    }

    /// A host that stops polling must not grow the queue without bound:
    /// the cart's newest datagrams are dropped, exactly as the wire would.
    #[test]
    fn link_endpoint_inbound_is_bounded_and_drops_newest_when_full() {
        let ep = LinkEndpoint::new();
        for i in 0..(LINK_INBOUND_CAPACITY + 5) {
            ep.push_inbound(vec![i as u8]);
        }
        let got = ep.try_recv_inbound();
        assert_eq!(got.len(), LINK_INBOUND_CAPACITY);
        assert_eq!(got[0], vec![0u8]);
        assert!(ep.try_recv_inbound().is_empty());
    }

    /// `tick` runs on the emu panel's per-frame path: it must never block,
    /// and many frames' worth of wakes collapse into the one pending wake
    /// the host has yet to observe.
    #[test]
    fn link_endpoint_tick_never_blocks_and_coalesces() {
        let ep = LinkEndpoint::new();
        for _ in 0..10 {
            ep.tick();
        }
        let ticks = ep.ticks();
        assert!(ticks.try_recv().is_ok());
        assert!(
            ticks.try_recv().is_err(),
            "ticks coalesce into one pending wake"
        );
    }

    /// A fresh endpoint is Building -- the cart has not been built yet, so
    /// the world panel must not claim the viewer is live.
    #[test]
    fn link_endpoint_state_starts_building() {
        let ep = LinkEndpoint::new();
        assert_eq!(ep.state(), ViewerState::Building);
        ep.set_state(ViewerState::Stopped("cart exited".into()));
        assert_eq!(ep.state(), ViewerState::Stopped("cart exited".into()));
        assert!(
            ep.ticks().try_recv().is_ok(),
            "a state change wakes the host, or a stopped viewer shows as running \
             until the next frame that will never come"
        );
    }

    #[gpui::test]
    fn boot_viewer_returns_none_without_a_booter(cx: &mut gpui::TestAppContext) {
        // No registry global at all: the world panel must fall back to Design.
        let registered = cx.update(|cx| {
            cx.try_global::<ViewerBooters>()
                .map(|b| b.0.len())
                .unwrap_or(0)
        });
        assert_eq!(registered, 0);
    }
}

/// Collapse every center-pane split: move all items from every other
/// pane into the FIRST pane (emptied panes remove themselves), leaving
/// one pane. The "heavy action" prelude -- starting emulation or opening
/// a report wants the whole center area, so the splits fold away first.
pub fn collapse_center_splits(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let panes: Vec<_> = workspace.panes().to_vec();
    let Some(first) = panes.first().cloned() else {
        return;
    };
    for pane in panes.iter().skip(1) {
        let item_ids: Vec<_> = pane.read(cx).items().map(|item| item.item_id()).collect();
        for item_id in item_ids {
            let destination_index = first.read(cx).items_len();
            workspace::move_item(pane, &first, item_id, destination_index, false, window, cx);
        }
    }
}

// ---------------------------------------------------------- failure text

/// A failure message every GGO panel can render the same way: the text
/// plus a copy button that puts the EXACT string on the clipboard, so
/// any logged failure can be pasted into a bug report or chat. gpui has
/// no selectable static text (real highlight-selection needs an Editor
/// or Markdown entity per site); one-click copy is the working
/// equivalent, applied uniformly.
#[derive(gpui::IntoElement)]
pub struct CopyableText {
    id: gpui::ElementId,
    text: gpui::SharedString,
    color: ui::Color,
    size: ui::LabelSize,
}

impl CopyableText {
    pub fn new(id: impl Into<gpui::ElementId>, text: impl Into<gpui::SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            color: ui::Color::Error,
            size: ui::LabelSize::XSmall,
        }
    }

    pub fn color(mut self, color: ui::Color) -> Self {
        self.color = color;
        self
    }

    pub fn size(mut self, size: ui::LabelSize) -> Self {
        self.size = size;
        self
    }
}

impl ui::prelude::RenderOnce for CopyableText {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl gpui::IntoElement {
        use ui::prelude::*;
        let text = self.text.clone();
        h_flex()
            .gap_1()
            .items_start()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(self.text).size(self.size).color(self.color)),
            )
            .child(
                ui::IconButton::new(self.id, ui::IconName::Copy)
                    .icon_size(ui::IconSize::XSmall)
                    .tooltip(ui::Tooltip::text("Copy message"))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
                    }),
            )
    }
}
