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
//! what it breaks.
//!
//! **What is NOT here yet.** The schedule run-list editor (Task E3, which
//! `manifests`/`ops` are shaped for -- `schedule set` is the fourth
//! [`ops::ManifestOp`]), the `emd version` lock banner and its mtime poll
//! (Task E4; the post-run half of that enforcement, `verify_emd_result`,
//! is already on the mutation path here), and a streaming run console with
//! a cancel button. Runs are one-shot calls whose only interesting output
//! is a JSON trailer -- but, unlike F5.2's generates, the mutations are
//! `cargo check`-backed and slow, which is why they run under
//! [`runner::EMD_TIMEOUT`].
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
//! injectable so tests never need `emd` on `PATH`), and [`tileset`] is the
//! one artifact this panel writes itself rather than delegating to `emd`.
//! This module is the gpui glue.

mod forms;
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
    EXPECTED_EMD_VERSION, ManifestKind, emd_error_message, emd_reverted, group_by_module,
    schedules_using_system, verify_emd_result,
};

use forms::{ASSET_KIND, FIELD_KINDS, FieldDraft, GenDraft, GenKind};
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
    run_state: RunState,
    run_generation: u64,
    _run_task: Option<Task<()>>,
    _confirm_task: Option<Task<()>>,
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
            run_state: RunState::Idle,
            run_generation: 0,
            _run_task: None,
            _confirm_task: None,
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
        self.emerald_dir = self
            .project_root
            .as_deref()
            .and_then(emerald_project_root)
            .or_else(|| self.project_root.clone());
        self.refresh_manifests(cx);
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
        cx.notify();
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.field_form = None;
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
        if self.running() {
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
                // open for a while, and a run may have started (or the
                // project moved) since it went up.
                if this.running() {
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

    fn run_op(
        &mut self,
        op: ManifestOp,
        project_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    /// dropping the run future kills the child (`runner`'s doc explains
    /// the `kill_on_drop` chain). The panel therefore comes back to a
    /// usable state with a real message instead of sitting on "Running…"
    /// while an abandoned `cargo check` holds the project's `target/`
    /// lock. The executor's timer, not `smol::Timer`, because it is the
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
    /// the run to a failure rather than being trusted (Task E4 turns the
    /// same observation into the lock banner; this is the enforcement half
    /// and it belongs on the mutation path, which is what it protects).
    fn finish_run(
        &mut self,
        pending: PendingRun,
        outcome: ggo_worldlib::emerald::EmdRunOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let outcome = verify_emd_result(outcome, EXPECTED_EMD_VERSION);
        if !outcome.ok {
            let message = emd_error_message(&outcome);
            let reverted = emd_reverted(&outcome);
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
            self.refresh_manifests(cx);
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
        let submittable = error.is_none() && !self.running();

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
        let running = self.running();
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
                            .disabled(running)
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
        let running = self.running();
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
                            .disabled(running)
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
                        .disabled(running)
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

    /// The selected SCHEDULE's ordered run list. Read-only here; Task E3
    /// turns this into the reorder/add/cadence editor.
    fn render_schedule_detail(&self, name: &str) -> gpui::AnyElement {
        let Some(entry) = self.manifests.schedule(name) else {
            return div().into_any_element();
        };
        let mut col = v_flex().gap_0p5().pl_2();
        if entry.systems.is_empty() {
            col = col.child(
                Label::new("no systems")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        for system in &entry.systems {
            col = col.child(
                Label::new(system.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        col.into_any_element()
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
                        BrowseTab::Schedules => self.render_schedule_detail(&name),
                    });
                col = col.child(self.render_row(ix, &name, detail, cx));
                ix += 1;
            }
        }
        Some(col.into_any_element())
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let form = match &self.form {
            Some(PanelForm::Generate(_)) => Some(self.render_generate_form(window, cx)),
            Some(PanelForm::Tileset(_)) => Some(self.render_tileset_form(cx)),
            None => None,
        };
        let browser = self.render_browser(window, cx);
        let run_state = self.render_run_state();
        if form.is_none() && browser.is_none() && run_state.is_none() {
            return self.render_message(EMPTY_MESSAGE.to_string(), cx);
        }
        v_flex()
            .size_full()
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

    /// The panel alone, pointed at `root` with no workspace.
    fn lone_panel(
        cx: &mut gpui::VisualTestContext,
        root: &std::path::Path,
        runner: EmdRunner,
    ) -> Entity<EmeraldPanel> {
        let root = root.to_path_buf();
        cx.update(|_window, cx| {
            cx.new(|cx| {
                let mut panel = EmeraldPanel::new(None, cx);
                panel.root_override = Some(root);
                panel.runner = runner;
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
