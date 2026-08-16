//! `TaskView`: a workspace pane item that is a full `Editor` over a task's
//! description markdown, backed by `zedgg.sqlite` instead of a file. Save
//! (cmd-s, the close prompt) writes the description back to the DB;
//! `reload` re-reads it. Modeled directly on
//! `zedgg_design_panel::doc_view::DesignDocView`.
//!
//! The buffer has NO `language::File`: `Buffer::is_dirty` tracks unsaved
//! edits against `saved_version` on its own, and `has_conflict` is never
//! raised for a fileless buffer. What DOES need saying explicitly is
//! `buffer_kind == Singleton` -- without it `Pane::skip_save_on_close`
//! treats the item as having nothing to save and closes a dirty tab with no
//! prompt.

use std::any::TypeId;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyEntity, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use language::Buffer;
use project::Project;
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemBufferKind, ItemEvent, SaveOptions};
use zedgg_project_db::open as open_db;
use zedgg_project_db::tasks::{self, TaskState};

pub struct TaskView {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    task_id: i64,
    title: SharedString,
    state: TaskState,
    project_root: PathBuf,
    _editor_event_subscription: Subscription,
}

/// Open (or re-activate) the tab for task `id`. Title and description are
/// read from the DB on a background thread.
///
/// Deferred to the end of the current effect cycle: a caller may invoke
/// this from within an update of `workspace` itself (as the board and, in
/// tests, the harness do), and reading `workspace` back out immediately
/// would panic while it's still leased for that update.
pub fn open_task(
    workspace: WeakEntity<Workspace>,
    project_root: PathBuf,
    id: i64,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        let existing = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<TaskView>(cx)
                    .find(|view| view.read(cx).task_id == id)
            })
            .ok()
            .flatten();
        if let Some(existing) = existing {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_item(&existing, true, true, window, cx);
                })
                .ok();
            return;
        }

        let load = cx.background_spawn({
            let project_root = project_root.clone();
            async move {
                let connection = open_db(&project_root)?;
                let task = tasks::get_task(&connection, id)?
                    .with_context(|| format!("no task with id {id}"))?;
                let description = tasks::load_description(&connection, id)?;
                anyhow::Ok((task, description))
            }
        });
        window
            .spawn(cx, async move |cx| {
                let (task, description) = load.await?;
                let markdown = cx
                    .update(|_, cx| {
                        workspace.read_with(cx, |workspace, cx| {
                            workspace
                                .project()
                                .read(cx)
                                .languages()
                                .language_for_name("Markdown")
                        })
                    })??
                    .await
                    .ok();
                workspace.update_in(cx, |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let view = cx.new(|cx| {
                        TaskView::new(
                            project_root,
                            task.id,
                            task.title.into(),
                            task.state,
                            description,
                            markdown,
                            project,
                            window,
                            cx,
                        )
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
            })
            .detach_and_log_err(cx);
    });
}

impl TaskView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        project_root: PathBuf,
        task_id: i64,
        title: SharedString,
        state: TaskState,
        description: String,
        markdown: Option<Arc<language::Language>>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let languages = project.read(cx).languages().clone();
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(description, cx);
            buffer.set_language_registry(languages);
            buffer.set_language(markdown, cx);
            buffer
        });
        let editor = cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project), window, cx));
        // Without this the pane never hears about edits: no dirty dot, no
        // save prompt.
        let _editor_event_subscription =
            cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| cx.emit(event.clone()));

        Self {
            editor,
            buffer,
            task_id,
            title,
            state,
            project_root,
            _editor_event_subscription,
        }
    }

    pub fn task_id(&self) -> i64 {
        self.task_id
    }

    pub fn title(&self) -> &SharedString {
        &self.title
    }

    fn set_from_db(&mut self, title: SharedString, description: String, cx: &mut Context<Self>) {
        self.title = title;
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_text(description, cx);
            let version = buffer.version();
            buffer.did_save(version, None, cx);
        });
        cx.emit(EditorEvent::TitleChanged);
    }
}

impl EventEmitter<EditorEvent> for TaskView {}

impl Focusable for TaskView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for TaskView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(self.title.clone()).size(LabelSize::Large))
                    .child(Label::new(self.state.label()).color(Color::Muted)),
            )
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

impl Item for TaskView {
    type Event = EditorEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("Task: {}", self.title).into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer.read(cx).is_dirty()
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn can_save_as(&self, _cx: &App) -> bool {
        false
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let (text, version) = {
            let buffer = self.buffer.read(cx);
            (buffer.text(), buffer.version())
        };
        let write = cx.background_spawn({
            let root = self.project_root.clone();
            let id = self.task_id;
            async move { tasks::save_description(&open_db(&root)?, id, &text) }
        });
        cx.spawn(async move |this, cx| {
            write.await?;
            // Only edits up to `version` are saved; anything typed during
            // the write keeps the tab dirty.
            this.update(cx, |this, cx| {
                this.buffer
                    .update(cx, |buffer, cx| buffer.did_save(version, None, cx));
            })
        })
    }

    fn reload(
        &mut self,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let read = cx.background_spawn({
            let root = self.project_root.clone();
            let id = self.task_id;
            async move {
                let connection = open_db(&root)?;
                let task = tasks::get_task(&connection, id)?
                    .with_context(|| format!("no task with id {id}"))?;
                let description = tasks::load_description(&connection, id)?;
                anyhow::Ok((task.title, description))
            }
        });
        cx.spawn(async move |this, cx| {
            let (title, description) = read.await?;
            this.update(cx, |this, cx| this.set_from_db(title.into(), description, cx))
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl TaskView {
    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }
}
