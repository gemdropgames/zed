//! `TaskBoard`: a workspace tab showing all tasks as a four-column kanban
//! (`TaskState::ALL`), backed by the same `zedgg.sqlite` as
//! [`crate::panel::TasksPanel`]. Cards open the task's tab
//! ([`crate::task_view::open_task`]); a column's "+" creates a task inline
//! and drops it straight into that column.
//!
//! Structural mirror of the panel's off-thread reload/mutate pair, plus the
//! design panel's `EditState` inline-editor pattern for the "+" row. Unlike
//! the panel, the board reloads on focus (see `TaskBoard::new`) rather than
//! `Panel::set_active`, since an `Item` has no such hook.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    IntoElement, Render, Rgba, SharedString, Subscription, Task, WeakEntity, Window, div,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::Item;
use zedgg_project_db::tasks::{self, Tag, TaskState};
use zedgg_project_db::{Connection, open, open_existing};

use crate::task_view::open_task;

const KEY_CONTEXT: &str = "ZedGGTaskBoard";

fn bind_board_keys(cx: &mut App) {
    let in_editor = format!("{KEY_CONTEXT} > Editor");
    cx.bind_keys([
        gpui::KeyBinding::new("enter", menu::Confirm, Some(&in_editor)),
        gpui::KeyBinding::new("escape", menu::Cancel, Some(&in_editor)),
    ]);
}

/// Registers the board's own key bindings. Called once from
/// [`crate::panel::init`], alongside the panel's own binding setup.
pub(crate) fn init(cx: &mut App) {
    bind_board_keys(cx);
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_board_keys)
        .detach();
}

/// Activates the existing board tab if any pane has one, else opens a new
/// one in the active pane.
pub fn open_board(
    workspace: &mut Workspace,
    project_root: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<TaskBoard>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak_workspace = cx.weak_entity();
    let board = cx.new(|cx| TaskBoard::new(weak_workspace, project_root, window, cx));
    workspace.add_item_to_active_pane(Box::new(board), None, true, window, cx);
}

fn column_index(state: TaskState) -> usize {
    match state {
        TaskState::Backlog => 0,
        TaskState::InProgress => 1,
        TaskState::Review => 2,
        TaskState::Done => 3,
    }
}

#[derive(Clone)]
struct CardRow {
    id: i64,
    title: SharedString,
    /// (name, color) per tag, resolved from `tags` at reload.
    tags: Vec<(SharedString, SharedString)>,
}

/// An in-column "new task" title editor. Mirrors the design panel's
/// `EditState`: created per edit session (needs a `Window`), dropped when
/// confirmed or cancelled.
struct EditState {
    state: TaskState,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

pub struct TaskBoard {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project_root: PathBuf,
    columns: [Vec<CardRow>; 4],
    tags: HashMap<i64, Tag>,
    edit: Option<EditState>,
    error: Option<SharedString>,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    _focus_subscription: Subscription,
}

impl TaskBoard {
    fn new(
        workspace: WeakEntity<Workspace>,
        project_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let _focus_subscription = cx.on_focus(&focus_handle, window, |this, _, cx| {
            this.reload(cx);
        });
        let mut board = Self {
            focus_handle,
            workspace,
            project_root,
            columns: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            tags: HashMap::new(),
            edit: None,
            error: None,
            load_generation: 0,
            _load_task: None,
            _focus_subscription,
        };
        board.reload(cx);
        board
    }

    /// Card ids in column order. Not yet called outside tests -- Task 8's
    /// drag-drop uses it at runtime to compute drop neighbors, at which
    /// point this `allow` should come back out.
    #[allow(dead_code)]
    pub(crate) fn column(&self, state: TaskState) -> Vec<i64> {
        self.columns[column_index(state)]
            .iter()
            .map(|card| card.id)
            .collect()
    }

    /// Open the tab for task `id`. This is a card's `on_click` body as well
    /// as the test entry point.
    pub(crate) fn open_card(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        open_task(self.workspace.clone(), self.project_root.clone(), id, window, cx);
    }

    /// Re-read tasks + tags off-thread, behind a generation guard so a
    /// slower stale request can't clobber a faster later one.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_generation += 1;
        let generation = self.load_generation;
        let root = self.project_root.clone();
        let read = cx.background_spawn(async move {
            match open_existing(&root)? {
                Some(connection) => {
                    let tasks = tasks::list_tasks(&connection)?;
                    let tags = tasks::list_tags(&connection)?;
                    anyhow::Ok(Some((tasks, tags)))
                }
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
                    Ok(Some((tasks, tags))) => this.apply(tasks, tags),
                    Ok(None) => this.apply(Vec::new(), Vec::new()),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn apply(&mut self, tasks: Vec<tasks::TaskRow>, tags: Vec<Tag>) {
        let tag_map: HashMap<i64, Tag> = tags.into_iter().map(|tag| (tag.id, tag)).collect();
        let mut columns: [Vec<CardRow>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for task in tasks {
            let tags = task
                .tag_ids
                .iter()
                .filter_map(|id| tag_map.get(id))
                .map(|tag| (SharedString::from(tag.name.clone()), SharedString::from(tag.color.clone())))
                .collect();
            columns[column_index(task.state)].push(CardRow {
                id: task.id,
                title: task.title.into(),
                tags,
            });
        }
        self.tags = tag_map;
        self.columns = columns;
    }

    /// Run one DB mutation off-thread (creating the file on first write),
    /// then reload. Errors land in the board's error line.
    fn mutate<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        op: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.error = None;
        let root = self.project_root.clone();
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

    // ---------------------------------------------------------------- edits

    fn begin_edit(&mut self, state: TaskState, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| Editor::single_line(window, cx));
        let subscription = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            // Clicking elsewhere commits, like the design panel's rename.
            if matches!(event, EditorEvent::Blurred) && window.is_window_active() {
                this.confirm_edit(window, cx);
            }
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.edit = Some(EditState {
            state,
            editor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn confirm_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        let title = edit.editor.read(cx).text(cx).trim().to_string();
        window.focus(&self.focus_handle, cx);
        cx.notify();
        if title.is_empty() {
            return;
        }
        let state = edit.state;
        self.mutate(
            window,
            cx,
            move |connection| {
                let id = tasks::create_task(connection, &title)?;
                if state != TaskState::Backlog {
                    tasks::move_task_between(connection, id, state, None, None)?;
                }
                Ok(id)
            },
            move |this, id, window, cx| this.open_card(id, window, cx),
        );
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    // -------------------------------------------------------------- render

    fn chip_color(color: &str, cx: &App) -> Hsla {
        Rgba::try_from(color)
            .map(Hsla::from)
            .unwrap_or_else(|_| Color::Muted.color(cx))
    }

    fn render_card(&self, card: &CardRow, cx: &mut Context<Self>) -> AnyElement {
        let id = card.id;
        div()
            .id(("zedgg-board-card", id as usize))
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_card(id, window, cx)
            }))
            .child(Label::new(card.title.clone()).size(LabelSize::Small))
            .when(!card.tags.is_empty(), |parent| {
                parent.child(h_flex().gap_1().mt_1().flex_wrap().children(
                    card.tags.iter().map(|(name, color)| {
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(Self::chip_color(color, cx))
                            .child(Label::new(name.clone()).size(LabelSize::XSmall))
                    }),
                ))
            })
            .into_any_element()
    }

    fn render_column(&self, state: TaskState, cx: &mut Context<Self>) -> AnyElement {
        let index = column_index(state);
        let cards = &self.columns[index];
        let editing_editor = self
            .edit
            .as_ref()
            .filter(|edit| edit.state == state)
            .map(|edit| edit.editor.clone());
        let mut card_elements = Vec::with_capacity(cards.len());
        for card in cards {
            card_elements.push(self.render_card(card, cx));
        }
        v_flex()
            .id(("zedgg-board-column", index))
            .flex_1()
            .min_w_0()
            .h_full()
            .when(index > 0, |parent| {
                parent
                    .border_l_1()
                    .border_color(cx.theme().colors().border_variant)
            })
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(state.label()).size(LabelSize::Small))
                    .child(
                        Label::new(cards.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new(("zedgg-board-new", index), IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(ui::Tooltip::text("New Task"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.begin_edit(state, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id(("zedgg-board-cards", index))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .gap_1()
                            .p_1()
                            .when_some(editing_editor, |parent, editor| {
                                parent.child(
                                    div()
                                        .p_1()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(cx.theme().colors().border_focused)
                                        .child(editor),
                                )
                            })
                            .children(card_elements),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for TaskBoard {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TaskBoard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.confirm_edit(window, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.cancel_edit(window, cx)
            }))
            .when_some(self.error.clone(), |parent, error| {
                parent.child(
                    div()
                        .px_2()
                        .py_1()
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child({
                let mut columns = Vec::with_capacity(TaskState::ALL.len());
                for state in TaskState::ALL {
                    columns.push(self.render_column(state, cx));
                }
                h_flex().size_full().children(columns)
            })
    }
}

impl EventEmitter<()> for TaskBoard {}

impl Item for TaskBoard {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Task Board".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}
