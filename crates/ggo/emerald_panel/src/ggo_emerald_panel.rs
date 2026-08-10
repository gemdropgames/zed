//! GGO Emerald panel (F5.2 task S3): the surface that CREATES engine
//! artifacts -- components, systems, resources, modules, worlds and
//! schedules -- by running `emd generate` and reporting what it did.
//!
//! **Why the panel exists at this point in the migration.** The spec's
//! rule is that forms live in the panel that owns the domain and the
//! context menu only routes to them, and the project panel's only prompt
//! primitive (`window.prompt`) is button-choice: it cannot collect a name,
//! let alone a module and a repeatable list of `name:kind` fields. So
//! "New Component…" needs a real form, and a form needs a panel. This one
//! arrives with exactly that responsibility. **F5.3** adds the manifest
//! ops (rename/delete/field add/remove), the schedule run-list editor and
//! the version-lock banner; the [`runner`] seam and the argv/validation
//! split in [`forms`] are shaped for them.
//!
//! **What is NOT here, deliberately.** No manifest browser, no schedule
//! order editor, no `emd version` polling, no run console with a cancel
//! button. ggo-ide has all of those (`pages/emerald/mod.rs`) and they are
//! F5.3's; a `generate` is a short one-shot call whose only interesting
//! output is its JSON trailer.
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
//! submittable" half (no gpui), [`runner`] is the "how do we run it" half
//! (no gpui either, and injectable so tests never need `emd` on `PATH`),
//! [`tileset`] is the one artifact this panel writes itself rather than
//! delegating to `emd`. This module is the gpui glue.

mod forms;
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
use ggo_worldlib::emerald::emd_error_message;

use forms::{ASSET_KIND, FIELD_KINDS, FieldDraft, GenDraft, GenKind};
use runner::{EMERALD_MANIFEST, EmdRequest, EmdRunner, emerald_project_root, system_runner};

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

/// The directory `emd` keeps `components.toml`/`systems.toml`/
/// `schedules.toml` in, under the project root.
const MANIFESTS_DIR: &str = "manifests";

/// The `assets/` directory name under an emerald project root.
const ASSETS_DIR: &str = "assets";

/// Empty-state text. The panel has no browser of its own by design: work
/// arrives by right-clicking a directory in the project panel.
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
    Running { command: String },
    Done { message: String, transcript: String },
    Failed { message: String, transcript: String },
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
    run_state: RunState,
    run_generation: u64,
    _run_task: Option<Task<()>>,
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
            run_state: RunState::Idle,
            run_generation: 0,
            _run_task: None,
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
        cx.notify();
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
        let request = EmdRequest::emd(project_dir, draft.args());
        let kind = draft.kind;
        let name = draft.name;
        self.run_state = RunState::Running {
            command: request.command_line(),
        };
        self.run_generation += 1;
        let generation = self.run_generation;
        let runner = self.runner.clone();
        let run = cx.background_spawn(async move { runner(request) });
        self._run_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = run.await;
            this.update_in(cx, |this, window, cx| {
                if this.run_generation != generation {
                    return;
                }
                this.finish_run(kind, &name, outcome, window, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Apply a finished run: on success clear the form and refresh
    /// whatever the new artifact affects; on failure keep the form open,
    /// with `emd`'s own message, so the name can be fixed and resubmitted.
    fn finish_run(
        &mut self,
        kind: GenKind,
        name: &str,
        outcome: ggo_worldlib::emerald::EmdRunOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !outcome.ok {
            self.run_state = RunState::Failed {
                message: emd_error_message(&outcome),
                transcript: outcome.output,
            };
            cx.notify();
            return;
        }
        self.form = None;
        self.run_state = RunState::Done {
            message: format!("Created {} {name}", kind.noun().to_lowercase()),
            transcript: outcome.output,
        };
        self.after_success(kind, outcome.result.as_ref(), window, cx);
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

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let form = match &self.form {
            Some(PanelForm::Generate(_)) => Some(self.render_generate_form(window, cx)),
            Some(PanelForm::Tileset(_)) => Some(self.render_tileset_form(cx)),
            None => None,
        };
        let run_state = self.render_run_state();
        if form.is_none() && run_state.is_none() {
            return self.render_message(EMPTY_MESSAGE.to_string(), cx);
        }
        v_flex()
            .size_full()
            .children(form)
            .children(run_state)
            .into_any_element()
    }
}

/// A fresh empty single-line editor.
fn single_line(window: &mut Window, cx: &mut Context<EmeraldPanel>) -> Entity<Editor> {
    cx.new(|cx| Editor::single_line(window, cx))
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
            reply(&request)
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
