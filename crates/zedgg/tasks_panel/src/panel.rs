//! `TasksPanel`: a left-dock, flat grouped list of tasks (one section per
//! `TaskState`), all stored in the project's `zedgg.sqlite` (see
//! `zedgg_project_db::tasks`). Clicking a task opens it as a real editor tab
//! ([`crate::task_view::TaskView`]).
//!
//! Structural mirror of `zedgg_design_panel::DesignPanel`: `Panel` impl,
//! `ToggleFocus`, `observe_new` registration into every new workspace, and
//! off-thread DB work behind a generation guard. Unlike the design tree this
//! list has no depth/expansion, drag-move, import, or rename-in-place --
//! tasks rename from their tab (Task 9) and reorder/delete land in Task 11.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use gpui::{
    Action, AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, Pixels, Render, SharedString, Subscription, Task, WeakEntity, Window,
    actions, div, px, uniform_list,
};
use project::Project;
use ui::{ListItem, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use zedgg_project_db::tasks::{self, TaskRow, TaskState};
use zedgg_project_db::{Connection, DB_FILE, open, open_existing};

use crate::task_view::open_task;

actions!(
    zedgg_tasks,
    [
        /// Toggles focus on the ZedGG Tasks panel.
        ToggleFocus,
        /// Creates a new task in the Backlog.
        NewTask,
        /// Deletes the selected task.
        Delete,
        /// Opens the task board (Task 7 replaces this with the real view).
        OpenBoard,
    ]
);

const PANEL_KEY: &str = "ZedGGTasksPanel";
const KEY_CONTEXT: &str = "ZedGGTasksPanel";
const DEFAULT_WIDTH: Pixels = px(300.);
const EMPTY_MESSAGE: &str = "Open a local project to keep tasks in it";

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    crate::board::init(cx);
    crate::task_view::init(cx);
    // `zed::reload_keymaps` clears and rebuilds ALL key bindings on every
    // keymap/settings change (including once at startup), so re-bind on
    // `KeymapEventChannel` like every GGO panel does.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let weak_workspace = workspace.weak_handle();
        let project = workspace.project().clone();
        let panel = cx.new(|cx| TasksPanel::new(Some((weak_workspace, project)), cx));
        workspace.add_panel(panel, window, cx);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TasksPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &OpenBoard, window, cx| {
            let Some(root) = discover_project_root(workspace, cx) else {
                return;
            };
            crate::board::open_board(workspace, root, window, cx);
        });
    })
    .detach();
}

/// The workspace's first visible LOCAL worktree, i.e. the project root a
/// `zedgg.sqlite` would live in. Shared by the panel's own root discovery
/// (`TasksPanel::refresh_root`) and `OpenBoard`, which has no panel instance
/// (and thus no `root_override`) to consult.
pub(crate) fn discover_project_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let project = workspace.project().read(cx);
    if !project.is_local() {
        return None;
    }
    let worktree = project.visible_worktrees(cx).next()?;
    Some(worktree.read(cx).abs_path().to_path_buf())
}

fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("delete", Delete, Some(KEY_CONTEXT)),
        KeyBinding::new("backspace", Delete, Some(KEY_CONTEXT)),
    ]);
}

// ------------------------------------------------------------- view state

/// One visible line of the list: either a section header or a task under
/// the most recently emitted header.
#[derive(Clone)]
struct PanelRow {
    header: Option<TaskState>,
    task: Option<(i64, SharedString, Vec<i64>)>,
}

pub struct TasksPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    pub(crate) root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// Last `list_tasks` result.
    tasks: Vec<TaskRow>,
    collapsed: HashSet<TaskState>,
    selected: Option<i64>,
    rows: Vec<PanelRow>,
    error: Option<SharedString>,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    _project_subscription: Option<Subscription>,
}

impl TasksPanel {
    fn new(
        workspace: Option<(WeakEntity<Workspace>, Entity<Project>)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (workspace, project) = workspace.unzip();
        // `git checkout`/`pull` swapping `zedgg.sqlite` under us: re-read the
        // task list when the worktree reports that file changed. Our own
        // writes trigger this too, which is a harmless second reload.
        let _project_subscription = project.map(|project| {
            cx.subscribe(&project, |this, _, event: &project::Event, cx| {
                if let project::Event::WorktreeUpdatedEntries(_, changes) = event
                    && changes
                        .iter()
                        .any(|(path, _, _)| path.file_name() == Some(DB_FILE))
                {
                    this.reload(cx);
                }
            })
        });
        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            workspace,
            root_override: None,
            project_root: None,
            tasks: Vec::new(),
            collapsed: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            error: None,
            load_generation: 0,
            _load_task: None,
            _project_subscription,
        };
        panel.rebuild_rows();
        panel
    }

    /// Re-discover the project root: the workspace's first visible LOCAL
    /// worktree (same rule as `ggo_common::rel_in_primary_worktree`). Must
    /// not run while the workspace itself is mid-update -- see the deferral
    /// in `set_active`.
    pub(crate) fn refresh_root(&mut self, cx: &mut Context<Self>) {
        let root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            discover_project_root(workspace.read(cx), cx)
        });
        if root != self.project_root {
            self.project_root = root;
            self.tasks = Vec::new();
            self.selected = None;
        }
        self.reload(cx);
    }

    /// Re-read the tasks from the DB on a background thread. Never creates
    /// the file: a project merely browsed in ZedGG stays untouched.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.project_root.clone() else {
            self.rebuild_rows();
            cx.notify();
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        let read = cx.background_spawn(async move {
            match open_existing(&root)? {
                Some(connection) => tasks::list_tasks(&connection).map(Some),
                None => Ok(None),
            }
        });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                match result {
                    Ok(Some(tasks)) => this.tasks = tasks,
                    Ok(None) => this.tasks = Vec::new(),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.rebuild_rows();
                cx.notify();
            })
            .ok();
        }));
    }

    /// Run one DB mutation off-thread (creating the file on first write),
    /// then reload. Errors land in the panel's error line.
    fn mutate<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        op: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(root) = self.project_root.clone() else {
            self.error = Some(EMPTY_MESSAGE.into());
            cx.notify();
            return;
        };
        // A new attempt clears the previous attempt's error; `reload`
        // deliberately leaves `error` alone so a failed mutation stays
        // visible past the reload that follows it.
        self.error = None;
        let write = cx.background_spawn(async move { op(&open(&root)?) });
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(value) => then(this, value, window, cx),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    // ---------------------------------------------------------------- rows

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for state in TaskState::ALL {
            rows.push(PanelRow {
                header: Some(state),
                task: None,
            });
            if !self.collapsed.contains(&state) {
                for task in self.tasks.iter().filter(|task| task.state == state) {
                    rows.push(PanelRow {
                        header: None,
                        task: Some((task.id, task.title.clone().into(), task.tag_ids.clone())),
                    });
                }
            }
        }
        self.rows = rows;
    }

    fn toggle_collapsed(&mut self, state: TaskState, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&state) {
            self.collapsed.insert(state);
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Header label or task title, in display order -- test helper.
    #[cfg(test)]
    pub(crate) fn visible_row_labels(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| match (row.header, &row.task) {
                (Some(state), None) => state.label().to_string(),
                (None, Some((_, title, _))) => title.to_string(),
                _ => String::new(),
            })
            .collect()
    }

    /// Open the tab for task `id`. This is the row's real `on_click` body
    /// as well as the test entry point.
    pub(crate) fn click_task(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        cx.notify();
        let (Some(root), Some(workspace)) = (self.project_root.clone(), self.workspace.clone())
        else {
            return;
        };
        open_task(workspace, root, id, window, cx);
    }

    fn new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mutate(
            window,
            cx,
            |connection| tasks::create_task(connection, "New task"),
            |this, id, window, cx| this.click_task(id, window, cx),
        );
    }

    /// The `Delete` keybinding's body: acts on the current selection.
    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            return;
        };
        self.delete_task(id, window, cx);
    }

    /// Confirm ("Delete \"<title>\"?", no cascade -- a task's files/tags are
    /// implementation detail, not things the user would recognize as lost)
    /// then delete, then close the task's tab if it's open. Mirrors
    /// `zedgg_design_panel::DesignPanel::delete_selected` verbatim, minus
    /// the cascade list (tasks have no children to enumerate).
    pub(crate) fn delete_task(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .map(|task| task.title.clone())
        else {
            return;
        };
        let confirm = ggo_common::confirm_destructive_cascade(
            &format!("Delete \"{title}\"?"),
            &[],
            "Delete",
            false,
            window,
            cx,
        );
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            if !confirm.await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.mutate(
                    window,
                    cx,
                    move |connection| tasks::delete_task(connection, id),
                    move |this, (), window, cx| {
                        if this.selected == Some(id) {
                            this.selected = None;
                        }
                        if let Some(workspace) = workspace.as_ref().and_then(|w| w.upgrade()) {
                            workspace.update(cx, |workspace, cx| {
                                crate::close_task_tabs(workspace, id, window, cx);
                            });
                        }
                    },
                )
            })
            .ok();
        })
        .detach();
    }

    // -------------------------------------------------------------- render

    fn render_header_row(
        &self,
        ix: usize,
        state: TaskState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.collapsed.contains(&state);
        let icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        div()
            .id(("zedgg-tasks-header", ix))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_collapsed(state, cx)
            }))
            .child(
                h_flex()
                    .gap_1()
                    .h_6()
                    .px_1()
                    .w_full()
                    .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                    .child(
                        Label::new(state.label())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }

    fn render_task_row(
        &self,
        ix: usize,
        id: i64,
        title: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected == Some(id);
        ListItem::new(("zedgg-tasks-row", ix))
            .indent_level(1)
            .toggle_state(selected)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.click_task(id, window, cx)
            }))
            .child(
                h_flex()
                    .gap_1()
                    .h_6()
                    .w_full()
                    .child(
                        Icon::new(IconName::ListTodo)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(title).size(LabelSize::Small)),
            )
    }

    fn render_row(&self, ix: usize, row: &PanelRow, cx: &mut Context<Self>) -> AnyElement {
        match (row.header, &row.task) {
            (Some(state), None) => self.render_header_row(ix, state, cx).into_any_element(),
            (None, Some((id, title, _))) => self
                .render_task_row(ix, *id, title.clone(), cx)
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.project_root.is_some();
        h_flex()
            .gap_1()
            .px_1()
            .py_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("Tasks").size(LabelSize::Small).color(Color::Muted))
            .child(div().flex_1())
            .child(
                IconButton::new("zedgg-tasks-new", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("New Task"))
                    .disabled(!enabled)
                    .on_click(cx.listener(|this, _, window, cx| this.new_task(window, cx))),
            )
    }
}

impl Render for TasksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.rows.len();
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &NewTask, window, cx| this.new_task(window, cx)))
            .on_action(cx.listener(|this, _: &Delete, window, cx| {
                this.delete_selected(window, cx)
            }))
            .child(self.render_toolbar(cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div().px_2().py_1().child(
                        Label::new(error)
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    ),
                )
            })
            .when(self.project_root.is_none(), |this| {
                this.child(
                    div().p_2().child(
                        Label::new(EMPTY_MESSAGE)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(
                div().id("zedgg-tasks-list").flex_1().min_h_0().child(
                    uniform_list(
                        "zedgg-tasks-rows",
                        row_count,
                        cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .filter_map(|ix| {
                                    let row = this.rows.get(ix)?.clone();
                                    Some(this.render_row(ix, &row, cx))
                                })
                                .collect()
                        }),
                    )
                    .size_full(),
                ),
            )
    }
}

impl Focusable for TasksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TasksPanel {}

impl Panel for TasksPanel {
    fn persistent_name() -> &'static str {
        "ZedGG Tasks"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
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
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("ZedGG Tasks")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Built-ins 0-7, GGO panels 8-15, `zedgg_design_panel` 16 (grep
        // activation_priority across crates/).
        17
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update, and `refresh_root` reads the workspace.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_root(cx)).ok();
            });
        }
    }
}
