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
//!
//! The header (title, state, tags) is a second source of unsaved state:
//! `title_dirty` tracks a pending rename independently of the buffer, and
//! `Item::save` writes both in one call. State and tag changes, unlike the
//! title, write straight to the DB in the background (`mutate`) and are
//! never "dirty" -- there's no tab-close prompt for them.

use std::any::TypeId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyEntity, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, IntoElement, Render, Rgba, SharedString, Subscription, Task, WeakEntity,
    Window,
};
use language::Buffer;
use project::Project;
use ui::{ContextMenu, DropdownMenu, prelude::*};
use workspace::Workspace;
use workspace::item::{Item, ItemBufferKind, ItemEvent, SaveOptions};
use zedgg_project_db::Connection;
use zedgg_project_db::open as open_db;
use zedgg_project_db::tasks::{self, Tag, TaskRow, TaskState};

const KEY_CONTEXT: &str = "ZedGGTaskView";

const TAG_COLORS: [&str; 8] = [
    "#e06c75", "#61afef", "#98c379", "#e5c07b", "#c678dd", "#56b6c2", "#d19a66", "#abb2bf",
];

fn bind_task_view_keys(cx: &mut App) {
    let in_editor = format!("{KEY_CONTEXT} > Editor");
    cx.bind_keys([
        gpui::KeyBinding::new("enter", menu::Confirm, Some(&in_editor)),
        gpui::KeyBinding::new("escape", menu::Cancel, Some(&in_editor)),
    ]);
}

/// Registers the title/tag inline editors' Enter-confirms/Escape-cancels.
/// Called once from [`crate::panel::init`], alongside the board's own
/// binding setup.
pub(crate) fn init(cx: &mut App) {
    bind_task_view_keys(cx);
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_task_view_keys)
        .detach();
}

/// An in-header single-line editor: the title rename, or a new tag's name.
/// Mirrors the board's `EditState` -- created per edit session (needs a
/// `Window`), dropped when confirmed or cancelled.
struct TitleEditState {
    editor: Entity<Editor>,
}

struct TagEditState {
    editor: Entity<Editor>,
}

pub struct TaskView {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    task_id: i64,
    title: SharedString,
    /// A committed-but-unsaved rename (from the title editor's Enter),
    /// independent of the buffer's own dirty tracking. Cleared by `save`
    /// (after writing `rename_task`) and by `refresh` (an external reload
    /// discards it same as it discards unsaved description edits).
    title_dirty: bool,
    state: TaskState,
    /// This task's assigned tags, resolved to name+color and ordered by
    /// name (per `TaskRow::tag_ids`).
    tags: Vec<Tag>,
    error: Option<SharedString>,
    project_root: PathBuf,
    title_edit: Option<TitleEditState>,
    tag_edit: Option<TagEditState>,
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
                let all_tags = tasks::list_tags(&connection)?;
                let tags = resolve_tags(&task.tag_ids, all_tags);
                anyhow::Ok((task, description, tags))
            }
        });
        window
            .spawn(cx, async move |cx| {
                let (task, description, tags) = load.await?;
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
                            tags,
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

/// Resolves `tag_ids` to full `Tag`s, keeping their order (already
/// name-ordered -- see `tag_ids_by_task`).
fn resolve_tags(tag_ids: &[i64], all_tags: Vec<Tag>) -> Vec<Tag> {
    let tag_map: HashMap<i64, Tag> = all_tags.into_iter().map(|tag| (tag.id, tag)).collect();
    tag_ids
        .iter()
        .filter_map(|id| tag_map.get(id).cloned())
        .collect()
}

impl TaskView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        project_root: PathBuf,
        task_id: i64,
        title: SharedString,
        state: TaskState,
        tags: Vec<Tag>,
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
            title_dirty: false,
            state,
            tags,
            error: None,
            project_root,
            title_edit: None,
            tag_edit: None,
            _editor_event_subscription,
        }
    }

    pub fn task_id(&self) -> i64 {
        self.task_id
    }

    pub fn title(&self) -> &SharedString {
        &self.title
    }

    // ------------------------------------------------------------- title

    /// The title editor's Enter-commit body: sets the title immediately
    /// (the tab renames right away) and marks it unsaved. `save` writes it
    /// alongside the description.
    pub fn set_title_for_save(
        &mut self,
        title: SharedString,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.title = title;
        self.title_dirty = true;
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    fn begin_title_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.to_string();
        let editor = cx.new(|cx| Editor::single_line(window, cx));
        editor.update(cx, |editor, cx| editor.set_text(title, window, cx));
        window.focus(&editor.focus_handle(cx), cx);
        self.title_edit = Some(TitleEditState { editor });
        cx.notify();
    }

    fn confirm_title_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.title_edit.take() else {
            return;
        };
        let title = edit.editor.read(cx).text(cx).trim().to_string();
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
        if title.is_empty() {
            return;
        }
        self.set_title_for_save(title.into(), window, cx);
    }

    fn cancel_title_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_edit.take().is_some() {
            window.focus(&self.editor.focus_handle(cx), cx);
            cx.notify();
        }
    }

    // ------------------------------------------------------------- state

    /// The state dropdown's entry body: moves the task to the end of
    /// `state`'s column in the background, then refreshes from the DB.
    pub fn set_state(&mut self, state: TaskState, window: &mut Window, cx: &mut Context<Self>) {
        let task_id = self.task_id;
        self.mutate(
            window,
            cx,
            move |connection| tasks::move_task_between(connection, task_id, state, None, None),
            |_, (), _, _| {},
        );
    }

    // -------------------------------------------------------------- tags

    /// The "+ tag" editor's Enter-commit body: assigns an existing tag
    /// (case-insensitive name match) or creates one off the fixed palette,
    /// then assigns it.
    pub fn add_tag(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let task_id = self.task_id;
        self.mutate(
            window,
            cx,
            move |connection| {
                let trimmed = name.trim();
                let existing_tags = tasks::list_tags(connection)?;
                let existing = existing_tags
                    .iter()
                    .find(|tag| tag.name.eq_ignore_ascii_case(trimmed));
                let tag_id = match existing {
                    Some(tag) => tag.id,
                    None => {
                        let color = TAG_COLORS[existing_tags.len() % TAG_COLORS.len()];
                        tasks::create_tag(connection, trimmed, color)?
                    }
                };
                tasks::assign_tag(connection, task_id, tag_id)
            },
            |_, (), _, _| {},
        );
    }

    fn remove_tag(&mut self, tag_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let task_id = self.task_id;
        self.mutate(
            window,
            cx,
            move |connection| tasks::unassign_tag(connection, task_id, tag_id),
            |_, (), _, _| {},
        );
    }

    fn begin_tag_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| Editor::single_line(window, cx));
        window.focus(&editor.focus_handle(cx), cx);
        self.tag_edit = Some(TagEditState { editor });
        cx.notify();
    }

    fn confirm_tag_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.tag_edit.take() else {
            return;
        };
        let name = edit.editor.read(cx).text(cx).trim().to_string();
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
        if name.is_empty() {
            return;
        }
        self.add_tag(name.into(), window, cx);
    }

    fn cancel_tag_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tag_edit.take().is_some() {
            window.focus(&self.editor.focus_handle(cx), cx);
            cx.notify();
        }
    }

    fn confirm_header_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_edit.is_some() {
            self.confirm_title_edit(window, cx);
        } else if self.tag_edit.is_some() {
            self.confirm_tag_edit(window, cx);
        }
    }

    fn cancel_header_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_title_edit(window, cx);
        self.cancel_tag_edit(window, cx);
    }

    // ----------------------------------------------------------- mutate

    /// Run one DB mutation off-thread, then refresh from the DB. Errors
    /// land in the header's error line. Mirrors the panel's/board's
    /// `mutate`, minus the "no project root yet" case -- a `TaskView` only
    /// exists once its task has been read from a real `zedgg.sqlite`.
    fn mutate<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        op: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.error = None;
        let root = self.project_root.clone();
        let write = cx.background_spawn(async move { op(&open_db(&root)?) });
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(value) => then(this, value, window, cx),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.refresh(cx).detach_and_log_err(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Re-read title, state, description, and tags from the DB. Shared by
    /// the `Item::reload` hook (an external file change) and every header
    /// mutation's post-write refresh.
    fn refresh(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let read = cx.background_spawn({
            let root = self.project_root.clone();
            let id = self.task_id;
            async move {
                let connection = open_db(&root)?;
                let task = tasks::get_task(&connection, id)?
                    .with_context(|| format!("no task with id {id}"))?;
                let description = tasks::load_description(&connection, id)?;
                let all_tags = tasks::list_tags(&connection)?;
                let tags = resolve_tags(&task.tag_ids, all_tags);
                anyhow::Ok((task, description, tags))
            }
        });
        cx.spawn(async move |this, cx| {
            let (task, description, tags) = read.await?;
            this.update(cx, |this, cx| this.apply_refresh(task, description, tags, cx))
        })
    }

    fn apply_refresh(
        &mut self,
        task: TaskRow,
        description: String,
        tags: Vec<Tag>,
        cx: &mut Context<Self>,
    ) {
        self.title = task.title.into();
        self.state = task.state;
        self.tags = tags;
        self.title_dirty = false;
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_text(description, cx);
            let version = buffer.version();
            buffer.did_save(version, None, cx);
        });
        cx.emit(EditorEvent::TitleChanged);
    }

    // -------------------------------------------------------------- render

    fn render_header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_row: AnyElement = if let Some(edit) = &self.title_edit {
            div()
                .id("zedgg-task-title-edit")
                .min_w_32()
                .child(edit.editor.clone())
                .into_any_element()
        } else {
            div()
                .id("zedgg-task-title")
                .cursor_pointer()
                .child(Label::new(self.title.clone()).size(LabelSize::Large))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.begin_title_edit(window, cx)
                }))
                .into_any_element()
        };

        let weak_self = cx.weak_entity();
        let state_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for state in TaskState::ALL {
                let weak_self = weak_self.clone();
                menu = menu.entry(state.label(), None, move |window, cx| {
                    weak_self
                        .update(cx, |this, cx| this.set_state(state, window, cx))
                        .ok();
                });
            }
            menu
        });
        let state_dropdown = DropdownMenu::new("zedgg-task-state", self.state.label(), state_menu)
            .trigger_size(ButtonSize::Compact);

        v_flex()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.confirm_header_edit(window, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.cancel_header_edit(window, cx)
            }))
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(h_flex().gap_2().items_center().child(title_row).child(state_dropdown))
            .child(self.render_tags(cx))
    }

    fn render_tags(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .id("zedgg-task-tags")
            .gap_1()
            .flex_wrap()
            .children(self.tags.iter().map(|tag| self.render_tag_chip(tag, cx)))
            .child(self.render_tag_add(cx))
    }

    fn render_tag_chip(&self, tag: &Tag, cx: &Context<Self>) -> impl IntoElement {
        let tag_id = tag.id;
        let color = Rgba::try_from(tag.color.as_str())
            .map(Hsla::from)
            .unwrap_or_else(|_| Color::Muted.color(cx));
        h_flex()
            .id(("zedgg-task-tag", tag_id as usize))
            .gap_1()
            .px_1()
            .rounded_sm()
            .bg(color)
            .child(Label::new(tag.name.clone()).size(LabelSize::XSmall))
            .child(
                IconButton::new(("zedgg-task-tag-remove", tag_id as usize), IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove_tag(tag_id, window, cx)
                    })),
            )
    }

    /// The "+ tag" button; swaps to a free-text single-line editor while
    /// `tag_edit` is active (`add_tag` resolves the typed name against
    /// existing tags case-insensitively, so this single control covers both
    /// "assign an existing tag" and "create a new one").
    fn render_tag_add(&self, cx: &Context<Self>) -> impl IntoElement {
        if let Some(edit) = &self.tag_edit {
            div()
                .id("zedgg-task-tag-add-edit")
                .w_20()
                .child(edit.editor.clone())
                .into_any_element()
        } else {
            IconButton::new("zedgg-task-tag-add", IconName::Plus)
                .icon_size(IconSize::XSmall)
                .tooltip(ui::Tooltip::text("Add Tag"))
                .on_click(cx.listener(|this, _, window, cx| this.begin_tag_edit(window, cx)))
                .into_any_element()
        }
    }
}

impl EventEmitter<EditorEvent> for TaskView {}

impl Focusable for TaskView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for TaskView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(self.render_header(window, cx))
            .when_some(self.error.clone(), |parent, error| {
                parent.child(
                    div().px_2().py_1().child(
                        Label::new(error).size(LabelSize::Small).color(Color::Error),
                    ),
                )
            })
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
        self.buffer.read(cx).is_dirty() || self.title_dirty
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
        let title_dirty = self.title_dirty;
        let title = self.title.to_string();
        let write = cx.background_spawn({
            let root = self.project_root.clone();
            let id = self.task_id;
            async move {
                let connection = open_db(&root)?;
                // Two statements, not a single SQL transaction: the second
                // (description) can still fail after the first (title)
                // commits. Acceptable here -- `write.await?` below then
                // skips clearing both dirty bits, so the tab stays dirty
                // and the next save retries `rename_task` too (idempotent
                // for an unchanged title).
                if title_dirty {
                    tasks::rename_task(&connection, id, &title)?;
                }
                tasks::save_description(&connection, id, &text)
            }
        });
        cx.spawn(async move |this, cx| {
            write.await?;
            // Only edits up to `version` are saved; anything typed during
            // the write keeps the tab dirty.
            this.update(cx, |this, cx| {
                this.buffer
                    .update(cx, |buffer, cx| buffer.did_save(version, None, cx));
                if title_dirty {
                    this.title_dirty = false;
                }
            })
        })
    }

    fn reload(
        &mut self,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.refresh(cx)
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

    pub fn tag_names(&self) -> Vec<String> {
        self.tags.iter().map(|tag| tag.name.clone()).collect()
    }
}
