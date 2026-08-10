//! GGO Emerald panel: the surface that creates AND removes engine
//! artifacts -- components, systems, resources, modules, worlds and
//! schedules -- by running `emd` and reporting what it did.
//!
//! **Why the panel exists at this point in the migration.** The spec's
//! rule is that forms live in the panel that owns the domain and the
//! context menu only routes to them, and the project panel's only prompt
//! primitive (`window.prompt`) is button-choice: it cannot collect a name,
//! let alone a module and a repeatable list of `name:kind` fields. So
//! "New Component…" needs a real form, and a form needs a panel (F5.2/S3).
//! **F5.3/E2** adds the other half: a browser over the three manifests and
//! the remove/field ops against them, each behind a confirm that names
//! what it breaks. **F5.3/E3** makes the schedules tab's run list
//! editable -- reorder, add, drop and cadence, each one `emd schedule set`
//! carrying the whole new list, shown optimistically and rolled back
//! visibly when `emd` refuses it. **F5.3/E4** closes the loop with the
//! version lock: a banner naming the drift, a gate that keeps every
//! mutation off a mismatched CLI, and an mtime-gated poll so a re-installed
//! `emd` is noticed without a restart ([`lock`]).
//!
//! **What is NOT here yet.** A streaming run console with a cancel button.
//! Runs are one-shot calls whose only interesting output is a JSON trailer
//! -- but, unlike F5.2's generates, the mutations are `cargo check`-backed
//! and slow, which is why they run under [`runner::EMD_TIMEOUT`].
//!
//! Structural mirror of the recent siblings (`ggo_map_panel`,
//! `ggo_sprite_panel`): `Panel` impl, `ToggleFocus`, `observe_new`
//! registration into every new workspace, a `KeymapEventChannel` observer
//! so panel keybinds survive `zed::reload_keymaps`, a context-menu
//! contributor whose entries defer all panel work into
//! `ggo_common::panel_entry_handler`, and a load/run-generation guard so a
//! superseded result is dropped rather than applied.
//!
//! Module split: [`forms`] is the pure "what would we run, and is it
//! submittable" half of the generate forms (no gpui), [`ops`] is the same
//! for the manifest mutations -- including the confirm text, so "the
//! schedules that break are named in the prompt" is testable without a
//! window -- [`manifests`] reads the manifests and gathers the blast
//! radius, [`runner`] is the "how do we run it" half (no gpui either, and
//! injectable so tests never need `emd` on `PATH`), [`lock`] is the version
//! lock's banner text and gating rule (pure, and it is where the
//! pre-release asymmetry between worldlib's two version comparisons is
//! resolved), and [`tileset`] is the one artifact this panel writes itself
//! rather than delegating to `emd`. This module is the gpui glue.

mod forms;
mod lock;
mod manifests;
mod ops;
mod runner;
mod tileset;

use std::path::{Path, PathBuf};

use editor::Editor;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding,
    Pixels, Render, SharedString, Styled, Task, WeakEntity, Window, actions, div, px,
};
use project::ProjectPath;
use ui::prelude::*;
use ui::{ContextMenu, DropdownMenu};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_world_panel::WorldPanel;
use ggo_worldlib::emerald::{
    EXPECTED_EMD_VERSION, ManifestKind, OrderEdit, apply_order_edit, available_systems,
    emd_error_message, emd_reverted, group_by_module, parse_cadenced_ref, schedules_using_system,
    system_ref, verify_emd_result, with_cadence,
};

use forms::{ASSET_KIND, FIELD_KINDS, FieldDraft, GenDraft, GenKind};
use lock::{BinProbe, EMD_LOCK_POLL_INTERVAL, LockCheck, LockProbe};
use manifests::{ASSETS_DIR, MANIFESTS_DIR, Manifests};
use ops::ManifestOp;
use runner::{
    EMD_TIMEOUT, EMERALD_MANIFEST, EmdRequest, EmdRunner, emerald_project_root, system_runner,
};

actions!(
    ggo_emerald,
    [
        /// Toggles focus on the GGO emerald panel.
        ToggleFocus,
        /// Submits the open generate/new-tileset form.
        Submit
    ]
);

const GGO_EMERALD_PANEL_KEY: &str = "GGOEmeraldPanel";

/// The panel's key-dispatch context identifier.
const KEY_CONTEXT: &str = "GgoEmeraldPanel";

/// Fixed default width until the panel grows real settings persistence
/// (same call every other GGO panel made at this stage).
const DEFAULT_WIDTH: Pixels = px(420.);

/// Group header for the shared (module-less) bucket -- `group_by_module`
/// keys it on `""`, which is not a heading.
const SHARED_MODULE_LABEL: &str = "(shared)";

/// What a rolled-back mutation says before `emd`'s own compiler message.
/// Mirrors ggo-ide's "Reverted -- the compiler rejected the change:", which
/// it rendered as its own block for the same reason.
const REVERTED_PREFIX: &str = "Rolled back — the project no longer compiles: ";

/// What the run list says when an edit did not land and the panel put the
/// saved order back. The run state line carries `emd`'s own reason; this
/// line's job is the other half -- that the list on screen is no longer
/// the one the user just made.
const ORDER_ROLLBACK_NOTICE: &str = "Edit not applied — this is the saved run list again.";

/// The cadences the per-row picker offers. A fixed list rather than a
/// number field for the same reason [`FIELD_KINDS`] is one: it makes an
/// invalid cadence unreachable instead of merely rejected (`emd` refuses
/// `@0`, and worldlib's `parse_cadenced_ref` refuses to even split a
/// malformed suffix). A run list that already carries some other cadence
/// still SHOWS it -- the picker's label is the current value, whatever it
/// is; the list is only what can be chosen.
const CADENCES: [u32; 8] = [1, 2, 3, 4, 6, 8, 12, 16];

/// Empty-state text -- shown when there is nothing to list and no form
/// open, i.e. an unmanaged project (or one whose manifests are still
/// empty), where work can only arrive by right-clicking a directory.
const EMPTY_MESSAGE: &str = "Right-click the project root or manifests/ → New Component…, or an assets directory → New World…";

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as every other GGO panel's `init`: `zed::reload_keymaps`
    // clears and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), and keymap assets are upstream files
    // this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` keeps the panel's bindings alive across reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    // Right-clicking a directory offers the generate entries that belong
    // to it. Deliberately NOT a `register_path_open_interceptor`: this
    // panel claims no file extension -- the artifacts `emd generate`
    // writes are Rust sources and TOML that upstream's editor opens
    // perfectly well.
    workspace::register_context_menu_contributor(cx, contribute_emerald_menu);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| EmeraldPanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<EmeraldPanel>(window, cx);
        });
    })
    .detach();
}

/// Enter submits the open form.
///
/// Scoped `KEY_CONTEXT > Editor` for the same reason `ggo_world_panel`
/// scopes its own commit binding there: single-line editors don't bind
/// Enter themselves (the default keymap's `enter -> editor::Newline` is
/// `mode == full` only), so this fires while a form field is focused,
/// which is the only time the form can be submitted from the keyboard.
fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "enter",
        Submit,
        Some(&format!("{KEY_CONTEXT} > Editor")),
    )]);
}

// -------------------------------------------------------- the context menu

/// `workspace::ContextMenuContributor` for the directories that own
/// generate actions:
///
/// | right-clicked directory        | entries |
/// |--------------------------------|---------|
/// | the emerald project root, or its `manifests/` | New Component… / New System… / New Schedule… |
/// | the project's `assets/`, or anything under it | New World… / New Tileset… |
/// | anything else                  | none |
///
/// The three named entries are the spec's list. **Resource and Module
/// have forms but no entry of their own**: the form carries a kind
/// selector, so they are one click away inside any of the three, and a
/// six-entry directory menu appended to upstream's own Duplicate/Rename/
/// Delete would be worse than a selector. `New World…` sits with the
/// assets entries because a world is an asset file -- though note `emd`
/// writes it to `<root>/assets/worlds/` regardless of WHICH assets
/// directory was clicked (`emd generate world --help` says so), so the
/// click chooses the project, not the destination.
///
/// MUST NOT touch the project panel or any GGO panel: contributors run
/// while `ProjectPanel` is leased (see
/// `Workspace::context_menu_contributions`). Everything panel-shaped is
/// deferred into the entries' handlers via
/// [`ggo_common::panel_entry_handler`], which run after the lease is
/// released. The `is_file`/`is_dir` stats the two predicates make are not
/// panel work and are legal here, same as in `ggo_map_panel`.
fn contribute_emerald_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    if !is_dir {
        return Vec::new();
    }
    // Declines a path outside the primary worktree AND a non-local project
    // (SSH remote / collab guest): `emd` is spawned against the worktree's
    // `abs_path`, which on a remote project names a directory that does
    // not exist on this machine.
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    let Some(worktree_root) = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    else {
        return Vec::new();
    };
    let dir = worktree_root.join(&rel);

    let mut items: Vec<ui::ContextMenuItem> = Vec::new();
    if is_generate_dir(&dir) {
        for kind in [GenKind::Component, GenKind::System, GenKind::Schedule] {
            items.push(
                ui::ContextMenuEntry::new(format!("New {}…", kind.noun()))
                    .icon(ui::IconName::Plus)
                    .handler(new_item_handler(cx.weak_entity(), kind, rel.clone()))
                    .into(),
            );
        }
    }
    if is_assets_dir(&dir) {
        items.push(
            ui::ContextMenuEntry::new("New World…")
                .icon(ui::IconName::Plus)
                .handler(new_item_handler(
                    cx.weak_entity(),
                    GenKind::World,
                    rel.clone(),
                ))
                .into(),
        );
        items.push(
            ui::ContextMenuEntry::new("New Tileset…")
                .icon(ui::IconName::Plus)
                .handler(new_tileset_handler(cx.weak_entity(), rel))
                .into(),
        );
    }
    items
}

/// The generate entries' handler. Split out from
/// [`contribute_emerald_menu`] so a test can invoke exactly what the menu
/// invokes -- `ContextMenuEntry` keeps its handler private, so a
/// contributed entry cannot be fired from a test any other way.
fn new_item_handler(
    workspace: WeakEntity<Workspace>,
    kind: GenKind,
    dir_rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(
        workspace,
        move |panel: &Entity<EmeraldPanel>, window, cx| {
            let dir_rel = dir_rel.clone();
            panel.update(cx, |panel, cx| panel.new_item(kind, &dir_rel, window, cx));
        },
    )
}

/// The "New Tileset…" entry's handler -- see [`new_item_handler`].
fn new_tileset_handler(
    workspace: WeakEntity<Workspace>,
    dir_rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    ggo_common::panel_entry_handler(
        workspace,
        move |panel: &Entity<EmeraldPanel>, window, cx| {
            let dir_rel = dir_rel.clone();
            panel.update(cx, |panel, cx| panel.new_tileset(&dir_rel, window, cx));
        },
    )
}

/// Is `dir` a directory the manifest-backed generate entries belong on --
/// the emerald project root itself, or its `manifests/` directory?
///
/// Both, rather than just one: `manifests/` is where a user looking for
/// "the components" ends up, and the root is where a user who has never
/// opened `manifests/` right-clicks. Nothing deeper qualifies, so a
/// right-click anywhere in `crates/` stays clean.
fn is_generate_dir(dir: &Path) -> bool {
    emerald_project_root(dir).is_some_and(|root| dir == root || dir == root.join(MANIFESTS_DIR))
}

/// Walk up from `dir` (inclusive) to the nearest emerald project root,
/// returning that project's `assets/` dir. Same helper (and same
/// directory-inclusive start) as `ggo_map_panel`'s.
fn emerald_asset_root(dir: &Path) -> Option<PathBuf> {
    let assets = emerald_project_root(dir)?.join(ASSETS_DIR);
    assets.is_dir().then_some(assets)
}

/// Is `dir` the asset root of an emerald project, or a directory under it?
fn is_assets_dir(dir: &Path) -> bool {
    emerald_asset_root(dir).is_some_and(|assets| dir.starts_with(&assets))
}

// ------------------------------------------------------------- view state

/// One `--field name:kind` row's widgets. The kind is panel state rather
/// than an editor because it is chosen from a fixed list
/// ([`FIELD_KINDS`]), which is what makes an invalid kind unreachable
/// instead of merely rejected.
struct FieldRow {
    name: Entity<Editor>,
    kind: String,
    ext: Entity<Editor>,
}

/// The `emd generate` form.
struct GenerateForm {
    kind: GenKind,
    /// `emd`'s working directory: the emerald project root the clicked
    /// directory belongs to. Resolved when the form opens, so a run can
    /// never be aimed at a project the user has since navigated away from.
    project_dir: PathBuf,
    name: Entity<Editor>,
    module: Entity<Editor>,
    fields: Vec<FieldRow>,
}

/// The "New Tileset…" form. Not an `emd` command -- see [`tileset`].
struct TilesetForm {
    /// The asset root the pair is written under, and the clicked
    /// directory RELATIVE TO IT -- the asset-root-relative frame every
    /// downstream binder stores (the F4 `ggo-sprfix` contract).
    asset_root: PathBuf,
    under: String,
    stem: Entity<Editor>,
    error: Option<String>,
}

enum PanelForm {
    Generate(GenerateForm),
    Tileset(TilesetForm),
}

/// What the panel is showing about the most recent `emd` run.
enum RunState {
    Idle,
    Running {
        command: String,
    },
    Done {
        message: String,
        transcript: String,
    },
    Failed {
        message: String,
        transcript: String,
    },
    /// **Not a failure.** `emd` applied the change, its `cargo check`
    /// rejected the result, and it put everything back
    /// (`ggo_worldlib::emerald::emd_reverted`). The project is exactly as
    /// it was, which is the opposite of what a red "failed" implies about
    /// a half-applied mutation -- so it renders as its own state, with
    /// `emd`'s compiler message beneath it.
    Reverted {
        message: String,
        transcript: String,
    },
}

/// Which manifest the browser is listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowseTab {
    Components,
    Systems,
    Schedules,
}

impl BrowseTab {
    const ALL: [BrowseTab; 3] = [
        BrowseTab::Components,
        BrowseTab::Systems,
        BrowseTab::Schedules,
    ];

    fn label(self) -> &'static str {
        match self {
            BrowseTab::Components => "Components",
            BrowseTab::Systems => "Systems",
            BrowseTab::Schedules => "Schedules",
        }
    }

    /// The manifest kind a row on this tab removes.
    fn kind(self) -> ManifestKind {
        match self {
            BrowseTab::Components => ManifestKind::Component,
            BrowseTab::Systems => ManifestKind::System,
            BrowseTab::Schedules => ManifestKind::Schedule,
        }
    }
}

/// Which run's result is coming back, and what to do with it. Carried
/// through the spawn rather than parked in a field so a superseded run
/// cannot apply the current one's effect (the generation guard drops it
/// first, but the data never gets confused either).
enum PendingRun {
    Generate { kind: GenKind, name: String },
    Manifest(ManifestOp),
}

pub struct EmeraldPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// How `emd` is actually run -- the injection seam. Production gets
    /// [`system_runner`]; tests swap in a recording fake, which is what
    /// lets "validation rejects this before any spawn" be an assertion
    /// about a call COUNT rather than about a message.
    runner: EmdRunner,
    form: Option<PanelForm>,
    /// The emerald project root the BROWSER reads its manifests from (and
    /// the cwd its mutations run in). Not the same thing as
    /// `project_root`, which is the worktree: an emerald project can sit
    /// below the worktree root, and `emd` discovers a project from its cwd.
    emerald_dir: Option<PathBuf>,
    manifests: Manifests,
    tab: BrowseTab,
    /// The selected row on the ACTIVE tab, by manifest name. Cleared when
    /// the tab changes or the row stops existing -- a stale selection is
    /// how a detail pane ends up offering to remove a field from a
    /// component that is already gone.
    selected: Option<String>,
    /// The open "+ Field" row on the selected component, if any.
    field_form: Option<FieldRow>,
    /// The selected SCHEDULE's run list, as the panel is showing it right
    /// now -- which is not always what the manifest says. An edit is shown
    /// the instant its `emd schedule set` starts (the optimistic commit),
    /// and [`EmeraldPanel::refresh_manifests`] resyncs this from disk the
    /// instant that run ends, whichever way it ended. Empty when the
    /// Schedules tab has no selection.
    schedule_order: Vec<String>,
    /// Set when a schedule edit did NOT land and the list the user is
    /// looking at was put back. Rendered right at the run list, because
    /// that is where the edit visibly disappeared from -- a silent
    /// rollback would leave the user believing an edit landed that did
    /// not, which is worse than not being optimistic at all.
    order_rollback: Option<&'static str>,
    /// What the panel last learned about the installed `emd`'s version.
    /// **Every mutation is gated on this** (`lock::mutations_enabled`), and
    /// it is what the banner renders. Updated by the poll below, and
    /// immediately by a finished run whose own trailer disagrees with
    /// [`EXPECTED_EMD_VERSION`] -- that trailer is a first-hand sighting of
    /// the binary, so it does not wait for the next tick.
    lock: LockCheck,
    /// The last stat of the `emd` binary. The poll spends an `emd version`
    /// child process only when this CHANGES, so a steady-state install
    /// costs one `fs::metadata` per tick and nothing else.
    emd_probe: BinProbe,
    /// How that stat is taken -- injected for the same reason [`runner`]
    /// is, and additionally so "the poll is not on the render path" can be
    /// asserted as a call count.
    probe: LockProbe,
    run_state: RunState,
    run_generation: u64,
    _run_task: Option<Task<()>>,
    _confirm_task: Option<Task<()>>,
    _lock_task: Option<Task<()>>,
}

impl EmeraldPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            runner: system_runner(),
            form: None,
            emerald_dir: None,
            manifests: Manifests::default(),
            tab: BrowseTab::Components,
            selected: None,
            field_form: None,
            schedule_order: Vec::new(),
            order_rollback: None,
            lock: LockCheck::Unchecked,
            emd_probe: BinProbe::Unprobed,
            probe: lock::system_probe(),
            run_state: RunState::Idle,
            run_generation: 0,
            _run_task: None,
            _confirm_task: None,
            _lock_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree). MUST NOT run while the workspace itself is mid-update
    /// (it reads the workspace entity) -- see the deferral in `set_active`.
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        // The worktree root is not necessarily the emerald project root;
        // walk up (inclusively) the way emerald's own `Project::discover`
        // does, and fall back to the worktree itself so a checkout with no
        // `emerald.toml` simply lists nothing rather than reading some
        // ancestor's manifests.
        let managed = self.project_root.as_deref().and_then(emerald_project_root);
        self.emerald_dir = managed.clone().or_else(|| self.project_root.clone());
        // Only a checkout that actually HAS an emerald project is worth
        // watching `emd` for -- `emerald_dir` falls back to the worktree
        // root, so it is `Some` everywhere and cannot stand in for this.
        if managed.is_some() {
            self.start_lock_poll(cx);
        }
        self.refresh_manifests(cx);
    }

    /// Start the `emd` version-lock poll, once.
    ///
    /// **Here, not in [`EmeraldPanel::new`]**: `new` runs for every
    /// workspace the moment it opens (`init`'s `observe_new`), including
    /// every checkout that has no emerald project and every session where
    /// this dock is never touched. `refresh_root` is the panel being USED
    /// -- the dock activating, or a context-menu entry opening a form --
    /// which is the first moment the answer can matter. It is also the
    /// reason the gate's `Unchecked` window is narrow in practice: the poll
    /// has been running since before the first form could be filled in.
    ///
    /// **Not on the render path, and it cannot drift onto it**: the only
    /// caller is `refresh_root`, the loop lives in a spawned task, and the
    /// stat itself is handed to the background executor. A render reads
    /// `self.lock` and nothing else.
    ///
    /// **Why a poll at all.** Nothing in Zed reports "a binary on PATH was
    /// replaced"; the honest alternatives are a filesystem watch on a path
    /// that may not exist yet, or re-running `emd version` before every
    /// mutation (a child process per click, which is exactly the smell E2's
    /// review flagged). A tick is one `fs::metadata` and it only spends a
    /// child process when the mtime actually moved --
    /// [`EMD_LOCK_POLL_INTERVAL`] explains the interval.
    ///
    /// **A project switch does NOT force a re-check**, unlike ggo-ide's
    /// version of this: the `emd` binary is resolved from `GGO_EMD`/`PATH`,
    /// never from the project, so which project is open cannot change the
    /// answer. The binary's identity is fully captured by the probe, and
    /// the probe is what triggers a re-query.
    fn start_lock_poll(&mut self, cx: &mut Context<Self>) {
        if self._lock_task.is_some() {
            return;
        }
        self._lock_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok((probe, known, dir, runner)) = this.read_with(cx, |this, _| {
                    (
                        this.probe.clone(),
                        this.emd_probe,
                        this.emerald_dir.clone(),
                        this.runner.clone(),
                    )
                }) else {
                    return;
                };
                let seen = cx.background_executor().spawn(async move { probe() }).await;
                if seen != known {
                    // `emd version` needs a cwd, but not a project one: it
                    // reports the binary's own semver wherever it runs.
                    let request = EmdRequest::emd(
                        dir.unwrap_or_else(|| PathBuf::from(".")),
                        lock::version_args(),
                    );
                    let outcome = cx
                        .background_executor()
                        .spawn(async move { runner(request).await })
                        .await;
                    let check = lock::check_from_outcome(&outcome);
                    if this
                        .update(cx, |this, cx| {
                            this.emd_probe = seen;
                            this.lock = check;
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                cx.background_executor().timer(EMD_LOCK_POLL_INTERVAL).await;
            }
        }));
    }

    /// Whether an `emd` mutation may start right now: the version lock
    /// allows it and no run is already in flight.
    ///
    /// The one predicate behind every `disabled(...)` on a mutating control
    /// AND behind the guards on the spawn path, so a button that looks
    /// clickable and an action that actually runs cannot disagree.
    fn mutations_blocked(&self) -> bool {
        self.running() || !lock::mutations_enabled(&self.lock)
    }

    /// Re-read the three manifests, and drop a selection that no longer
    /// names anything (the row it points at was just removed).
    fn refresh_manifests(&mut self, cx: &mut Context<Self>) {
        self.manifests = self
            .emerald_dir
            .as_deref()
            .map(manifests::read_manifests)
            .unwrap_or_default();
        if self
            .selected
            .as_deref()
            .is_some_and(|name| self.entry_module(name).is_none())
        {
            self.clear_selection();
        }
        // The manifest is the truth about a run list -- EXCEPT while a
        // `schedule set` is in flight, which is the one window the
        // optimistic order exists for. `finish_run` sets the terminal run
        // state BEFORE it refreshes, so a finished run always resyncs
        // here: that is what makes the rollback a re-read of what `emd`
        // actually left on disk rather than a remembered vector that could
        // drift from it.
        if !self.running() {
            self.resync_schedule_order();
        }
        cx.notify();
    }

    /// Re-read the shown run list from the manifests.
    fn resync_schedule_order(&mut self) {
        self.schedule_order = self
            .selected_schedule()
            .and_then(|(name, _)| self.manifests.schedule(&name))
            .map(|entry| entry.systems.clone())
            .unwrap_or_default();
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.field_form = None;
        self.schedule_order.clear();
        self.order_rollback = None;
    }

    /// The module of the named entry on the ACTIVE tab, or `None` when this
    /// tab has no such entry. `Some("")` is a real answer -- the shared
    /// module -- which is why this is not a `contains`.
    fn entry_module(&self, name: &str) -> Option<String> {
        match self.tab {
            BrowseTab::Components => self.manifests.component(name).map(|c| c.module.clone()),
            BrowseTab::Systems => self.manifests.system(name).map(|s| s.module.clone()),
            BrowseTab::Schedules => self.manifests.schedule(name).map(|s| s.module.clone()),
        }
    }

    // ------------------------------------------------------------- forms

    /// Open the generate form for `kind`, aimed at the emerald project
    /// that owns the worktree-relative directory `dir_rel`. The body of
    /// the "New Component…"/"New System…"/"New Schedule…"/"New World…"
    /// entries.
    ///
    /// Refreshes the root FIRST because `project_root` is only
    /// re-discovered on panel activation and a right-click can reach a
    /// panel that has never been activated. Safe here: the caller is a
    /// context-menu entry handler, which runs outside both the project
    /// panel's lease and any `Workspace` update.
    pub fn new_item(
        &mut self,
        kind: GenKind,
        dir_rel: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_root(cx);
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let Some(project_dir) = emerald_project_root(&project_root.join(dir_rel)) else {
            self.run_state = RunState::Failed {
                message: format!("no {EMERALD_MANIFEST} above {dir_rel}"),
                transcript: String::new(),
            };
            cx.notify();
            return;
        };
        // The clicked directory decides which project the browser lists
        // too, not just which one the form writes to -- otherwise a
        // right-click in a nested emerald project would open a form aimed
        // at it while the lists below still showed the worktree's.
        self.emerald_dir = Some(project_dir.clone());
        self.refresh_manifests(cx);
        self.form = Some(PanelForm::Generate(GenerateForm {
            kind,
            project_dir,
            name: single_line(window, cx),
            module: single_line(window, cx),
            fields: Vec::new(),
        }));
        self.run_state = RunState::Idle;
        cx.notify();
    }

    /// Open the "New Tileset…" form for the worktree-relative directory
    /// `dir_rel`.
    pub fn new_tileset(&mut self, dir_rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_root(cx);
        let Some(project_root) = self.project_root.clone() else {
            return;
        };
        let dir_abs = project_root.join(dir_rel);
        let Some(asset_root) = emerald_asset_root(&dir_abs) else {
            return;
        };
        let under = dir_abs
            .strip_prefix(&asset_root)
            .map(|p| p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
            .unwrap_or_default();
        self.form = Some(PanelForm::Tileset(TilesetForm {
            asset_root,
            under,
            stem: single_line(window, cx),
            error: None,
        }));
        self.run_state = RunState::Idle;
        cx.notify();
    }

    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    /// Switch the open generate form to another kind, keeping whatever is
    /// typed. Safe because [`GenDraft::args`] ignores the parts the new
    /// kind doesn't take (proved by `module_and_world_never_pass_a_module_flag`
    /// and `only_component_emits_fields`).
    fn select_kind(&mut self, kind: GenKind, cx: &mut Context<Self>) {
        if let Some(PanelForm::Generate(form)) = &mut self.form {
            form.kind = kind;
            cx.notify();
        }
    }

    fn select_field_kind(&mut self, ix: usize, kind: &str, cx: &mut Context<Self>) {
        if let Some(PanelForm::Generate(form)) = &mut self.form
            && let Some(row) = form.fields.get_mut(ix)
        {
            row.kind = kind.to_string();
            cx.notify();
        }
    }

    fn add_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = FieldRow {
            name: single_line(window, cx),
            kind: FieldDraft::default().kind,
            ext: single_line(window, cx),
        };
        if let Some(PanelForm::Generate(form)) = &mut self.form {
            form.fields.push(row);
            cx.notify();
        }
    }

    fn remove_field(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(PanelForm::Generate(form)) = &mut self.form
            && ix < form.fields.len()
        {
            form.fields.remove(ix);
            cx.notify();
        }
    }

    /// Read the open generate form's editors into the pure draft every
    /// decision is made from -- validation, the "stored as" hint, and the
    /// argv. There is no other path from widgets to `emd`.
    fn draft(&self, cx: &App) -> Option<GenDraft> {
        let PanelForm::Generate(form) = self.form.as_ref()? else {
            return None;
        };
        Some(GenDraft {
            kind: form.kind,
            name: form.name.read(cx).text(cx).trim().to_string(),
            module: form.module.read(cx).text(cx).trim().to_string(),
            fields: form
                .fields
                .iter()
                .map(|row| FieldDraft {
                    name: row.name.read(cx).text(cx).trim().to_string(),
                    kind: row.kind.clone(),
                    ext: row.ext.read(cx).text(cx).trim().to_string(),
                })
                .collect(),
        })
    }

    fn running(&self) -> bool {
        matches!(self.run_state, RunState::Running { .. })
    }

    // ------------------------------------------------------ browsing

    fn select_tab(&mut self, tab: BrowseTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            // Names are only unique WITHIN a manifest, so a selection can
            // never survive a tab change.
            self.clear_selection();
            cx.notify();
        }
    }

    /// Select (or, on a second click, deselect) a row on the active tab.
    fn select_item(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(name) {
            self.clear_selection();
        } else {
            self.selected = Some(name.to_string());
            self.field_form = None;
            self.order_rollback = None;
            self.resync_schedule_order();
        }
        cx.notify();
    }

    /// Open (or close) the "+ Field" row on the selected component.
    fn toggle_field_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.field_form = match self.field_form {
            Some(_) => None,
            None => Some(FieldRow {
                name: single_line(window, cx),
                kind: FieldDraft::default().kind,
                ext: single_line(window, cx),
            }),
        };
        cx.notify();
    }

    fn select_new_field_kind(&mut self, kind: &str, cx: &mut Context<Self>) {
        if let Some(row) = &mut self.field_form {
            row.kind = kind.to_string();
            cx.notify();
        }
    }

    /// The "+ Field" row as the pure draft its validation and spec come
    /// from -- the same [`FieldDraft`] the generate form uses, so the two
    /// paths cannot disagree about what `emd` accepts.
    fn field_draft(&self, cx: &App) -> Option<FieldDraft> {
        let row = self.field_form.as_ref()?;
        Some(FieldDraft {
            name: row.name.read(cx).text(cx).trim().to_string(),
            kind: row.kind.clone(),
            ext: row.ext.read(cx).text(cx).trim().to_string(),
        })
    }

    // ------------------------------------------------- manifest mutations

    /// Remove the named entry from the ACTIVE tab's manifest, after a
    /// confirm that names what it breaks. The body of every row's trash
    /// button.
    fn request_remove(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(module) = self.entry_module(name) else {
            return;
        };
        self.request_op(
            ManifestOp::remove(self.tab.kind(), name, &module),
            window,
            cx,
        );
    }

    /// Remove `field` from the selected component, after a confirm.
    fn request_field_remove(&mut self, field: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((component, module)) = self.selected_component() else {
            return;
        };
        self.request_op(
            ManifestOp::field_remove(&component, &module, field),
            window,
            cx,
        );
    }

    /// Add the "+ Field" row's field to the selected component. No
    /// confirm -- it is the one manifest op that only ever adds -- but the
    /// same validation gate as the generate form: an invalid spec never
    /// reaches the runner.
    fn submit_field_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((component, module)) = self.selected_component() else {
            return;
        };
        let Some(draft) = self.field_draft(cx) else {
            return;
        };
        let spec = draft.spec();
        if !ggo_worldlib::emerald::valid_field_spec(&draft.name, &spec) {
            cx.notify();
            return;
        }
        self.request_op(
            ManifestOp::field_add(&component, &module, &format!("{}:{spec}", draft.name)),
            window,
            cx,
        );
    }

    /// The selected component's `(name, module)`, or `None` when the
    /// Components tab has no live selection.
    fn selected_component(&self) -> Option<(String, String)> {
        if self.tab != BrowseTab::Components {
            return None;
        }
        let name = self.selected.clone()?;
        let module = self.manifests.component(&name)?.module.clone();
        Some((name, module))
    }

    /// The selected schedule's `(name, module)` -- [`selected_component`]'s
    /// twin for the Schedules tab.
    ///
    /// [`selected_component`]: EmeraldPanel::selected_component
    fn selected_schedule(&self) -> Option<(String, String)> {
        if self.tab != BrowseTab::Schedules {
            return None;
        }
        let name = self.selected.clone()?;
        let module = self.manifests.schedule(&name)?.module.clone();
        Some((name, module))
    }

    // ------------------------------------------------ the schedule run list

    /// Move the entry at `from` one row up or down.
    ///
    /// The bounds are checked HERE rather than only on the buttons'
    /// `disabled`, for the same reason `submit_generate` re-checks the
    /// draft: `apply_order_edit` is deliberately tolerant of an
    /// out-of-range index (it no-ops), and a no-op edit that still spawned
    /// `emd` would be a `cargo`-free but pointless manifest rewrite.
    fn move_system(&mut self, from: usize, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if up && from == 0 || !up && from + 1 >= self.schedule_order.len() {
            return;
        }
        let Some(system_ref) = self.schedule_order.get(from).cloned() else {
            return;
        };
        let to = if up { from - 1 } else { from + 1 };
        self.commit_order(
            OrderEdit::Move { from, to },
            ops::ScheduleEdit::Move {
                system_ref,
                from,
                to,
            },
            window,
            cx,
        );
    }

    /// Drop the entry at `index` from the run list -- the one schedule
    /// edit that confirms first ([`ops::ManifestOp::destructive`]).
    fn remove_system(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(system_ref) = self.schedule_order.get(index).cloned() else {
            return;
        };
        self.commit_order(
            OrderEdit::Remove { index },
            ops::ScheduleEdit::Remove { system_ref, index },
            window,
            cx,
        );
    }

    /// Append a system to the run list -- `system_ref` comes from
    /// [`available_systems`], so a duplicate (or a name `emd` has never
    /// heard of) is unreachable rather than merely rejected.
    fn add_system(&mut self, system_ref: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_order(
            OrderEdit::Add {
                system_ref: system_ref.to_string(),
            },
            ops::ScheduleEdit::Add {
                system_ref: system_ref.to_string(),
            },
            window,
            cx,
        );
    }

    /// Set the entry at `index`'s `@N` cadence.
    ///
    /// **Not an [`OrderEdit`]**, and deliberately not faked as one: the
    /// order is untouched, and worldlib's own pair for this is
    /// [`parse_cadenced_ref`] + [`with_cadence`] (an `OrderEdit::Remove`
    /// followed by an `Add` would move the row to the end, which is a
    /// different edit entirely). The list mutation is still not
    /// hand-rolled -- the ref itself is rebuilt by `with_cadence`, which
    /// owns the "cadence 1 carries no suffix" rule.
    fn set_cadence(
        &mut self,
        index: usize,
        cadence: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.schedule_order.get(index) else {
            return;
        };
        let (base, _) = parse_cadenced_ref(current);
        let mut next = self.schedule_order.clone();
        next[index] = with_cadence(&base, cadence);
        self.commit_order_list(
            next,
            ops::ScheduleEdit::Cadence {
                system_ref: base,
                cadence,
            },
            window,
            cx,
        );
    }

    /// Apply one [`OrderEdit`] to the shown run list and commit the result.
    ///
    /// The panel never splices the list itself: it hands the current order
    /// and the edit to worldlib's [`apply_order_edit`] and commits what
    /// comes back, so "up" at the top and "remove" past the end behave the
    /// way the unit-tested pure function says they do rather than the way
    /// a second implementation here would.
    fn commit_order(
        &mut self,
        edit: OrderEdit,
        kind: ops::ScheduleEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = apply_order_edit(&self.schedule_order, edit);
        self.commit_order_list(next, kind, window, cx);
    }

    /// Commit a whole new run list: one `emd schedule set` carrying it.
    ///
    /// **The concurrent-edit rule is "reject while running"**, enforced by
    /// [`request_op`]'s own `running()` guard and stated on the buttons
    /// (they are disabled mid-run). Last-write-wins would be the wrong
    /// choice here: the second edit is computed from the OPTIMISTIC order,
    /// so if the first run fails, the second would commit a list derived
    /// from a state that never existed on disk -- silently reapplying the
    /// rejected edit. Refusing the second edit keeps every committed list
    /// derived from a list `emd` has accepted.
    ///
    /// [`request_op`]: EmeraldPanel::request_op
    fn commit_order_list(
        &mut self,
        next: Vec<String>,
        kind: ops::ScheduleEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((schedule, module)) = self.selected_schedule() else {
            return;
        };
        if next == self.schedule_order {
            return;
        }
        self.request_op(
            ManifestOp::schedule_set(&schedule, &module, next, kind),
            window,
            cx,
        );
    }

    /// **The one gate on every manifest mutation.** Gathers the blast
    /// radius, raises the confirm the op needs, and only spawns `emd` if
    /// the user says yes.
    ///
    /// `ops::confirm_for` returning `None` is the only way to skip the
    /// prompt, and it does that for exactly one op (adding a field), so a
    /// new destructive op cannot be added later that silently runs -- it
    /// would have to opt out in `ManifestOp::destructive` to do so, in
    /// plain sight.
    fn request_op(&mut self, op: ManifestOp, window: &mut Window, cx: &mut Context<Self>) {
        // The version lock is consulted BEFORE the confirm, not only at the
        // spawn: raising "this will break two schedules, are you sure?" for
        // a mutation that cannot run either way is a prompt whose only
        // possible outcome is nothing happening.
        if self.mutations_blocked() {
            return;
        }
        let Some(project_dir) = self.emerald_dir.clone() else {
            return;
        };
        let cascade = manifests::cascade_for(&op, &self.manifests, &project_dir);
        let Some(confirm) = ops::confirm_for(&op, &cascade) else {
            self.run_op(op, project_dir, window, cx);
            return;
        };
        // `unsaved: false` always: this panel holds no document, so there
        // is never an unsaved edit of its own to warn about (its
        // `Panel::prepare_to_close` says the same thing). The cascade
        // lines carry what IS at stake.
        let answer = ggo_common::confirm_destructive_cascade(
            &confirm.message,
            &confirm.cascade,
            confirm.label,
            false,
            window,
            cx,
        );
        self._confirm_task = Some(cx.spawn_in(window, async move |this, cx| {
            if !answer.await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                // Re-checked on THIS side of the await: the dialog was
                // open for a while, and a run may have started, the
                // project moved, or the lock poll noticed a swapped `emd`
                // since it went up.
                if this.mutations_blocked() {
                    return;
                }
                let Some(project_dir) = this.emerald_dir.clone() else {
                    return;
                };
                this.run_op(op, project_dir, window, cx);
            })
            .ok();
        }));
    }

    /// Start `op`. **The optimistic commit lives here**: a schedule set's
    /// new run list becomes what the panel shows the moment the run
    /// starts -- not when the edit was computed, so a confirm the user
    /// cancels never moves a row, and not when the run finishes, which is
    /// the wait this optimism exists to hide.
    ///
    /// Nothing needs to be remembered to undo it: on disk the run list is
    /// either what `emd` wrote or what it was, and
    /// [`EmeraldPanel::refresh_manifests`] re-reads it either way.
    fn run_op(
        &mut self,
        op: ManifestOp,
        project_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Before the optimistic commit, not only at the spawn: a blocked
        // run must not leave a run list on screen that was never written
        // and that nothing is going to roll back (the rollback rides on a
        // finished run, and there will not be one).
        if self.mutations_blocked() {
            return;
        }
        if let ManifestOp::ScheduleSet { systems, .. } = &op {
            self.schedule_order = systems.clone();
            self.order_rollback = None;
        }
        let args = op.args();
        self.start_run(project_dir, args, PendingRun::Manifest(op), window, cx);
    }

    // --------------------------------------------------------- submitting

    /// Submit whichever form is open. Bound to Enter and to the form's
    /// button; both are no-ops while a run is in flight or the draft is
    /// invalid.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.form {
            Some(PanelForm::Generate(_)) => self.submit_generate(window, cx),
            Some(PanelForm::Tileset(_)) => self.submit_tileset(cx),
            None => {}
        }
    }

    /// Validate, then spawn `emd generate` off the UI thread.
    ///
    /// **The guard is here, not in the button's `disabled`.** The button
    /// is disabled too, but Enter reaches this directly, and the whole
    /// point of the rule ("never shell out with a name the CLI will
    /// reject") is that there is exactly one gate and it is on the path to
    /// the spawn. `draft.error()` is that gate; the tests assert the fake
    /// runner was never CALLED for a bad draft.
    fn submit_generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        let Some(PanelForm::Generate(form)) = &self.form else {
            return;
        };
        let project_dir = form.project_dir.clone();
        let Some(draft) = self.draft(cx) else {
            return;
        };
        if draft.error().is_some() {
            cx.notify();
            return;
        }
        let pending = PendingRun::Generate {
            kind: draft.kind,
            name: draft.name.clone(),
        };
        self.start_run(project_dir, draft.args(), pending, window, cx);
    }

    /// Spawn one `emd` invocation, under [`EMD_TIMEOUT`], and apply its
    /// result unless a newer run has superseded it.
    ///
    /// **The timeout is a race, not a poll**: the run future and
    /// `background_executor().timer()` go into `smol::future::or`, so
    /// whichever finishes first wins and the loser is DROPPED -- and
    /// dropping the run future kills `emd` (`runner`'s doc explains the
    /// `kill_on_drop` chain). The panel therefore comes back to a usable
    /// state with a real message instead of sitting on "Running…" forever,
    /// and leaves no orphaned `emd` behind. It does NOT stop a `cargo
    /// check` that `emd` had already started -- that grandchild is outside
    /// the kill and runs to completion, still holding the project's
    /// `target/` lock. The executor's timer, not `smol::Timer`, because it is the
    /// one clock the gpui test executor can advance (and the one this
    /// checkout's `clippy.toml` allows).
    fn start_run(
        &mut self,
        project_dir: PathBuf,
        args: Vec<String>,
        pending: PendingRun,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // **The version lock's one guarantee.** The two callers check it
        // too, each for its own reason (no pointless confirm, no orphaned
        // optimistic list), but this is the check that makes "a mutation
        // never runs against a mismatched CLI" true rather than merely
        // likely: every `emd` this panel spawns passes through here, so a
        // future op cannot be added that bypasses the gate. Same rule the
        // draft validation follows -- the gate is on the path to the
        // spawn, not only on the button.
        if self.mutations_blocked() {
            return;
        }
        let request = EmdRequest::emd(project_dir, args);
        self.run_state = RunState::Running {
            command: request.command_line(),
        };
        self.run_generation += 1;
        let generation = self.run_generation;
        let runner = self.runner.clone();
        let timer = cx.background_executor().timer(EMD_TIMEOUT);
        let timed_out = runner::timed_out(&request);
        let run = cx.background_spawn(async move {
            smol::future::or(runner(request), async move {
                timer.await;
                timed_out
            })
            .await
        });
        self._run_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = run.await;
            this.update_in(cx, |this, window, cx| {
                if this.run_generation != generation {
                    return;
                }
                this.finish_run(pending, outcome, window, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Apply a finished run: on success clear the form and refresh
    /// whatever the new artifact affects; on failure keep the form open,
    /// with `emd`'s own message, so the name can be fixed and resubmitted.
    ///
    /// Three outcomes, not two. A non-ok run is either a REVERT -- `emd`
    /// made the change, its `cargo check` rejected it, and it put
    /// everything back -- or a plain failure, where nothing was ever
    /// written. `emd_reverted` reads that off the trailer, and the two
    /// render differently because they mean opposite things about the
    /// state of the project on disk.
    ///
    /// `verify_emd_result` runs first, on every outcome: a trailer whose
    /// own `emd` version is not the one this build negotiated downgrades
    /// the run to a failure rather than being trusted. That same
    /// observation also updates [`EmeraldPanel::lock`], which raises the
    /// banner and gates every further mutation -- the pre-flight and
    /// post-run halves of the lock, meeting here.
    ///
    /// **The two halves compare versions the same way**, which they do not
    /// do by default: `verify_emd_result` is exact string equality while
    /// worldlib's `compare_lock` is prefix-tolerant, and [`lock::lock_error`]
    /// exists to reconcile them (see [`lock`]'s module doc). Without that,
    /// a `0.2.0-rc1` build would sail through the gate and then land here
    /// on every single run.
    fn finish_run(
        &mut self,
        pending: PendingRun,
        outcome: ggo_worldlib::emerald::EmdRunOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Read the trailer's own `emd` version BEFORE `verify_emd_result`
        // rewrites `result.error` into the version-drift message, and
        // decide on it FIRST. `verify_emd_result` leaves `result.reverted`
        // alone, so a run that both reverted and came from the wrong `emd`
        // would otherwise render as "Rolled back — the project no longer
        // compiles: emd version changed mid-session (…)", which is two
        // unrelated causes in one sentence and blames the wrong one. A
        // drifted binary is not a compiler verdict, so it is a plain
        // failure. (E4 owns the banner; this is only which state the
        // mutation path lands in.)
        let drifted = outcome
            .result
            .as_ref()
            .and_then(|r| r.get("emd"))
            .and_then(serde_json::Value::as_str)
            .filter(|got| *got != EXPECTED_EMD_VERSION)
            .map(str::to_string);
        let version_mismatch = drifted.is_some();
        // **The mid-run drift check.** The trailer is a first-hand sighting
        // of the binary that just ran, so it updates the lock immediately
        // rather than waiting for the next poll tick: the banner is up, and
        // every further mutation is gated, by the time the user reads why
        // this one failed. No second `emd version` round trip is needed --
        // the version is right there in the trailer. (The poll still
        // re-checks on its own schedule; a swapped binary has a new mtime,
        // so the probe will confirm or correct this within one interval.)
        if let Some(actual) = drifted {
            self.lock = LockCheck::Reached(actual);
        }
        let outcome = verify_emd_result(outcome, EXPECTED_EMD_VERSION);
        if !outcome.ok {
            let message = emd_error_message(&outcome);
            let reverted = emd_reverted(&outcome) && !version_mismatch;
            let transcript = outcome.output;
            self.run_state = if reverted {
                RunState::Reverted {
                    message,
                    transcript,
                }
            } else {
                RunState::Failed {
                    message,
                    transcript,
                }
            };
            // A revert leaves the manifests exactly as they were and a
            // failure never touched them -- but re-reading is cheap and is
            // the only way the lists cannot drift from what `emd` decided.
            // It is also the ROLLBACK: the optimistic run list goes back
            // to what is on disk, which for both of those outcomes is the
            // order the edit started from.
            self.refresh_manifests(cx);
            if matches!(
                pending,
                PendingRun::Manifest(ManifestOp::ScheduleSet { .. })
            ) {
                self.order_rollback = Some(ORDER_ROLLBACK_NOTICE);
            }
            cx.notify();
            return;
        }
        match pending {
            PendingRun::Generate { kind, name } => {
                self.form = None;
                self.run_state = RunState::Done {
                    message: format!("Created {} {name}", kind.noun().to_lowercase()),
                    transcript: outcome.output,
                };
                self.refresh_manifests(cx);
                self.after_success(kind, outcome.result.as_ref(), window, cx);
            }
            PendingRun::Manifest(op) => {
                self.run_state = RunState::Done {
                    message: op.done_message(),
                    transcript: outcome.output,
                };
                if matches!(op, ManifestOp::Remove { .. }) {
                    self.clear_selection();
                }
                self.refresh_manifests(cx);
                // Every component-shaped op changes the inspector's schema
                // set, exactly as a `generate component` does -- a removed
                // component (or field) must stop being offerable on an
                // entity without reopening the world.
                if component_shaped(&op) {
                    self.after_success(GenKind::Component, None, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Refresh what a successful generate invalidated elsewhere.
    ///
    /// - **Component**: `ggo_world_panel`'s inspector schema set is
    ///   `manifests/components.toml` plus the builtins
    ///   (`ggo_world_panel::loader::manifest_schemas`), and it is read
    ///   ONCE at world-load time. Without this a component created here
    ///   would not be offerable on an entity until the world was closed
    ///   and reopened -- the concrete "restart the editor" failure this
    ///   task exists to remove.
    /// - **World**: `emd` wrote a file a panel owns, so it is opened
    ///   there. The path comes from the run's own JSON trailer
    ///   (`result.path`, absolute), NOT from re-deriving
    ///   `assets/worlds/<name>.toml` here -- `emd generate world` owns
    ///   that convention and this side must not keep a second copy of it.
    /// - The other four kinds write Rust sources under `crates/`, which
    ///   no GGO panel owns; upstream's editor opens them normally.
    ///
    /// Both branches reach ANOTHER panel through the workspace, which is
    /// legal here: this runs from a spawned task's callback, not from
    /// inside a `Workspace` update.
    fn after_success(
        &mut self,
        kind: GenKind,
        result: Option<&serde_json::Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        match kind {
            GenKind::Component => {
                let Some(world_panel) = workspace.read(cx).panel::<WorldPanel>(cx) else {
                    return;
                };
                world_panel.update(cx, |panel, cx| panel.refresh_schemas(cx));
            }
            GenKind::World => {
                let Some(rel) = result
                    .and_then(|r| r.get("path"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|abs| self.worktree_rel(Path::new(abs)))
                else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    ggo_common::open_in_panel(
                        workspace,
                        window,
                        cx,
                        move |panel: &mut WorldPanel, window, cx| {
                            panel.open_rel_path(&rel, window, cx)
                        },
                    );
                });
            }
            _ => {}
        }
    }

    /// An absolute path as a `/`-separated worktree-relative one -- the
    /// frame every GGO panel's `open_rel_path` takes. `None` for a path
    /// outside the worktree.
    fn worktree_rel(&self, abs: &Path) -> Option<String> {
        let root = self.project_root.as_ref()?;
        let under = abs.strip_prefix(root).ok()?;
        Some(
            under
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/"),
        )
    }

    /// Write the blank tileset pair. Synchronous: two small file writes
    /// and a read-back, not a child process.
    fn submit_tileset(&mut self, cx: &mut Context<Self>) {
        let Some(PanelForm::Tileset(form)) = &self.form else {
            return;
        };
        let typed = form.stem.read(cx).text(cx);
        let asset_root = form.asset_root.clone();
        let result = tileset::tileset_rel(&form.under, &typed)
            .and_then(|rel| tileset::create_blank_tileset(&asset_root, &rel).map(|()| rel));
        match result {
            Ok(rel) => {
                self.form = None;
                self.run_state = RunState::Done {
                    message: format!("Created tileset {rel}"),
                    transcript: String::new(),
                };
            }
            Err(message) => {
                if let Some(PanelForm::Tileset(form)) = &mut self.form {
                    form.error = Some(message);
                }
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------- render

    fn render_message(&self, message: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .p_2()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
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

    /// A labelled row: caption on the left, widget on the right.
    fn labelled(caption: &str, child: gpui::AnyElement) -> gpui::AnyElement {
        h_flex()
            .gap_1()
            .items_center()
            .child(
                div().w(px(72.)).child(
                    Label::new(caption.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(child)
            .into_any_element()
    }

    /// The kind selector -- how Resource and Module are reached without
    /// their own menu entries.
    fn render_kind_dropdown(
        &self,
        selected: GenKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for kind in GenKind::ALL {
                let weak = weak.clone();
                menu = menu.entry(SharedString::from(kind.noun()), None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_kind(kind, cx)).ok();
                });
            }
            menu
        });
        DropdownMenu::new(
            "ggo-emerald-kind",
            SharedString::from(selected.noun()),
            menu,
        )
        .into_any_element()
    }

    fn render_field_kind_dropdown(
        &self,
        ix: usize,
        selected: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for kind in FIELD_KINDS {
                let weak = weak.clone();
                menu = menu.entry(SharedString::from(kind), None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_field_kind(ix, kind, cx))
                        .ok();
                });
            }
            menu
        });
        DropdownMenu::new(
            ("ggo-emerald-field-kind", ix),
            SharedString::from(selected.to_string()),
            menu,
        )
        .into_any_element()
    }

    fn render_generate_form(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(PanelForm::Generate(form)) = &self.form else {
            unreachable!("render_generate_form is only called with a generate form open");
        };
        let draft = self.draft(cx).unwrap_or_else(|| GenDraft::new(form.kind));
        let error = draft.error();
        let submittable = error.is_none() && !self.mutations_blocked();

        let mut col = v_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Self::labelled(
                "Kind",
                self.render_kind_dropdown(form.kind, window, cx),
            ))
            .child(Self::labelled(
                "Name",
                Self::editor_input(form.name.clone(), cx),
            ));

        if let Some(stored) = draft.stored_as() {
            col = col.child(
                Label::new(format!("stored as {stored}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        if form.kind.takes_module() {
            col = col.child(Self::labelled(
                "Module",
                Self::editor_input(form.module.clone(), cx),
            ));
            col = col.child(
                Label::new("blank = the shared module")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        if form.kind.takes_fields() {
            for (ix, row) in form.fields.iter().enumerate() {
                let mut field_row = h_flex()
                    .gap_1()
                    .items_center()
                    .child(Self::editor_input(row.name.clone(), cx))
                    .child(self.render_field_kind_dropdown(ix, &row.kind, window, cx));
                if row.kind == ASSET_KIND {
                    field_row = field_row.child(Self::editor_input(row.ext.clone(), cx));
                }
                col = col.child(
                    field_row.child(
                        IconButton::new(("ggo-emerald-field-rm", ix), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_field(ix, cx))),
                    ),
                );
            }
            col = col.child(
                Button::new("ggo-emerald-field-add", "+ Field")
                    .on_click(cx.listener(|this, _, window, cx| this.add_field(window, cx))),
            );
        }
        if let Some(error) = error
            && !draft.pristine()
        {
            col = col.child(Label::new(error).size(LabelSize::Small).color(Color::Error));
        }
        col.child(
            h_flex()
                .gap_1()
                .child(
                    Button::new("ggo-emerald-create", "Create")
                        .disabled(!submittable)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                )
                .child(
                    Button::new("ggo-emerald-cancel", "Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                ),
        )
        .into_any_element()
    }

    fn render_tileset_form(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(PanelForm::Tileset(form)) = &self.form else {
            unreachable!("render_tileset_form is only called with a tileset form open");
        };
        let dir = if form.under.is_empty() {
            "the asset root".to_string()
        } else {
            form.under.clone()
        };
        v_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(format!("New blank tileset in {dir}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(Self::labelled(
                "Name",
                Self::editor_input(form.stem.clone(), cx),
            ))
            .children(
                form.error
                    .clone()
                    .map(|e| Label::new(e).size(LabelSize::Small).color(Color::Error)),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("ggo-emerald-tileset-create", "Create")
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    )
                    .child(
                        Button::new("ggo-emerald-tileset-cancel", "Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_form(cx))),
                    ),
            )
            .into_any_element()
    }

    /// The last run's outcome: one status line plus `emd`'s transcript.
    ///
    /// A REVERT is worded and coloured differently from a failure on
    /// purpose. "failed" invites the user to wonder what state the project
    /// was left in; the revert line answers that in its first clause --
    /// the change was rolled back, nothing is half-applied, and what
    /// follows is the compiler's complaint, not `emd`'s.
    fn render_run_state(&self) -> Option<gpui::AnyElement> {
        let (message, color, transcript) = match &self.run_state {
            RunState::Idle => return None,
            RunState::Running { command } => (format!("Running {command}…"), Color::Muted, ""),
            RunState::Done {
                message,
                transcript,
            } => (message.clone(), Color::Success, transcript.as_str()),
            RunState::Failed {
                message,
                transcript,
            } => (message.clone(), Color::Error, transcript.as_str()),
            RunState::Reverted {
                message,
                transcript,
            } => (
                format!("{REVERTED_PREFIX}{message}"),
                Color::Warning,
                transcript.as_str(),
            ),
        };
        Some(
            v_flex()
                .id("ggo-emerald-run-state")
                .gap_0p5()
                .p_1()
                .overflow_scroll()
                .child(Label::new(message).size(LabelSize::Small).color(color))
                .children((!transcript.is_empty()).then(|| {
                    Label::new(transcript.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                }))
                .into_any_element(),
        )
    }

    /// The version-lock banner: nothing at all when the installed `emd` is
    /// the expected one, and otherwise the line
    /// [`ggo_worldlib::emerald::EmdError`] wrote for this exact drift, plus
    /// (when there is one) the raw spawn error under it, plus this fork's
    /// `GGO_EMD` hint when the drift is about WHERE `emd` was looked for
    /// rather than which version answered.
    ///
    /// Rendered ABOVE the form and the browser rather than beside the run
    /// state, because it is not about a run: it explains why the controls
    /// below it are disabled, so it has to be readable before anything is
    /// clicked. Muted while the first check is in flight, `Warning`
    /// once there is a real mismatch to report -- amber, not red, for the
    /// same reason a revert is: nothing is broken, something is out of
    /// step, and the line says which way.
    fn render_lock_banner(&self) -> Option<gpui::AnyElement> {
        let message = lock::lock_message(&self.lock)?;
        let checking = self.lock == LockCheck::Unchecked;
        let mut col =
            v_flex()
                .gap_0p5()
                .p_1()
                .child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(if checking {
                            Color::Muted
                        } else {
                            Color::Warning
                        }),
                );
        // Both are already `None` while the first check is in flight, and
        // the hint is `None` for a CLI-version drift too -- see
        // [`lock::lock_hint`]. `checking` only decides the colour.
        col = col
            .children(lock::lock_detail(&self.lock).map(|detail| {
                Label::new(detail)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .children(
                lock::lock_hint(&self.lock)
                    .map(|hint| Label::new(hint).size(LabelSize::XSmall).color(Color::Muted)),
            );
        Some(col.into_any_element())
    }

    // ------------------------------------------------- the manifest browser

    /// The three-way tab row.
    fn render_tabs(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut row = h_flex().gap_1().p_1();
        for tab in BrowseTab::ALL {
            row = row.child(
                Button::new(("ggo-emerald-tab", tab as usize), tab.label())
                    .toggle_state(self.tab == tab)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_tab(tab, cx))),
            );
        }
        row.into_any_element()
    }

    /// One manifest row: the name (click to select), and the trash button
    /// that starts a confirmed remove.
    fn render_row(
        &self,
        ix: usize,
        name: &str,
        detail: Option<gpui::AnyElement>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.selected.as_deref() == Some(name);
        let for_click = name.to_string();
        let for_remove = name.to_string();
        let blocked = self.mutations_blocked();
        v_flex()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .id(("ggo-emerald-row", ix))
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name.to_string()).size(LabelSize::Small).color(
                                if selected {
                                    Color::Accent
                                } else {
                                    Color::Default
                                },
                            ))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.select_item(&for_click, cx)),
                            ),
                    )
                    .child(
                        IconButton::new(("ggo-emerald-rm", ix), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .disabled(blocked)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_remove(&for_remove, window, cx)
                            })),
                    ),
            )
            .children(detail)
            .into_any_element()
    }

    /// The selected COMPONENT's fields, each removable, plus the "+ Field"
    /// row that adds one.
    fn render_component_detail(
        &self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(entry) = self.manifests.component(name) else {
            return div().into_any_element();
        };
        let blocked = self.mutations_blocked();
        let mut col = v_flex().gap_0p5().pl_2();
        for (ix, field) in entry.fields.iter().enumerate() {
            let field_name = field.name.clone();
            col = col.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(format!("{}: {}", field.name, field.kind))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        IconButton::new(("ggo-emerald-field-remove", ix), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .disabled(blocked)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_field_remove(&field_name, window, cx)
                            })),
                    ),
            );
        }
        if let Some(row) = &self.field_form {
            let mut form = h_flex()
                .gap_1()
                .items_center()
                .child(Self::editor_input(row.name.clone(), cx))
                .child(self.render_new_field_kind_dropdown(&row.kind, window, cx));
            if row.kind == ASSET_KIND {
                form = form.child(Self::editor_input(row.ext.clone(), cx));
            }
            col = col.child(
                form.child(
                    Button::new("ggo-emerald-field-commit", "Add")
                        .disabled(blocked)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.submit_field_add(window, cx)),
                        ),
                ),
            );
        }
        col.child(
            Button::new("ggo-emerald-field-open", "+ Field")
                .on_click(cx.listener(|this, _, window, cx| this.toggle_field_form(window, cx))),
        )
        .into_any_element()
    }

    fn render_new_field_kind_dropdown(
        &self,
        selected: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for kind in FIELD_KINDS {
                let weak = weak.clone();
                menu = menu.entry(SharedString::from(kind), None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_new_field_kind(kind, cx))
                        .ok();
                });
            }
            menu
        });
        DropdownMenu::new(
            "ggo-emerald-new-field-kind",
            SharedString::from(selected.to_string()),
            menu,
        )
        .into_any_element()
    }

    /// The selected SYSTEM's schedules -- the same
    /// `schedules_using_system` answer the remove confirm quotes, shown
    /// before the user reaches for the trash button rather than only after.
    fn render_system_detail(&self, name: &str) -> gpui::AnyElement {
        let Some(entry) = self.manifests.system(name) else {
            return div().into_any_element();
        };
        let used_by = schedules_using_system(&self.manifests.schedules, &entry.module, &entry.name);
        let text = if used_by.is_empty() {
            "in no schedule".to_string()
        } else {
            format!("in {}", used_by.join(", "))
        };
        div()
            .pl_2()
            .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
            .into_any_element()
    }

    /// The selected SCHEDULE's ordered run list, editable: reorder, drop,
    /// add, and set each entry's cadence.
    ///
    /// It renders [`EmeraldPanel::schedule_order`], NOT the manifest
    /// entry -- that is the whole of the optimistic commit on this side:
    /// while a `schedule set` is in flight the two differ, and this shows
    /// the edit the user just made.
    ///
    /// **Up/Down buttons, not drag-and-drop** -- ggo-ide's own choice for
    /// this list, and the same one for the same reason: worldlib's
    /// `OrderEdit::Move` is exercised identically either way, and a drag
    /// affordance in this dock would be a bespoke widget for a list that
    /// is usually four rows long.
    fn render_schedule_detail(
        &self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.manifests.schedule(name).is_none() {
            return div().into_any_element();
        }
        let blocked = self.mutations_blocked();
        let last = self.schedule_order.len().saturating_sub(1);
        let mut col = v_flex().gap_0p5().pl_2();
        if self.schedule_order.is_empty() {
            col = col.child(
                Label::new("no systems")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        for (ix, entry_ref) in self.schedule_order.iter().enumerate() {
            let (base, cadence) = parse_cadenced_ref(entry_ref);
            // A ref naming a system that is not in the manifest: `emd`
            // would reject the whole `schedule set`, so saying so here is
            // the difference between a puzzling failure and an obvious
            // one. (`emd rm system` leaves these behind by design -- it
            // does not rewrite schedules.)
            let dangling = !self
                .manifests
                .systems
                .iter()
                .any(|s| system_ref(&s.module, &s.name) == base);
            col = col.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(if dangling {
                                format!("{}. {base} (dangling)", ix + 1)
                            } else {
                                format!("{}. {base}", ix + 1)
                            })
                            .size(LabelSize::XSmall)
                            .color(if dangling {
                                Color::Error
                            } else {
                                Color::Muted
                            }),
                        ),
                    )
                    .child(self.render_cadence_dropdown(ix, cadence, window, cx))
                    .child(
                        IconButton::new(("ggo-emerald-order-up", ix), IconName::ChevronUp)
                            .icon_size(IconSize::XSmall)
                            .disabled(blocked || ix == 0)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.move_system(ix, true, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new(("ggo-emerald-order-down", ix), IconName::ChevronDown)
                            .icon_size(IconSize::XSmall)
                            .disabled(blocked || ix >= last)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.move_system(ix, false, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new(("ggo-emerald-order-rm", ix), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .disabled(blocked)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_system(ix, window, cx)
                            })),
                    ),
            );
        }
        if let Some(notice) = self.order_rollback {
            col = col.child(
                Label::new(notice)
                    .size(LabelSize::XSmall)
                    .color(Color::Warning),
            );
        }
        col.child(self.render_add_system_dropdown(window, cx))
            .into_any_element()
    }

    /// One run-list row's cadence picker. The label is the entry's CURRENT
    /// cadence even when [`CADENCES`] does not offer it.
    fn render_cadence_dropdown(
        &self,
        ix: usize,
        cadence: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for n in CADENCES {
                let weak = weak.clone();
                menu = menu.entry(
                    SharedString::from(ops::every(n)),
                    None,
                    move |window, cx| {
                        weak.update(cx, |this, cx| this.set_cadence(ix, n, window, cx))
                            .ok();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(
            ("ggo-emerald-cadence", ix),
            SharedString::from(ops::every(cadence)),
            menu,
        )
        .disabled(self.mutations_blocked())
        .into_any_element()
    }

    /// The "+ System" picker: every system NOT already in this run list
    /// ([`available_systems`]), so adding a duplicate is unreachable
    /// rather than merely rejected. Picking one commits immediately --
    /// there is no second "Add" step to forget.
    fn render_add_system_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let available = available_systems(&self.manifests.systems, &self.schedule_order);
        if available.is_empty() {
            return Label::new("every system is already in this schedule")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element();
        }
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for system in &available {
                let weak = weak.clone();
                let sref = system_ref(&system.module, &system.name);
                let label = sref.clone();
                menu = menu.entry(SharedString::from(label), None, move |window, cx| {
                    let sref = sref.clone();
                    weak.update(cx, |this, cx| this.add_system(&sref, window, cx))
                        .ok();
                });
            }
            menu
        });
        DropdownMenu::new(
            "ggo-emerald-add-system",
            SharedString::from("+ System"),
            menu,
        )
        .disabled(self.mutations_blocked())
        .into_any_element()
    }

    /// The active tab's manifest, grouped by module (shared first, then
    /// alphabetical -- `group_by_module`'s own order), with the selected
    /// row's detail inline.
    fn render_browser(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if self.manifests.is_empty() {
            return None;
        }
        let mut col = v_flex()
            .id("ggo-emerald-browser")
            .gap_0p5()
            .p_1()
            .overflow_scroll()
            .child(self.render_tabs(cx));
        // One counter across the whole tab so every row's element id is
        // unique even when two modules hold a same-named entry.
        let mut ix = 0;
        let groups: Vec<(String, Vec<String>)> = match self.tab {
            BrowseTab::Components => group_by_module(self.manifests.components.clone())
                .into_iter()
                .map(|g| (g.module, g.items.into_iter().map(|c| c.name).collect()))
                .collect(),
            BrowseTab::Systems => group_by_module(self.manifests.systems.clone())
                .into_iter()
                .map(|g| (g.module, g.items.into_iter().map(|s| s.name).collect()))
                .collect(),
            BrowseTab::Schedules => group_by_module(self.manifests.schedules.clone())
                .into_iter()
                .map(|g| (g.module, g.items.into_iter().map(|s| s.name).collect()))
                .collect(),
        };
        for (module, names) in groups {
            col = col.child(
                Label::new(if module.is_empty() {
                    SHARED_MODULE_LABEL.to_string()
                } else {
                    module
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
            for name in names {
                let detail =
                    (self.selected.as_deref() == Some(name.as_str())).then(|| match self.tab {
                        BrowseTab::Components => self.render_component_detail(&name, window, cx),
                        BrowseTab::Systems => self.render_system_detail(&name),
                        BrowseTab::Schedules => self.render_schedule_detail(&name, window, cx),
                    });
                col = col.child(self.render_row(ix, &name, detail, cx));
                ix += 1;
            }
        }
        Some(col.into_any_element())
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let banner = self.render_lock_banner();
        let form = match &self.form {
            Some(PanelForm::Generate(_)) => Some(self.render_generate_form(window, cx)),
            Some(PanelForm::Tileset(_)) => Some(self.render_tileset_form(cx)),
            None => None,
        };
        let browser = self.render_browser(window, cx);
        let run_state = self.render_run_state();
        // The banner is deliberately NOT part of this emptiness test: an
        // unmanaged project still needs the "right-click a directory" text,
        // and a lock banner is never a substitute for it -- they answer
        // different questions and both can be true at once.
        if form.is_none() && browser.is_none() && run_state.is_none() {
            return v_flex()
                .size_full()
                .children(banner)
                .child(self.render_message(EMPTY_MESSAGE.to_string(), cx))
                .into_any_element();
        }
        v_flex()
            .size_full()
            .children(banner)
            .children(form)
            .children(browser)
            .children(run_state)
            .into_any_element()
    }
}

/// A fresh empty single-line editor.
fn single_line(window: &mut Window, cx: &mut Context<EmeraldPanel>) -> Entity<Editor> {
    cx.new(|cx| Editor::single_line(window, cx))
}

/// Does `op` change what components (or their fields) exist -- i.e. does
/// `ggo_world_panel`'s inspector schema set need re-reading afterwards?
fn component_shaped(op: &ManifestOp) -> bool {
    match op {
        ManifestOp::Remove { kind, .. } => *kind == ManifestKind::Component,
        ManifestOp::FieldAdd { .. } | ManifestOp::FieldRemove { .. } => true,
        // A run list says nothing about what components exist.
        ManifestOp::ScheduleSet { .. } => false,
    }
}

impl Render for EmeraldPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.render_body(window, cx);
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Submit, window, cx| this.submit(window, cx)))
            .bg(cx.theme().colors().panel_background)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for EmeraldPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for EmeraldPanel {}

impl Panel for EmeraldPanel {
    fn persistent_name() -> &'static str {
        "GGO Emerald"
    }

    fn panel_key() -> &'static str {
        GGO_EMERALD_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call as every other GGO panel: no settings persistence yet.
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
        // This panel SCAFFOLDS things, and `ToolHammer` is the only
        // build/make-shaped glyph in this checkout (grep the `IconName`
        // enum: no wand, no gem, no factory). No other panel uses it as a
        // dock icon; `Blocks` is `ggo_tileset_panel`, `Public` is
        // `ggo_world_panel`, `Image` is `ggo_sprite_panel`, `SquareDot` is
        // `ggo_map_panel`.
        Some(IconName::ToolHammer)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO Emerald")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8, `ggo_sprite_panel` 9,
        // `ggo_charts_panel` 10, `ggo_emu_panel` 11,
        // `ggo_tileset_panel` 12, `ggo_import_panel` 13,
        // `ggo_map_panel` 14 (grep activation_priority across crates/).
        15
    }

    // No `prepare_to_close`: this panel holds no document. A half-typed
    // form is not an unsaved edit -- nothing has been written and nothing
    // can be lost by closing the dock.

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_root` has to READ that
            // same workspace entity -- reading it re-entrantly panics
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
    use std::sync::{Arc, Mutex};

    use ggo_worldlib::emerald::{
        EmdRunOutcome, FieldEntry, build_generate_component_args, emd_run_outcome,
    };
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
    /// something renders `Workspace::actions` (same lesson as the other
    /// GGO panels').
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
                workspace.panel::<EmeraldPanel>(cx).is_some(),
                "EmeraldPanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<EmeraldPanel>(cx)
                .expect("EmeraldPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    // ------------------------------------------------------- the fake emd

    /// An [`EmdRunner`] that never spawns anything, plus the log of every
    /// request it was handed.
    ///
    /// This is the whole reason the runner is an injected `Fn` rather than
    /// a call to `Command::new` inline: "validation rejected this before
    /// any spawn" is asserted as `calls.is_empty()`, which no amount of
    /// message-matching could establish, and the success/failure paths run
    /// in CI with no `emd` on `PATH`.
    fn fake_runner(
        reply: impl Fn(&EmdRequest) -> EmdRunOutcome + Send + Sync + 'static,
    ) -> (EmdRunner, Arc<Mutex<Vec<EmdRequest>>>) {
        let calls: Arc<Mutex<Vec<EmdRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let runner: EmdRunner = Arc::new(move |request: EmdRequest| {
            recorded.lock().unwrap().push(request.clone());
            let outcome = reply(&request);
            Box::pin(async move { outcome })
        });
        (runner, calls)
    }

    /// A runner whose run NEVER finishes -- the stand-in for a `cargo
    /// check` that has wedged. Only [`EMD_TIMEOUT`] can end a run started
    /// with this.
    fn hung_runner() -> (EmdRunner, Arc<Mutex<Vec<EmdRequest>>>) {
        let calls: Arc<Mutex<Vec<EmdRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let runner: EmdRunner = Arc::new(move |request: EmdRequest| {
            recorded.lock().unwrap().push(request);
            Box::pin(std::future::pending())
        });
        (runner, calls)
    }

    /// A successful run's transcript, shaped like the real thing: a plain
    /// JSON line then the `emd-json:` trailer (verified against
    /// `emd 0.2.0`).
    fn ok_outcome(path: &str) -> EmdRunOutcome {
        emd_run_outcome(
            true,
            &[format!(
                "emd-json: {{\"emd\":\"0.2.0\",\"ok\":true,\"path\":\"{path}\"}}"
            )],
        )
    }

    /// A failing run, with `emd`'s own error in the trailer.
    fn err_outcome(message: &str) -> EmdRunOutcome {
        emd_run_outcome(
            false,
            &[format!(
                "emd-json: {{\"emd\":\"0.2.0\",\"ok\":false,\"error\":\"{message}\"}}"
            )],
        )
    }

    // -------------------------------------------------------- the fixture

    /// A real-fs emerald project: `emerald.toml`, an empty `manifests/`,
    /// an `assets/` tree, and a `crates/` directory that must NOT get any
    /// menu entries.
    fn emerald_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("emerald.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        std::fs::create_dir_all(root.join("assets/worlds")).unwrap();
        std::fs::create_dir_all(root.join("assets/tiles")).unwrap();
        std::fs::create_dir_all(root.join("crates/game-core/src")).unwrap();
        std::fs::write(root.join("manifests/components.toml"), "version = 1\n").unwrap();
        dir
    }

    /// [`emerald_project`] with all three manifests populated and two
    /// worlds placing components -- the fixture every browse/remove test
    /// works from.
    fn populated_project() -> tempfile::TempDir {
        let dir = emerald_project();
        let root = dir.path();
        std::fs::write(
            root.join("manifests/components.toml"),
            "version = 1\n\
             [[component]]\nname = \"HeroUnit\"\nmodule = \"gameplay\"\n\
             [[component.field]]\nname = \"hp\"\nkind = \"int\"\n\
             [[component]]\nname = \"Marker\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("manifests/systems.toml"),
            "version = 1\n\
             [[system]]\nname = \"spawn_enemies\"\nmodule = \"gameplay\"\n\
             [[system]]\nname = \"tick_clock\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("manifests/schedules.toml"),
            "version = 1\n\
             [[schedule]]\nname = \"update\"\nsystems = [\"gameplay/spawn_enemies\", \"tick_clock\"]\n\
             [[schedule]]\nname = \"render\"\nsystems = [\"gameplay/spawn_enemies@4\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("assets/worlds/arena.toml"),
            "[[entity]]\nHeroUnit = { hp = 3 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("assets/worlds/empty.toml"),
            "[[entity]]\nMarker = {}\n",
        )
        .unwrap();
        dir
    }

    /// A window with the app state a form's `Editor`s need, but no
    /// workspace -- everything but the two cross-panel refreshes works
    /// from here.
    fn empty_window(cx: &mut TestAppContext) -> &mut gpui::VisualTestContext {
        cx.update(|cx| {
            AppState::test(cx);
        });
        cx.add_empty_window()
    }

    /// A [`BinProbe`] that stands in for "the `emd` binary, unchanged" --
    /// an arbitrary fixed instant, so a probe of it never differs from
    /// itself.
    fn settled_probe() -> BinProbe {
        BinProbe::At(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000))
    }

    /// The panel alone, pointed at `root` with no workspace, with a landed
    /// version-lock check that matches.
    ///
    /// **The lock is seeded, not driven.** Every mutation is gated on it
    /// (`lock::mutations_enabled`), so a fixture that left it `Unchecked`
    /// would turn every test in this module into a test of the gate. The
    /// probe is frozen to match `emd_probe`, so the poll loop's first tick
    /// sees no change and never spends a runner call -- which is what keeps
    /// the `calls` log in every other test a log of MUTATIONS only.
    /// [`locked_panel`] is the constructor for the lock's own tests.
    fn lone_panel(
        cx: &mut gpui::VisualTestContext,
        root: &std::path::Path,
        runner: EmdRunner,
    ) -> Entity<EmeraldPanel> {
        locked_panel(
            cx,
            root,
            runner,
            LockCheck::Reached(EXPECTED_EMD_VERSION.to_string()),
        )
    }

    /// [`lone_panel`] with the version lock in a chosen state.
    fn locked_panel(
        cx: &mut gpui::VisualTestContext,
        root: &std::path::Path,
        runner: EmdRunner,
        lock: LockCheck,
    ) -> Entity<EmeraldPanel> {
        let root = root.to_path_buf();
        cx.update(|_window, cx| {
            cx.new(|cx| {
                let mut panel = EmeraldPanel::new(None, cx);
                panel.root_override = Some(root);
                panel.runner = runner;
                panel.lock = lock;
                panel.emd_probe = settled_probe();
                panel.probe = Arc::new(settled_probe);
                panel
            })
        })
    }

    fn generate_form(
        panel: &Entity<EmeraldPanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> (Entity<Editor>, Entity<Editor>) {
        panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Generate(form)) => (form.name.clone(), form.module.clone()),
            _ => panic!("expected an open generate form"),
        })
    }

    fn type_into(editor: &Entity<Editor>, text: &str, cx: &mut gpui::VisualTestContext) {
        editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    }

    fn run_state_message(panel: &Entity<EmeraldPanel>, cx: &mut gpui::VisualTestContext) -> String {
        panel.read_with(cx, |panel, _| match &panel.run_state {
            RunState::Idle => "idle".to_string(),
            RunState::Running { command } => format!("running {command}"),
            RunState::Done { message, .. } => format!("done {message}"),
            RunState::Failed { message, .. } => format!("failed {message}"),
            RunState::Reverted { message, .. } => format!("reverted {message}"),
        })
    }

    // ------------------------------------------------------- argv + spawn

    /// The argv the component form spawns must be EXACTLY worldlib's
    /// builder output plus `--json`, and it must run in the emerald
    /// project root -- `emd` discovers the project from its cwd, so that
    /// is what decides which project is written to.
    #[gpui::test]
    async fn test_new_component_spawns_worldlibs_argv_in_the_project_root(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/hero_unit.rs"));
        let panel = lone_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::Component, "manifests", window, cx);
                panel.add_field(window, cx);
                panel.add_field(window, cx);
                panel.select_field_kind(1, ASSET_KIND, cx);
            })
        });
        let (name, module) = generate_form(&panel, cx);
        type_into(&name, "hero_unit", cx);
        type_into(&module, "gameplay", cx);
        let fields = panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Generate(form)) => form
                .fields
                .iter()
                .map(|row| (row.name.clone(), row.ext.clone()))
                .collect::<Vec<_>>(),
            _ => panic!("expected a generate form"),
        });
        type_into(&fields[0].0, "hp", cx);
        type_into(&fields[1].0, "art", cx);
        type_into(&fields[1].1, "png", cx);

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one spawn");
        let mut expected = build_generate_component_args(
            "hero_unit",
            "gameplay",
            &[
                FieldEntry {
                    name: "hp".into(),
                    kind: "int".into(),
                },
                FieldEntry {
                    name: "art".into(),
                    kind: "asset:png".into(),
                },
            ],
        );
        expected.push("--json".to_string());
        assert_eq!(calls[0].args, expected);
        assert_eq!(calls[0].cwd, dir.path());
        drop(calls);

        assert!(
            run_state_message(&panel, cx).starts_with("done Created component hero_unit"),
            "{}",
            run_state_message(&panel, cx)
        );
        assert!(
            panel.read_with(cx, |panel, _| panel.form.is_none()),
            "a successful run closes the form"
        );
    }

    /// Every kind's argv, through the panel rather than through
    /// [`forms`]'s unit tests -- so the "which editors does this kind
    /// read" wiring is covered too, including that a `--module` typed for
    /// a Component does not survive a switch to World.
    #[gpui::test]
    async fn test_each_kind_spawns_its_own_subcommand(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/thing"));
        let panel = lone_panel(cx, dir.path(), runner);

        for (kind, expected) in [
            (
                GenKind::System,
                vec![
                    "generate", "system", "thing", "--module", "gameplay", "--json",
                ],
            ),
            (
                GenKind::Resource,
                vec![
                    "generate", "resource", "thing", "--module", "gameplay", "--json",
                ],
            ),
            (
                GenKind::Schedule,
                vec![
                    "generate", "schedule", "thing", "--module", "gameplay", "--json",
                ],
            ),
            (
                GenKind::Module,
                vec!["generate", "module", "thing", "--json"],
            ),
            (GenKind::World, vec!["generate", "world", "thing", "--json"]),
        ] {
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.new_item(kind, "manifests", window, cx)
                })
            });
            let (name, module) = generate_form(&panel, cx);
            type_into(&name, "thing", cx);
            type_into(&module, "gameplay", cx);
            cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
            cx.run_until_parked();
            assert_eq!(
                calls.lock().unwrap().last().unwrap().args,
                expected,
                "{kind:?}"
            );
        }
        assert_eq!(calls.lock().unwrap().len(), 5);
    }

    /// **The gate.** A name `emd` would reject must never reach the
    /// runner -- asserted as a call COUNT, not as a message. Enter is used
    /// rather than the button, because the button is merely `disabled`
    /// while the keybinding reaches `submit` directly, which is exactly
    /// the hole this guard exists to close.
    #[gpui::test]
    async fn test_an_invalid_draft_never_reaches_the_runner(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/nope.rs"));
        let panel = lone_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::Component, "manifests", window, cx)
            })
        });
        let (name, module) = generate_form(&panel, cx);

        // A bad name.
        for bad in ["", "Bad-Name", "9lives", "hero unit", "_"] {
            type_into(&name, bad, cx);
            cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
            cx.run_until_parked();
            assert!(calls.lock().unwrap().is_empty(), "{bad:?} must not spawn");
        }
        // A good name, but a bad module.
        type_into(&name, "hero_unit", cx);
        type_into(&module, "Bad-Mod", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();
        assert!(
            calls.lock().unwrap().is_empty(),
            "a bad module must not spawn"
        );

        // A good name and module, but a bad field.
        type_into(&module, "", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.add_field(window, cx)));
        cx.update(|_window, cx| {
            panel.update(cx, |panel, cx| panel.select_field_kind(0, ASSET_KIND, cx))
        });
        let field = panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Generate(form)) => form.fields[0].name.clone(),
            _ => panic!("expected a generate form"),
        });
        type_into(&field, "art", cx); // asset with no extension
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();
        assert!(
            calls.lock().unwrap().is_empty(),
            "an incomplete asset field must not spawn"
        );

        assert!(
            panel.read_with(cx, |panel, _| panel.form.is_some()),
            "the form stays open so the mistake can be fixed"
        );
        assert_eq!(run_state_message(&panel, cx), "idle");
    }

    /// A non-zero exit surfaces `emd`'s own error text and LEAVES THE FORM
    /// OPEN -- the name is usually one character away from working, and
    /// throwing the draft away to retype it would be the wrong answer.
    #[gpui::test]
    async fn test_a_failing_run_keeps_the_form_open_with_emds_message(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| err_outcome("file already exists: hero_unit.rs"));
        let panel = lone_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::Component, "manifests", window, cx)
            })
        });
        let (name, _) = generate_form(&panel, cx);
        type_into(&name, "hero_unit", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();

        assert_eq!(calls.lock().unwrap().len(), 1, "it really did spawn");
        assert_eq!(
            run_state_message(&panel, cx),
            "failed file already exists: hero_unit.rs"
        );
        assert!(
            panel.read_with(cx, |panel, _| panel.form.is_some()),
            "a failed run keeps the form"
        );
    }

    /// A run that fails with NO parseable trailer -- a usage error, a
    /// panic, an `emd` too old to print one -- must still fail visibly,
    /// falling back to the captured transcript rather than reporting
    /// success or saying nothing.
    #[gpui::test]
    async fn test_a_malformed_trailer_still_fails_visibly(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let cx = empty_window(cx);
        let (runner, _) = fake_runner(|_| {
            emd_run_outcome(
                false,
                &[
                    "error: unexpected argument".to_string(),
                    "emd-json: {not json at all".to_string(),
                ],
            )
        });
        let panel = lone_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::System, "manifests", window, cx)
            })
        });
        let (name, _) = generate_form(&panel, cx);
        type_into(&name, "spawn_enemies", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();

        let message = run_state_message(&panel, cx);
        assert!(message.starts_with("failed"), "{message}");
        assert!(
            message.contains("unexpected argument"),
            "the transcript is the fallback message: {message}"
        );
        assert!(panel.read_with(cx, |panel, _| panel.form.is_some()));
    }

    /// A superseded run's result is dropped rather than applied -- the
    /// generation guard every GGO panel carries.
    #[gpui::test]
    async fn test_a_superseded_run_is_dropped(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let cx = empty_window(cx);
        let (runner, _) = fake_runner(|req| {
            if req.args.contains(&"first".to_string()) {
                err_outcome("stale")
            } else {
                ok_outcome("/x/second")
            }
        });
        let panel = lone_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::Module, "manifests", window, cx)
            })
        });
        let (name, _) = generate_form(&panel, cx);
        type_into(&name, "first", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        // Bump the generation before the first run's result is applied.
        panel.update(cx, |panel, _| panel.run_generation += 1);
        cx.run_until_parked();
        assert!(
            run_state_message(&panel, cx).starts_with("running"),
            "a stale result must not overwrite the run state: {}",
            run_state_message(&panel, cx)
        );
    }

    // ----------------------------------------------- the manifest browser

    /// A panel pointed at `root` with its manifests loaded, as a browse
    /// test needs it (production reaches `refresh_root` via panel
    /// activation or a context-menu click).
    fn browsing_panel(
        cx: &mut gpui::VisualTestContext,
        root: &std::path::Path,
        runner: EmdRunner,
    ) -> Entity<EmeraldPanel> {
        let panel = lone_panel(cx, root, runner);
        panel.update(cx, |panel, cx| panel.refresh_root(cx));
        panel
    }

    /// The three lists, read from the manifests and grouped by module --
    /// shared bucket first, then alphabetically (`group_by_module`'s own
    /// order).
    #[gpui::test]
    async fn test_the_browser_lists_all_three_manifests_grouped_by_module(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/never"));
        let panel = browsing_panel(cx, dir.path(), runner);

        panel.read_with(cx, |panel, _| {
            let components = group_by_module(panel.manifests.components.clone());
            assert_eq!(components.len(), 2);
            assert_eq!(components[0].module, "", "the shared bucket sorts first");
            assert_eq!(components[0].items[0].name, "Marker");
            assert_eq!(components[1].module, "gameplay");
            assert_eq!(components[1].items[0].name, "HeroUnit");
            assert_eq!(panel.manifests.systems.len(), 2);
            assert_eq!(panel.manifests.schedules.len(), 2);
        });

        // Selection is per-tab: names are only unique within a manifest,
        // so switching tabs drops it.
        panel.update(cx, |panel, cx| {
            panel.select_item("HeroUnit", cx);
            assert_eq!(panel.selected_component().unwrap().1, "gameplay");
            panel.select_tab(BrowseTab::Systems, cx);
            assert_eq!(panel.selected, None);
            assert_eq!(
                panel.entry_module("spawn_enemies").as_deref(),
                Some("gameplay")
            );
            assert_eq!(
                panel.entry_module("tick_clock").as_deref(),
                Some(""),
                "a shared item has a module, and it is the empty one"
            );
            assert_eq!(panel.entry_module("HeroUnit"), None, "wrong tab");
        });
        assert!(calls.lock().unwrap().is_empty(), "browsing runs nothing");
    }

    // -------------------------------------------------- cascade + removes

    /// **The task's centre.** Removing a system names the schedules that
    /// reference it -- in the prompt, before anything runs -- and then
    /// spawns exactly worldlib's `build_rm_args` argv.
    #[gpui::test]
    async fn test_removing_a_system_names_its_schedules_then_spawns_worldlibs_argv(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/gone"));
        let panel = browsing_panel(cx, dir.path(), runner);

        panel.update(cx, |panel, cx| panel.select_tab(BrowseTab::Systems, cx));
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_item("spawn_enemies", cx);
                panel.request_remove("spawn_enemies", window, cx);
            })
        });

        let (message, detail) = cx.pending_prompt().expect("a remove always confirms");
        assert_eq!(message, "Remove the system gameplay/spawn_enemies?");
        assert!(
            detail.contains("Also removed from 2 schedules: update, render."),
            "the cascade must be in the confirm body: {detail}"
        );
        assert!(
            detail.contains("This cannot be undone."),
            "and the cascade must not have displaced the standard warning: {detail}"
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "nothing runs while the prompt is up"
        );

        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one spawn");
        let mut expected =
            ggo_worldlib::emerald::build_rm_args(ManifestKind::System, "spawn_enemies", "gameplay");
        expected.push("--json".to_string());
        assert_eq!(calls[0].args, expected);
        assert_eq!(calls[0].cwd, dir.path());
        drop(calls);

        assert_eq!(
            run_state_message(&panel, cx),
            "done Removed system gameplay/spawn_enemies"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.selected.clone()),
            None,
            "the removed row's selection goes with it"
        );
    }

    /// Cancel is asserted as a call COUNT, which is the only assertion that
    /// can prove nothing was run -- no message check could.
    #[gpui::test]
    async fn test_cancelling_a_remove_spawns_nothing(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/never"));
        let panel = browsing_panel(cx, dir.path(), runner);

        for (tab, name) in [
            (BrowseTab::Systems, "spawn_enemies"),
            (BrowseTab::Components, "HeroUnit"),
            (BrowseTab::Schedules, "update"),
        ] {
            panel.update(cx, |panel, cx| panel.select_tab(tab, cx));
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| panel.request_remove(name, window, cx))
            });
            assert!(cx.has_pending_prompt(), "{name} must confirm");
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            assert!(
                calls.lock().unwrap().is_empty(),
                "Cancel must not run anything ({name})"
            );
            assert_eq!(run_state_message(&panel, cx), "idle");
        }
    }

    /// The component blast radius: the worlds that still place it, named in
    /// the prompt, together with the plain statement of what the scan does
    /// NOT cover.
    #[gpui::test]
    async fn test_removing_a_component_names_the_worlds_that_place_it(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/gone"));
        let panel = browsing_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.request_remove("HeroUnit", window, cx))
        });
        let (message, detail) = cx.pending_prompt().unwrap();
        assert_eq!(message, "Remove the component gameplay/HeroUnit?");
        assert!(
            detail.contains("Still placed in 1 world: worlds/arena.toml."),
            "{detail}"
        );
        assert!(
            detail.contains(ops::CODE_SCAN_NOTE),
            "the limit of the scan is stated, not implied: {detail}"
        );
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0].args,
            [
                "rm",
                "component",
                "HeroUnit",
                "--module",
                "gameplay",
                "--json"
            ]
        );
    }

    /// Adding a field is the one manifest op with no confirm (it only ever
    /// adds); removing one confirms like everything else. Both spawn
    /// worldlib's builder argv verbatim.
    #[gpui::test]
    async fn test_field_add_and_remove_spawn_worldlibs_argv(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/field"));
        let panel = browsing_panel(cx, dir.path(), runner);

        // Add: type into the "+ Field" row and commit.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_item("HeroUnit", cx);
                panel.toggle_field_form(window, cx);
            })
        });
        let field = panel.read_with(cx, |panel, _| {
            panel.field_form.as_ref().unwrap().name.clone()
        });
        type_into(&field, "armor", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit_field_add(window, cx)));
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "adding a field is not destructive, so it does not confirm"
        );
        assert_eq!(
            calls.lock().unwrap()[0].args,
            ggo_worldlib::emerald::build_field_add_args("HeroUnit", "gameplay", "armor:int")
                .into_iter()
                .chain(["--json".to_string()])
                .collect::<Vec<_>>()
        );

        // An invalid spec never reaches the runner (an asset field with no
        // extension -- the same gate the generate form applies).
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_new_field_kind(ASSET_KIND, cx);
                panel.submit_field_add(window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1, "still just the good one");

        // Remove: confirms, then runs `component field rm`.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.request_field_remove("hp", window, cx))
        });
        let (message, detail) = cx.pending_prompt().unwrap();
        assert_eq!(message, "Remove the field hp from gameplay/HeroUnit?");
        assert!(detail.contains("compiler check"), "{detail}");
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap()[1].args,
            ggo_worldlib::emerald::build_field_rm_args("HeroUnit", "gameplay", "hp")
                .into_iter()
                .chain(["--json".to_string()])
                .collect::<Vec<_>>()
        );
        assert_eq!(
            run_state_message(&panel, cx),
            "done Removed field hp from HeroUnit"
        );
    }

    // ------------------------------------------------- revert vs failure

    /// A REVERT is not a failure, and must not read like one: `emd` applied
    /// the change, its `cargo check` rejected it, and it put everything
    /// back. The two states are distinct and the revert says what happened
    /// to the project on disk.
    #[gpui::test]
    async fn test_a_reverted_run_reads_differently_from_a_plain_failure(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, _) = fake_runner(|req| {
            // `emd component field rm` reverts; `emd rm system` here just
            // fails outright.
            if req.args.contains(&"field".to_string()) {
                emd_run_outcome(
                    false,
                    &[
                        "emd-json: {\"emd\":\"0.2.0\",\"ok\":false,\"reverted\":true,\
                       \"error\":\"error[E0609]: no field `hp`\"}"
                            .to_string(),
                    ],
                )
            } else {
                err_outcome("no such system")
            }
        });
        let panel = browsing_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_item("HeroUnit", cx);
                panel.request_field_remove("hp", window, cx);
            })
        });
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();

        let reverted = run_state_message(&panel, cx);
        assert!(
            reverted.starts_with("reverted error[E0609]"),
            "a rolled-back run is its own state: {reverted}"
        );
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(&panel.run_state, RunState::Reverted { .. }),
                "not Failed"
            );
        });

        // The same panel, a genuine failure: a different state, and the
        // rolled-back wording is nowhere near it.
        panel.update(cx, |panel, cx| panel.select_tab(BrowseTab::Systems, cx));
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.request_remove("tick_clock", window, cx)
            })
        });
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(run_state_message(&panel, cx), "failed no such system");
        panel.read_with(cx, |panel, _| {
            assert!(matches!(&panel.run_state, RunState::Failed { .. }));
            // The rendered line, not just the variant: the revert leads
            // with what happened to the project, the failure does not.
            assert!(REVERTED_PREFIX.contains("Rolled back"));
        });
    }

    // ------------------------------------------------------------ timeout

    /// A run that never finishes must not strand the panel on "Running…".
    /// At [`EMD_TIMEOUT`] the race's timer arm wins, the run future (and
    /// with it the child) is dropped, and the panel reports it and accepts
    /// work again.
    #[gpui::test]
    async fn test_a_hung_run_times_out_and_leaves_the_panel_usable(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (hung, hung_calls) = hung_runner();
        let panel = browsing_panel(cx, dir.path(), hung);

        panel.update(cx, |panel, cx| panel.select_tab(BrowseTab::Schedules, cx));
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.request_remove("render", window, cx))
        });
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(hung_calls.lock().unwrap().len(), 1, "it really started");
        assert!(
            run_state_message(&panel, cx).starts_with("running"),
            "still in flight just before the deadline"
        );

        // One second short of the budget: still running, so the timeout is
        // a real deadline rather than "the next tick".
        cx.executor()
            .advance_clock(EMD_TIMEOUT - std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(run_state_message(&panel, cx).starts_with("running"));

        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        let message = run_state_message(&panel, cx);
        assert!(
            message.starts_with("failed timed out after 10 minutes and was killed"),
            "{message}"
        );
        assert!(
            message.contains("rm schedule render"),
            "the report names the command that was killed: {message}"
        );

        // And the panel takes work again -- the whole point of not being
        // stuck on "Running…".
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/gone"));
        panel.update(cx, |panel, _| panel.runner = runner);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.request_remove("update", window, cx))
        });
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(
            run_state_message(&panel, cx),
            "done Removed schedule update"
        );
    }

    /// A mutation cannot be started while one is in flight -- including
    /// from a confirm that was already on screen when the other run began.
    #[gpui::test]
    async fn test_a_second_mutation_cannot_start_mid_run(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (hung, calls) = hung_runner();
        let panel = browsing_panel(cx, dir.path(), hung);

        panel.update(cx, |panel, cx| panel.select_tab(BrowseTab::Systems, cx));
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.request_remove("tick_clock", window, cx)
            })
        });
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.request_remove("spawn_enemies", window, cx)
            })
        });
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "a run is in flight -- the second remove does not even confirm"
        );
        assert_eq!(calls.lock().unwrap().len(), 1, "and nothing else spawned");
    }

    // ------------------------------------------------ the schedule run list

    /// A panel with `name` selected on the Schedules tab -- the state
    /// every run-list edit starts from.
    fn schedule_panel(
        cx: &mut gpui::VisualTestContext,
        root: &std::path::Path,
        runner: EmdRunner,
        name: &str,
    ) -> Entity<EmeraldPanel> {
        let panel = browsing_panel(cx, root, runner);
        panel.update(cx, |panel, cx| {
            panel.select_tab(BrowseTab::Schedules, cx);
            panel.select_item(name, cx);
        });
        panel
    }

    fn order(panel: &Entity<EmeraldPanel>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, _| panel.schedule_order.clone())
    }

    /// The run list a spawned `schedule set` actually carries -- read back
    /// off the argv, so the assertions below compare what `emd` was told
    /// with what `apply_order_edit` said it should be told.
    fn spawned_systems(request: &EmdRequest) -> Vec<String> {
        let at = request
            .args
            .iter()
            .position(|a| a == "--systems")
            .unwrap_or_else(|| panic!("no --systems in {:?}", request.args));
        request.args[at + 1]
            .split(',')
            .map(str::to_string)
            .collect()
    }

    /// `build_schedule_set_args` plus the `--json` the request appends --
    /// what every edit's argv must equal exactly.
    fn expected_set_argv(name: &str, module: &str, systems: &[String]) -> Vec<String> {
        ggo_worldlib::emerald::build_schedule_set_args(name, module, systems)
            .into_iter()
            .chain(["--json".to_string()])
            .collect()
    }

    /// **The task's centre.** Each of the four edits computes the list
    /// worldlib's pure helpers say it should and spawns worldlib's own
    /// argv for it -- no hand-rolled splice, no hand-written vector on
    /// either side of the assertion.
    #[gpui::test]
    async fn test_reorder_add_remove_and_cadence_each_commit_worldlibs_list(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/update"));
        let panel = schedule_panel(cx, dir.path(), runner, "update");

        let before = order(&panel, cx);
        assert_eq!(before, ["gameplay/spawn_enemies", "tick_clock"]);

        // MOVE: "tick_clock" up one.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.move_system(1, true, window, cx))
        });
        cx.run_until_parked();
        let moved = apply_order_edit(&before, OrderEdit::Move { from: 1, to: 0 });
        assert_eq!(calls.lock().unwrap().len(), 1, "exactly one spawn");
        assert_eq!(spawned_systems(&calls.lock().unwrap()[0]), moved);
        assert_eq!(
            calls.lock().unwrap()[0].args,
            expected_set_argv("update", "", &moved)
        );
        assert_eq!(calls.lock().unwrap()[0].cwd, dir.path());
        assert_eq!(
            run_state_message(&panel, cx),
            "done Moved tick_clock to position 1 in schedule update"
        );

        // The fake `emd` never rewrote the manifest, so the reload puts
        // the saved order back -- which is the contract, not an artefact:
        // the list on screen is always what `emd` left on disk.
        assert_eq!(order(&panel, cx), before);

        // CADENCE: row 0 to every 4 ticks. Not an `OrderEdit` -- the order
        // does not change, only the ref's `@N` suffix does.
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.set_cadence(0, 4, window, cx)));
        cx.run_until_parked();
        let cadenced = vec![
            with_cadence("gameplay/spawn_enemies", 4),
            "tick_clock".to_string(),
        ];
        assert_eq!(spawned_systems(&calls.lock().unwrap()[1]), cadenced);
        assert_eq!(
            calls.lock().unwrap()[1].args,
            expected_set_argv("update", "", &cadenced)
        );
        assert_eq!(
            run_state_message(&panel, cx),
            "done gameplay/spawn_enemies now runs every 4 ticks in schedule update"
        );

        // REMOVE: row 1, after the confirm.
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.remove_system(1, window, cx)));
        assert!(cx.has_pending_prompt(), "dropping an entry confirms");
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        let removed = apply_order_edit(&before, OrderEdit::Remove { index: 1 });
        assert_eq!(spawned_systems(&calls.lock().unwrap()[2]), removed);
        assert_eq!(
            calls.lock().unwrap()[2].args,
            expected_set_argv("update", "", &removed)
        );
        assert_eq!(
            run_state_message(&panel, cx),
            "done Removed tick_clock from schedule update"
        );

        // ADD: `render` is the schedule with a system still available.
        panel.update(cx, |panel, cx| panel.select_item("render", cx));
        let render_before = order(&panel, cx);
        assert_eq!(render_before, ["gameplay/spawn_enemies@4"]);
        let candidates = available_systems(
            &panel.read_with(cx, |panel, _| panel.manifests.systems.clone()),
            &render_before,
        );
        assert_eq!(
            candidates
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            ["tick_clock"],
            "a system already in the list, cadence and all, is not offered again"
        );
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.add_system("tick_clock", window, cx))
        });
        cx.run_until_parked();
        let added = apply_order_edit(
            &render_before,
            OrderEdit::Add {
                system_ref: "tick_clock".to_string(),
            },
        );
        assert_eq!(spawned_systems(&calls.lock().unwrap()[3]), added);
        assert_eq!(
            calls.lock().unwrap()[3].args,
            expected_set_argv("render", "", &added)
        );
        assert_eq!(
            run_state_message(&panel, cx),
            "done Added tick_clock to schedule render"
        );
        assert_eq!(calls.lock().unwrap().len(), 4, "four edits, four runs");
    }

    /// **The optimistic commit, and the visible rollback.** The reordered
    /// list is on screen while `emd` is still running; when `emd` refuses
    /// it, the list goes back AND says so -- a silent snap-back would
    /// leave the user believing the edit landed.
    #[gpui::test]
    async fn test_an_edit_shows_at_once_and_rolls_back_visibly_when_emd_refuses_it(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| err_outcome("unknown system ref: tick_clock"));
        let panel = schedule_panel(cx, dir.path(), runner, "update");
        let saved = order(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.move_system(1, true, window, cx))
        });
        // Read BEFORE the run is allowed to finish: this is the whole
        // point of the optimism.
        assert_eq!(
            order(&panel, cx),
            ["tick_clock", "gameplay/spawn_enemies"],
            "the reordered list is shown while emd is still running"
        );
        assert!(run_state_message(&panel, cx).starts_with("running"));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.order_rollback),
            None,
            "nothing has been rolled back yet"
        );

        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1, "it really ran");
        assert_eq!(order(&panel, cx), saved, "the list went back");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.order_rollback),
            Some(ORDER_ROLLBACK_NOTICE),
            "and the panel SAYS it went back, at the list itself"
        );
        assert_eq!(
            run_state_message(&panel, cx),
            "failed unknown system ref: tick_clock",
            "with emd's own reason beside it"
        );

        // The next edit clears the notice: it belongs to the edit that
        // failed, not to the schedule.
        let (runner, _) = fake_runner(|_| ok_outcome("/x/update"));
        panel.update(cx, |panel, _| panel.runner = runner);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.move_system(1, true, window, cx))
        });
        assert_eq!(panel.read_with(cx, |panel, _| panel.order_rollback), None);
        cx.run_until_parked();
    }

    /// A REVERT rolls the list back just as visibly as a failure, and
    /// still reads as a revert rather than a failure. (`emd schedule set`
    /// runs no `cargo check` today -- this is the path that stays correct
    /// if it ever grows one.)
    #[gpui::test]
    async fn test_a_reverted_schedule_set_also_rolls_the_list_back(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, _) = fake_runner(|_| {
            emd_run_outcome(
                false,
                &[
                    "emd-json: {\"emd\":\"0.2.0\",\"ok\":false,\"reverted\":true,\
                   \"error\":\"error[E0425]: cannot find function `tick_clock`\"}"
                        .to_string(),
                ],
            )
        });
        let panel = schedule_panel(cx, dir.path(), runner, "update");
        let saved = order(&panel, cx);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.move_system(0, false, window, cx))
        });
        assert_eq!(order(&panel, cx), ["tick_clock", "gameplay/spawn_enemies"]);
        cx.run_until_parked();

        assert_eq!(order(&panel, cx), saved);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.order_rollback),
            Some(ORDER_ROLLBACK_NOTICE)
        );
        assert!(
            run_state_message(&panel, cx).starts_with("reverted error[E0425]"),
            "{}",
            run_state_message(&panel, cx)
        );
    }

    /// A successful edit RECONCILES against what `emd` wrote rather than
    /// keeping the optimistic list on faith: the fake writes a different
    /// order than it was asked for, and that is what ends up on screen.
    #[gpui::test]
    async fn test_a_successful_edit_shows_what_emd_actually_wrote(cx: &mut TestAppContext) {
        let dir = populated_project();
        let manifest = dir.path().join("manifests/schedules.toml");
        let cx = empty_window(cx);
        let (runner, _) = fake_runner(move |_| {
            std::fs::write(
                &manifest,
                "version = 1\n\
                 [[schedule]]\nname = \"update\"\nsystems = [\"tick_clock@2\", \"gameplay/spawn_enemies\"]\n\
                 [[schedule]]\nname = \"render\"\nsystems = [\"gameplay/spawn_enemies@4\"]\n",
            )
            .unwrap();
            ok_outcome("/x/update")
        });
        let panel = schedule_panel(cx, dir.path(), runner, "update");

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.move_system(1, true, window, cx))
        });
        assert_eq!(
            order(&panel, cx),
            ["tick_clock", "gameplay/spawn_enemies"],
            "optimistic"
        );
        cx.run_until_parked();
        assert_eq!(
            order(&panel, cx),
            ["tick_clock@2", "gameplay/spawn_enemies"],
            "reconciled against the manifest emd left behind, not the guess"
        );
        assert_eq!(panel.read_with(cx, |panel, _| panel.order_rollback), None);
    }

    /// **The concurrent-edit rule: reject while running.** A second edit
    /// mid-run does not spawn, does not confirm, and -- the part that
    /// matters -- does not disturb the list the first edit is showing, so
    /// no committed list is ever derived from an optimistic one.
    #[gpui::test]
    async fn test_a_second_run_list_edit_is_refused_while_one_is_in_flight(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (hung, calls) = hung_runner();
        let panel = schedule_panel(cx, dir.path(), hung, "update");

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.move_system(1, true, window, cx))
        });
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1);
        let in_flight = order(&panel, cx);
        assert_eq!(in_flight, ["tick_clock", "gameplay/spawn_enemies"]);

        // Every entry point, while the first run is still going.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.move_system(1, true, window, cx);
                panel.set_cadence(0, 4, window, cx);
                panel.add_system("tick_clock", window, cx);
                panel.remove_system(0, window, cx);
            })
        });
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "the removal does not even confirm mid-run"
        );
        assert_eq!(calls.lock().unwrap().len(), 1, "nothing else spawned");
        assert_eq!(
            order(&panel, cx),
            in_flight,
            "and the shown list is untouched -- a refused edit is not half-applied"
        );
    }

    /// Cancelling the removal confirm runs nothing AND moves nothing: the
    /// optimistic swap happens when the run starts, not when the edit is
    /// computed, so a cancelled edit is invisible.
    #[gpui::test]
    async fn test_cancelling_a_run_list_removal_spawns_nothing_and_keeps_the_order(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/never"));
        let panel = schedule_panel(cx, dir.path(), runner, "update");
        let saved = order(&panel, cx);

        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.remove_system(0, window, cx)));
        let (message, detail) = cx.pending_prompt().expect("a removal confirms");
        assert_eq!(
            message,
            "Remove gameplay/spawn_enemies from the schedule update?"
        );
        assert!(
            detail.contains("appends it to the end of the run list, not to position 1."),
            "the confirm names the specific loss: {detail}"
        );
        assert_eq!(
            order(&panel, cx),
            saved,
            "nothing moved while the prompt is up"
        );

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(
            calls.lock().unwrap().is_empty(),
            "Cancel must not run anything"
        );
        assert_eq!(order(&panel, cx), saved);
        assert_eq!(panel.read_with(cx, |panel, _| panel.order_rollback), None);
        assert_eq!(run_state_message(&panel, cx), "idle");
    }

    /// A no-op edit spawns nothing: "up" on the first row and "down" on
    /// the last are the tolerant no-ops `apply_order_edit` documents, and
    /// a pointless manifest rewrite is not what they should become.
    #[gpui::test]
    async fn test_a_no_op_edit_never_reaches_the_runner(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/never"));
        let panel = schedule_panel(cx, dir.path(), runner, "update");

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.move_system(0, true, window, cx);
                panel.move_system(1, false, window, cx);
                panel.move_system(9, true, window, cx);
                panel.remove_system(9, window, cx);
                panel.set_cadence(0, 1, window, cx);
                panel.set_cadence(9, 4, window, cx);
            })
        });
        cx.run_until_parked();
        assert!(calls.lock().unwrap().is_empty());
        assert!(!cx.has_pending_prompt());
        assert_eq!(run_state_message(&panel, cx), "idle");
    }

    /// **Carried E2 review fix.** A trailer from the wrong `emd` that ALSO
    /// says `reverted` is a VERSION failure, not a compiler verdict:
    /// `verify_emd_result` keeps `reverted`, so checking it first would
    /// render "Rolled back — the project no longer compiles: emd version
    /// changed mid-session (…)", which blames the compiler for a swapped
    /// binary.
    #[gpui::test]
    async fn test_a_version_mismatch_that_also_reverted_is_not_read_as_a_revert(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, _) = fake_runner(|_| {
            emd_run_outcome(
                false,
                &[
                    "emd-json: {\"emd\":\"0.9.9\",\"ok\":false,\"reverted\":true,\
                   \"error\":\"error[E0609]: no field `hp`\"}"
                        .to_string(),
                ],
            )
        });
        let panel = browsing_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_item("HeroUnit", cx);
                panel.request_field_remove("hp", window, cx);
            })
        });
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();

        let message = run_state_message(&panel, cx);
        assert!(
            message.starts_with("failed emd version changed mid-session (0.9.9 vs 0.2.0)"),
            "{message}"
        );
        assert!(
            !message.contains("no longer compiles"),
            "the version drift must not be dressed up as a compiler verdict: {message}"
        );
        panel.read_with(cx, |panel, _| {
            assert!(matches!(&panel.run_state, RunState::Failed { .. }));
        });
    }

    // ------------------------------------------------------- the menu

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

    /// A workspace over the REAL temp project (the fake fs mirrors its
    /// shape so the worktree scans, while the panels read the actual bytes
    /// through `std::fs` against the same `abs_path`).
    async fn emerald_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        Entity<Workspace>,
        Entity<EmeraldPanel>,
        WorktreeId,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            ggo_world_panel::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            serde_json::json!({
                "emerald.toml": "",
                "manifests": { "components.toml": "version = 1\n" },
                "assets": { "worlds": {}, "tiles": {} },
                "crates": { "game-core": { "src": {} } },
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
                .panel::<EmeraldPanel>(cx)
                .expect("init() adds the panel")
        });
        // The same seeding [`locked_panel`] does, and here it is not just
        // tidiness: this panel came from `init()`, so it holds the
        // PRODUCTION probe and runner, and the first `refresh_root` would
        // stat `PATH` and spawn a real `emd version` child -- which the
        // deterministic test scheduler rejects outright ("Detected activity
        // on thread async-process").
        panel.update(cx, |panel, _| {
            panel.lock = LockCheck::Reached(EXPECTED_EMD_VERSION.to_string());
            panel.emd_probe = settled_probe();
            panel.probe = Arc::new(settled_probe);
        });
        (workspace, panel, worktree_id, cx)
    }

    /// The generate entries appear on the project root and `manifests/`,
    /// the asset entries on `assets/` and below, and NOTHING appears
    /// anywhere else -- not on `crates/`, not on a file.
    #[gpui::test]
    async fn test_context_menu_offers_entries_only_on_the_right_dirs(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let (workspace, _panel, worktree_id, cx) = emerald_workspace(cx, dir.path()).await;

        let contributed = |rel: &str, is_dir: bool, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), is_dir, window, cx)
                    .len()
            })
        };
        assert_eq!(
            contributed("", true, cx),
            3,
            "the project root: New Component/System/Schedule"
        );
        assert_eq!(contributed("manifests", true, cx), 3, "and manifests/");
        assert_eq!(
            contributed("assets", true, cx),
            2,
            "the asset root: New World + New Tileset"
        );
        assert_eq!(contributed("assets/tiles", true, cx), 2, "and below it");
        assert_eq!(contributed("crates", true, cx), 0, "not just any directory");
        assert_eq!(
            contributed("crates/game-core/src", true, cx),
            0,
            "not deep inside the crate tree either"
        );
        assert_eq!(
            contributed("emerald.toml", false, cx),
            0,
            "this panel claims no files"
        );
    }

    /// A worktree with no `emerald.toml` gets no entries at all: `emd`
    /// would have nothing to discover.
    #[gpui::test]
    async fn test_no_entries_outside_an_emerald_project(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(dir.path(), serde_json::json!({ "assets": {} }))
            .await;
        let project = Project::test(fs, [dir.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);

        for rel in ["", "assets"] {
            let n = workspace.update_in(cx, |workspace, window, cx| {
                workspace
                    .context_menu_contributions(&project_path(worktree_id, rel), true, window, cx)
                    .len()
            });
            assert_eq!(n, 0, "{rel:?} is not in an emerald project");
        }
    }

    /// Fire the REAL menu handler (the only way to exercise a contributed
    /// entry -- `ContextMenuEntry` keeps its handler private) and prove it
    /// opens the form the entry is named for.
    #[gpui::test]
    async fn test_the_menu_handlers_open_the_forms(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let (workspace, panel, _, cx) = emerald_workspace(cx, dir.path()).await;

        let handler = new_item_handler(
            workspace.downgrade(),
            GenKind::Schedule,
            "manifests".to_string(),
        );
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Generate(form)) => {
                assert_eq!(form.kind, GenKind::Schedule);
                assert_eq!(form.project_dir, dir.path());
            }
            _ => panic!("expected a generate form"),
        });

        let handler = new_tileset_handler(workspace.downgrade(), "assets/tiles".to_string());
        cx.update(|window, cx| handler(window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Tileset(form)) => {
                assert_eq!(form.asset_root, dir.path().join("assets"));
                assert_eq!(form.under, "tiles");
            }
            _ => panic!("expected a tileset form"),
        });
    }

    // -------------------------------------------------------- new tileset

    /// "New Tileset…" writes a real blank pair, asset-root-relative, and
    /// spawns nothing.
    #[gpui::test]
    async fn test_new_tileset_writes_a_blank_pair_without_running_emd(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let cx = empty_window(cx);
        let (runner, calls) = fake_runner(|_| ok_outcome("/x/never"));
        let panel = lone_panel(cx, dir.path(), runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_tileset("assets/tiles", window, cx)
            })
        });
        let stem = panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Tileset(form)) => form.stem.clone(),
            _ => panic!("expected a tileset form"),
        });
        type_into(&stem, "world", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();

        assert!(calls.lock().unwrap().is_empty(), "emd is not involved");
        assert_eq!(
            run_state_message(&panel, cx),
            "done Created tileset tiles/world.til"
        );
        let read =
            ggo_worldlib::sprites::io::open_tileset(&dir.path().join("assets"), "tiles/world.til")
                .unwrap();
        assert_eq!(read.tile_count, tileset::BLANK_TILES);
        assert!(!read.missing_pal);
        assert!(dir.path().join("assets/tiles/world.pal").is_file());

        // A second one with the same name is refused inline, not silently.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_tileset("assets/tiles", window, cx)
            })
        });
        let stem = panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Tileset(form)) => form.stem.clone(),
            _ => panic!("expected a tileset form"),
        });
        type_into(&stem, "world", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Tileset(form)) => assert!(
                form.error.as_ref().unwrap().contains("already exists"),
                "{:?}",
                form.error
            ),
            _ => panic!("the form stays open on a refused name"),
        });
    }

    // ---------------------------------------------------- the version lock

    /// An `emd version` run's transcript, shaped like the real binary's
    /// (`emd version --json` prints BOTH fields, verified against 0.2.0).
    fn version_outcome(v: &str) -> EmdRunOutcome {
        emd_run_outcome(
            true,
            &[format!(
                "emd-json: {{\"emd\":\"{v}\",\"ok\":true,\"version\":\"{v}\"}}"
            )],
        )
    }

    /// A runner that answers `emd version` from a cell the test can change
    /// mid-flight, and every other call with a plain success.
    #[allow(clippy::type_complexity)]
    fn version_runner(
        initial: &str,
    ) -> (
        EmdRunner,
        Arc<Mutex<Vec<EmdRequest>>>,
        Arc<Mutex<String>>,
        Arc<Mutex<String>>,
    ) {
        let reported = Arc::new(Mutex::new(initial.to_string()));
        let trailer = Arc::new(Mutex::new(EXPECTED_EMD_VERSION.to_string()));
        let (cell, stamp) = (reported.clone(), trailer.clone());
        let (runner, calls) = fake_runner(move |req| {
            if req.args.first().is_some_and(|a| a == "version") {
                version_outcome(&cell.lock().unwrap())
            } else {
                emd_run_outcome(
                    true,
                    &[format!(
                        "emd-json: {{\"emd\":\"{}\",\"ok\":true,\"path\":\"/x/done\"}}",
                        stamp.lock().unwrap()
                    )],
                )
            }
        });
        (runner, calls, reported, trailer)
    }

    /// A [`LockProbe`] whose answer the test controls, counting how often
    /// it is asked.
    fn scripted_probe() -> (LockProbe, Arc<Mutex<BinProbe>>, Arc<Mutex<usize>>) {
        let value = Arc::new(Mutex::new(settled_probe()));
        let count = Arc::new(Mutex::new(0usize));
        let (v, c) = (value.clone(), count.clone());
        let probe: LockProbe = Arc::new(move || {
            *c.lock().unwrap() += 1;
            *v.lock().unwrap()
        });
        (probe, value, count)
    }

    /// Fire one of each mutation the panel offers that runs WITHOUT a
    /// confirm -- a field add, a run-list reorder and a generate -- against
    /// a panel sitting on `populated_project`, running each to completion
    /// before the next (the panel refuses a second run mid-flight, which
    /// would otherwise hide the gate behind that rule instead).
    ///
    /// The confirming ops are deliberately left out: an unanswered prompt
    /// in the allowed case would be the only thing under test, and their
    /// gate is asserted separately (via `_confirm_task`, which stays `None`
    /// when the version lock refuses before the prompt).
    fn fire_every_mutation(panel: &Entity<EmeraldPanel>, cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_tab(BrowseTab::Components, cx);
                panel.select_item("HeroUnit", cx);
                panel.toggle_field_form(window, cx);
            })
        });
        if let Some(field) = panel.read_with(cx, |panel, _| {
            panel.field_form.as_ref().map(|row| row.name.clone())
        }) {
            type_into(&field, "armour", cx);
        }
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit_field_add(window, cx)));
        cx.run_until_parked();

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_tab(BrowseTab::Schedules, cx);
                panel.select_item("update", cx);
                panel.move_system(1, true, window, cx);
            })
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::Component, "manifests", window, cx)
            })
        });
        if let Some(name) = panel.read_with(cx, |panel, _| match &panel.form {
            Some(PanelForm::Generate(form)) => Some(form.name.clone()),
            _ => None,
        }) {
            type_into(&name, "shield", cx);
        }
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();
    }

    fn banner(panel: &Entity<EmeraldPanel>, cx: &mut gpui::VisualTestContext) -> Option<String> {
        panel.read_with(cx, |panel, _| {
            let message = lock::lock_message(&panel.lock);
            assert_eq!(
                message.is_some(),
                panel.render_lock_banner().is_some(),
                "the rendered banner must appear exactly when there is a line for it"
            );
            message
        })
    }

    /// The leading arg of every request the fake runner was handed -- the
    /// `emd` subcommand, which is enough to say WHICH mutations ran.
    fn subcommands(calls: &Arc<Mutex<Vec<EmdRequest>>>) -> Vec<String> {
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.args.first().cloned().unwrap_or_default())
            .collect()
    }

    /// **Every mismatched lock state gates every mutation, and each one
    /// says something different about why.** The four `LockStatus` cases
    /// plus the state that is not a `LockStatus` at all (`Unchecked`), each
    /// driven through a real panel: the runner is never called, the
    /// optimistic run list never moves, and a destructive op does not even
    /// raise its confirm.
    #[gpui::test]
    async fn test_a_mismatched_emd_gates_every_mutation_and_says_why(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let mut lines: Vec<String> = Vec::new();

        for lock in [
            LockCheck::Unchecked,
            LockCheck::Unreachable("no such file or directory".into()),
            LockCheck::Reached("0.1.9".into()),
            LockCheck::Reached("0.3.0".into()),
            LockCheck::Reached("not-a-version".into()),
        ] {
            let (runner, calls) = fake_runner(|_| ok_outcome("/x/never"));
            let panel = locked_panel(cx, dir.path(), runner, lock.clone());
            panel.update(cx, |panel, cx| panel.refresh_root(cx));
            let before = order(&panel, cx);

            fire_every_mutation(&panel, cx);
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.select_tab(BrowseTab::Components, cx);
                    panel.request_remove("Marker", window, cx);
                })
            });
            cx.run_until_parked();

            assert!(
                calls.lock().unwrap().is_empty(),
                "{lock:?} must not spawn emd: {:?}",
                subcommands(&calls)
            );
            assert_eq!(
                order(&panel, cx),
                before,
                "{lock:?} must not leave an optimistic run list behind"
            );
            assert_eq!(
                run_state_message(&panel, cx),
                "idle",
                "{lock:?} left a run state behind"
            );
            panel.read_with(cx, |panel, _| {
                assert!(
                    panel._confirm_task.is_none(),
                    "{lock:?} raised a confirm whose only possible outcome is nothing happening"
                );
            });

            let line = banner(&panel, cx).expect("every gated state explains itself");
            assert!(
                !lines.contains(&line),
                "{lock:?} reuses another state's line"
            );
            lines.push(line);
        }
    }

    /// The other half: an exactly-matching `emd` shows NO banner and lets
    /// every one of those same mutations through.
    #[gpui::test]
    async fn test_a_matching_emd_clears_the_banner_and_allows_mutations(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls, _, _) = version_runner(EXPECTED_EMD_VERSION);
        let panel = locked_panel(
            cx,
            dir.path(),
            runner,
            LockCheck::Reached(EXPECTED_EMD_VERSION.to_string()),
        );
        panel.update(cx, |panel, cx| panel.refresh_root(cx));
        assert_eq!(banner(&panel, cx), None, "a matching emd says nothing");

        fire_every_mutation(&panel, cx);

        let spawned = subcommands(&calls);
        for expected in ["component", "schedule", "generate"] {
            assert!(
                spawned.iter().any(|a| a == expected),
                "a matching lock must let `emd {expected}` run: {spawned:?}"
            );
        }
        assert!(
            !spawned.iter().any(|a| a == "version"),
            "a settled probe must not spend a version query: {spawned:?}"
        );
    }

    /// **Mid-run drift.** The run's own trailer names an `emd` this build
    /// did not negotiate: the run is downgraded, the banner goes up
    /// immediately (no extra `emd version` round trip -- the version was in
    /// the trailer), and every further mutation is gated from that moment.
    ///
    /// The message must read as NEITHER of the other two non-ok outcomes:
    /// not a revert (nothing was rolled back, and nothing about the
    /// compiler is true here) and not a plain failure (`emd` did not
    /// complain about anything).
    #[gpui::test]
    async fn test_a_version_change_mid_run_is_caught_and_gates_what_follows(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls, _, trailer) = version_runner(EXPECTED_EMD_VERSION);
        *trailer.lock().unwrap() = "0.3.0".to_string();
        let panel = locked_panel(
            cx,
            dir.path(),
            runner,
            LockCheck::Reached(EXPECTED_EMD_VERSION.to_string()),
        );
        panel.update(cx, |panel, cx| panel.refresh_root(cx));
        assert_eq!(banner(&panel, cx), None);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.select_tab(BrowseTab::Schedules, cx);
                panel.select_item("update", cx);
                panel.move_system(1, true, window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1, "the edit really ran");

        let message = run_state_message(&panel, cx);
        assert_eq!(
            message,
            "failed emd version changed mid-session (0.3.0 vs 0.2.0) — re-check in Settings"
        );
        // Distinct from the other two non-ok outcomes, in state and in
        // wording -- a swapped binary is not a compiler verdict.
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(&panel.run_state, RunState::Failed { .. }),
                "not Reverted"
            );
        });
        assert!(!message.contains(REVERTED_PREFIX));
        assert!(!message.contains("no longer compiles"));
        assert_ne!(message, "failed no such system", "not a plain emd error");

        // The banner is up NOW, naming the drift the way a pre-flight
        // CliNew would have, and it did not cost a second `emd version`.
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.lock.clone()),
            LockCheck::Reached("0.3.0".to_string())
        );
        let line = banner(&panel, cx).expect("a drift raises the banner");
        assert!(
            line.contains("this IDE build is too old for emd 0.3.0"),
            "{line}"
        );
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.args.first().is_some_and(|a| a == "version")),
            "the trailer already named the version"
        );

        // ...and from here nothing else runs.
        let before = calls.lock().unwrap().len();
        fire_every_mutation(&panel, cx);
        assert_eq!(
            calls.lock().unwrap().len(),
            before,
            "a drifted lock gates every mutation that follows"
        );
    }

    /// **The poll is a timer, not a render effect.** The stat count is the
    /// assertion: rendering the panel any number of times must not move it,
    /// and a tick that finds the same mtime must not spend an `emd version`
    /// child either. Only a genuinely changed binary re-queries.
    #[gpui::test]
    async fn test_the_lock_poll_stats_on_a_timer_and_never_on_render(cx: &mut TestAppContext) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls, reported, _) = version_runner("0.1.9");
        let (probe, mtime, stats) = scripted_probe();
        let panel = locked_panel(cx, dir.path(), runner, LockCheck::Unchecked);
        panel.update(cx, |panel, _| {
            panel.emd_probe = BinProbe::Unprobed;
            panel.probe = probe;
        });

        let versions = |calls: &Arc<Mutex<Vec<EmdRequest>>>| {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.args.first().is_some_and(|a| a == "version"))
                .count()
        };

        // The first tick fires as soon as the panel is used, and it does
        // spend a query -- nothing has been probed yet.
        panel.update(cx, |panel, cx| panel.refresh_root(cx));
        cx.run_until_parked();
        assert_eq!(*stats.lock().unwrap(), 1);
        assert_eq!(versions(&calls), 1);
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.lock.clone()),
            LockCheck::Reached("0.1.9".to_string())
        );

        // Rendering does not poll. Five renders, no stat.
        for _ in 0..5 {
            panel.update_in(cx, |panel, window, cx| {
                let _ = panel.render_body(window, cx);
            });
        }
        cx.run_until_parked();
        assert_eq!(*stats.lock().unwrap(), 1, "a render must not stat emd");
        assert_eq!(versions(&calls), 1);

        // A tick with the binary unchanged stats, and stops there.
        cx.executor().advance_clock(EMD_LOCK_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(*stats.lock().unwrap(), 2, "the timer did fire");
        assert_eq!(
            versions(&calls),
            1,
            "an unchanged mtime must not spend a child process"
        );

        // Replace the binary: the next tick re-queries, and the banner
        // follows the new answer.
        *mtime.lock().unwrap() = BinProbe::At(std::time::UNIX_EPOCH);
        *reported.lock().unwrap() = EXPECTED_EMD_VERSION.to_string();
        cx.executor().advance_clock(EMD_LOCK_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(*stats.lock().unwrap(), 3);
        assert_eq!(versions(&calls), 2, "a changed mtime re-queries");
        assert_eq!(banner(&panel, cx), None, "the reinstall cleared the banner");

        // A binary that vanishes is a CHANGE too, and it is queried once
        // and then left alone -- not re-spawned on every tick forever.
        *mtime.lock().unwrap() = BinProbe::Unresolved;
        *reported.lock().unwrap() = "0.3.0".to_string();
        cx.executor().advance_clock(EMD_LOCK_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(versions(&calls), 3);
        cx.executor().advance_clock(EMD_LOCK_POLL_INTERVAL * 4);
        cx.run_until_parked();
        assert_eq!(
            versions(&calls),
            3,
            "an unresolvable emd is asked once, not once a tick"
        );
    }

    /// **The pre-release asymmetry, end to end.** `compare_lock` calls
    /// `0.2.0-rc1` a match; `verify_emd_result` calls it a drift. This
    /// panel takes the strict side, so the build is refused BEFORE the run
    /// -- the alternative being a panel with every control enabled whose
    /// every run comes back "changed mid-session".
    #[gpui::test]
    async fn test_a_pre_release_emd_is_refused_up_front_not_after_every_run(
        cx: &mut TestAppContext,
    ) {
        let dir = populated_project();
        let cx = empty_window(cx);
        let (runner, calls, _, trailer) = version_runner("0.2.0-rc1");
        *trailer.lock().unwrap() = "0.2.0-rc1".to_string();
        let (probe, mtime, _) = scripted_probe();
        *mtime.lock().unwrap() = BinProbe::At(std::time::UNIX_EPOCH);
        let panel = locked_panel(cx, dir.path(), runner, LockCheck::Unchecked);
        panel.update(cx, |panel, _| {
            panel.emd_probe = BinProbe::Unprobed;
            panel.probe = probe;
        });
        panel.update(cx, |panel, cx| panel.refresh_root(cx));
        cx.run_until_parked();

        assert_eq!(
            panel.read_with(cx, |panel, _| panel.lock.clone()),
            LockCheck::Reached("0.2.0-rc1".to_string())
        );
        let line = banner(&panel, cx).expect("a suffixed build is refused, and says so");
        assert!(
            line.contains("unrecognized version \"0.2.0-rc1\""),
            "{line}"
        );

        let before = calls.lock().unwrap().len();
        fire_every_mutation(&panel, cx);
        assert_eq!(
            calls.lock().unwrap().len(),
            before,
            "nothing may run against a binary whose every result would be downgraded"
        );
    }

    // ------------------------------------------------- cross-panel effects

    /// The point of the whole task's last bullet: a component created here
    /// becomes available in `ggo_world_panel`'s inspector WITHOUT a
    /// reload. The fake runner does what `emd` would -- append to
    /// `manifests/components.toml` -- and the assertion is on the world
    /// panel's live schema set.
    #[gpui::test]
    async fn test_a_new_component_refreshes_the_world_panels_schemas(cx: &mut TestAppContext) {
        let dir = emerald_project();
        std::fs::write(dir.path().join("assets/worlds/main.toml"), "version = 1\n").unwrap();
        let (workspace, panel, worktree_id, cx) = emerald_workspace(cx, dir.path()).await;

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(
                &project_path(worktree_id, "assets/worlds/main.toml"),
                window,
                cx,
            )
        });
        assert!(claimed, "the world panel claims a world file");
        cx.run_until_parked();
        let world_panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<WorldPanel>(cx)
                .expect("ggo_world_panel::init adds it")
        });
        assert_eq!(
            world_panel.read_with(cx, |panel, _| panel.open_rel_path_now().map(str::to_string)),
            Some("assets/worlds/main.toml".to_string())
        );
        assert!(
            !world_panel
                .read_with(cx, |panel, _| panel.schema_names())
                .contains(&"HeroUnit".to_string()),
            "not there yet"
        );

        let manifest = dir.path().join("manifests/components.toml");
        let (runner, _) = fake_runner(move |_| {
            std::fs::write(
                &manifest,
                "version = 1\n\n[[component]]\nname = \"HeroUnit\"\n\n[[component.field]]\nname = \"hp\"\nkind = \"int\"\n",
            )
            .unwrap();
            ok_outcome("/x/hero_unit.rs")
        });
        panel.update(cx, |panel, _| panel.runner = runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::Component, "manifests", window, cx)
            })
        });
        let (name, _) = generate_form(&panel, cx);
        type_into(&name, "hero_unit", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();

        assert!(
            world_panel
                .read_with(cx, |panel, _| panel.schema_names())
                .contains(&"HeroUnit".to_string()),
            "the new component must be offerable without reopening the world"
        );
    }

    /// A generated world is OPENED in the panel that owns it, at the path
    /// the run's own trailer reported -- this side never re-derives
    /// `assets/worlds/<name>.toml`.
    #[gpui::test]
    async fn test_a_generated_world_opens_in_the_world_panel(cx: &mut TestAppContext) {
        let dir = emerald_project();
        let (_workspace, panel, _worktree_id, cx) = emerald_workspace(cx, dir.path()).await;

        let written = dir.path().join("assets/worlds/arena.toml");
        let reported = written.to_string_lossy().to_string();
        let (runner, _) = fake_runner(move |_| {
            std::fs::write(&written, "version = 1\n").unwrap();
            ok_outcome(&reported)
        });
        panel.update(cx, |panel, _| panel.runner = runner);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_item(GenKind::World, "assets", window, cx)
            })
        });
        let (name, _) = generate_form(&panel, cx);
        type_into(&name, "arena", cx);
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.submit(window, cx)));
        cx.run_until_parked();

        let world_panel = panel.read_with(cx, |panel, cx| {
            panel
                .workspace
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .and_then(|w| w.read(cx).panel::<WorldPanel>(cx))
                .expect("the world panel is docked")
        });
        assert_eq!(
            world_panel.read_with(cx, |panel, _| panel.open_rel_path_now().map(str::to_string)),
            Some("assets/worlds/arena.toml".to_string()),
            "the generated world opens where a panel owns it"
        );
    }
}
