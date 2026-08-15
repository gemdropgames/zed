//! ZedGG Design Docs panel: a left-dock tree of folders, markdown
//! documents and supporting files (images the documents reference), all
//! stored in the project's `zedgg.sqlite` (see `zedgg_project_db`) so the
//! design travels with the game in git.
//!
//! The panel is ONLY the tree. Clicking a document opens it as a real
//! editor tab ([`doc_view::DesignDocView`]) whose save writes back to the
//! database, so every editor affordance -- keymap, vim, markdown preview --
//! comes for free.
//!
//! Structural mirror of the GGO panels (`ggo_tileset_panel` is the
//! smallest): `Panel` impl, `ToggleFocus`, `observe_new` registration into
//! every new workspace, a `KeymapEventChannel` observer so bindings survive
//! keymap reloads, and off-thread DB work behind a generation guard.

mod doc_view;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton, MouseDownEvent,
    PathPromptOptions, Pixels, Point, Render, SharedString, Subscription, Task, WeakEntity,
    Window, actions, anchored, deferred, div, px, uniform_list,
};
use project::Project;
use ui::{ContextMenu, ListItem, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use zedgg_project_db::design_docs::{self, DesignNode, NodeKind, ROOT_ID};
use zedgg_project_db::{Connection, DB_FILE, open, open_existing};

pub use doc_view::{DesignDocView, open_doc};

actions!(
    zedgg_design,
    [
        /// Toggles focus on the ZedGG Design Docs panel.
        ToggleFocus,
        /// Creates a new design document in the selected folder.
        NewDoc,
        /// Creates a new folder in the selected folder.
        NewFolder,
        /// Imports files from disk into the selected folder.
        ImportFiles,
        /// Renames the selected design node.
        Rename,
        /// Deletes the selected design node.
        Delete,
    ]
);

const PANEL_KEY: &str = "ZedGGDesignPanel";
const KEY_CONTEXT: &str = "ZedGGDesignPanel";
const DEFAULT_WIDTH: Pixels = px(300.);
const INDENT: Pixels = px(12.);
const EMPTY_MESSAGE: &str = "Open a local project to keep design docs in it";

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
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
        let panel = cx.new(|cx| DesignPanel::new(Some((weak_workspace, project)), cx));
        workspace.add_panel(panel, window, cx);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DesignPanel>(window, cx);
        });
    })
    .detach();
}

fn bind_panel_keys(cx: &mut App) {
    let in_editor = format!("{KEY_CONTEXT} > Editor");
    cx.bind_keys([
        KeyBinding::new("enter", menu::Confirm, Some(&in_editor)),
        KeyBinding::new("escape", menu::Cancel, Some(&in_editor)),
        KeyBinding::new("f2", Rename, Some(KEY_CONTEXT)),
        KeyBinding::new("delete", Delete, Some(KEY_CONTEXT)),
        KeyBinding::new("backspace", Delete, Some(KEY_CONTEXT)),
    ]);
}

// ------------------------------------------------------------- view state

/// One visible line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: i64,
    depth: usize,
    kind: NodeKind,
    name: SharedString,
    /// The not-yet-created node currently being named (`EditKind::New`).
    pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Rename { id: i64 },
    New { parent_id: i64, kind: NodeKind },
}

/// An in-row name editor. Created per edit session (a single-line editor
/// needs a `Window`, which the panel's constructor doesn't have) and
/// dropped with the session.
struct EditState {
    kind: EditKind,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

pub struct DesignPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// Last `list_nodes` result (or the synthetic root when no DB exists).
    nodes: Vec<DesignNode>,
    /// Whether the DB file exists at all: `false` means the tree is the
    /// synthetic root only and the first write will create the file.
    db_exists: bool,
    expanded: HashSet<i64>,
    selected: Option<i64>,
    edit: Option<EditState>,
    rows: Vec<Row>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    error: Option<SharedString>,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    _project_subscription: Option<Subscription>,
}

/// What a tree row carries while being dragged onto a folder.
#[derive(Clone, Debug)]
struct DraggedNode {
    id: i64,
    name: SharedString,
}

struct DragPreview(SharedString);

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(cx.theme().colors().element_background)
            .child(Label::new(self.0.clone()).size(LabelSize::Small))
    }
}

fn synthetic_root() -> DesignNode {
    DesignNode {
        id: ROOT_ID,
        parent_id: None,
        kind: NodeKind::Folder,
        name: "Design Docs".to_string(),
    }
}

impl DesignPanel {
    pub fn new(
        workspace: Option<(WeakEntity<Workspace>, Entity<Project>)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(ROOT_ID);
        let (workspace, project) = workspace.unzip();
        // `git checkout`/`pull` swapping `zedgg.sqlite` under us: re-read
        // the tree when the worktree reports that file changed. Our own
        // writes trigger this too, which is a harmless second reload.
        let _project_subscription = project.map(|project| {
            cx.subscribe(&project, |this, _, event: &project::Event, cx| {
                if let project::Event::WorktreeUpdatedEntries(_, changes) = event
                    && changes
                        .iter()
                        .any(|(path, _, _)| path.file_name() == Some(DB_FILE))
                {
                    this.reload(cx);
                    for view in this.open_views(cx) {
                        view.update(cx, |view, cx| view.clear_image_cache(cx));
                    }
                }
            })
        });
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            workspace,
            root_override: None,
            project_root: None,
            nodes: vec![synthetic_root()],
            db_exists: false,
            expanded,
            selected: None,
            edit: None,
            rows: Vec::new(),
            context_menu: None,
            error: None,
            load_generation: 0,
            _load_task: None,
            _project_subscription,
        }
    }

    /// Re-discover the project root: the workspace's first visible LOCAL
    /// worktree (same rule as `ggo_common::rel_in_primary_worktree`). Must
    /// not run while the workspace itself is mid-update -- see the deferral
    /// in `set_active`.
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        let root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().read(cx);
            if !project.is_local() {
                return None;
            }
            let worktree = project.visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        if root != self.project_root {
            self.project_root = root;
            self.nodes = vec![synthetic_root()];
            self.db_exists = false;
            self.selected = None;
            self.edit = None;
        }
        self.reload(cx);
    }

    /// Re-read the tree from the DB on a background thread. Never creates
    /// the file: a project merely browsed in ZedGG stays untouched.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.project_root.clone() else {
            self.rebuild_rows();
            cx.notify();
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        let read = cx.background_spawn(async move {
            match open_existing(&root)? {
                Some(connection) => design_docs::list_nodes(&connection).map(Some),
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
                    Ok(Some(nodes)) => {
                        this.nodes = nodes;
                        this.db_exists = true;
                    }
                    Ok(None) => {
                        this.nodes = vec![synthetic_root()];
                        this.db_exists = false;
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.after_nodes_changed(cx);
            })
            .ok();
        }));
    }

    fn after_nodes_changed(&mut self, cx: &mut Context<Self>) {
        let ids: HashSet<i64> = self.nodes.iter().map(|n| n.id).collect();
        self.expanded.retain(|id| ids.contains(id));
        self.expanded.insert(ROOT_ID);
        if self.selected.is_some_and(|id| !ids.contains(&id)) {
            self.selected = None;
        }
        self.rebuild_rows();
        cx.notify();
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

    // ---------------------------------------------------------------- tree

    fn node(&self, id: i64) -> Option<&DesignNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The folder new nodes go into: the selection if it is a folder, else
    /// the selection's parent, else root.
    fn target_folder(&self) -> i64 {
        match self.selected.and_then(|id| self.node(id)) {
            Some(node) if node.kind == NodeKind::Folder => node.id,
            Some(node) => node.parent_id.unwrap_or(ROOT_ID),
            None => ROOT_ID,
        }
    }

    fn rebuild_rows(&mut self) {
        let mut children: HashMap<i64, Vec<&DesignNode>> = HashMap::new();
        for node in &self.nodes {
            if let Some(parent) = node.parent_id {
                children.entry(parent).or_default().push(node);
            }
        }
        let pending = match self.edit.as_ref().map(|e| e.kind) {
            Some(EditKind::New { parent_id, kind }) => Some((parent_id, kind)),
            _ => None,
        };
        let mut rows = Vec::new();
        let mut stack: Vec<(i64, usize)> = self
            .nodes
            .iter()
            .filter(|n| n.parent_id.is_none())
            .rev()
            .map(|n| (n.id, 0))
            .collect();
        while let Some((id, depth)) = stack.pop() {
            let Some(node) = self.node(id) else {
                continue;
            };
            rows.push(Row {
                id,
                depth,
                kind: node.kind,
                name: node.name.clone().into(),
                pending: false,
            });
            if node.kind == NodeKind::Folder && self.expanded.contains(&id) {
                if let Some((parent_id, kind)) = pending
                    && parent_id == id
                {
                    rows.push(Row {
                        id: 0,
                        depth: depth + 1,
                        kind,
                        name: SharedString::default(),
                        pending: true,
                    });
                }
                if let Some(kids) = children.get(&id) {
                    for kid in kids.iter().rev() {
                        stack.push((kid.id, depth + 1));
                    }
                }
            }
        }
        self.rows = rows;
    }

    fn toggle_expanded(&mut self, id: i64, cx: &mut Context<Self>) {
        if id == ROOT_ID {
            return;
        }
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn click_row(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        let is_markdown_file = self
            .node(id)
            .is_some_and(|n| n.kind == NodeKind::File && is_markdown_name(&n.name));
        match self.node(id).map(|n| n.kind) {
            Some(NodeKind::Folder) => self.toggle_expanded(id, cx),
            Some(NodeKind::Doc) => self.open_doc(id, window, cx),
            // Markdown imported before docs were recognized sits in the DB
            // as a blob `File`; convert it in place and open the editor.
            Some(NodeKind::File) if is_markdown_file => self.mutate(
                window,
                cx,
                move |connection| design_docs::convert_file_to_doc(connection, id),
                move |this, (), window, cx| this.open_doc_by_id(id, window, cx),
            ),
            _ => cx.notify(),
        }
    }

    // ---------------------------------------------------------------- edits

    fn begin_edit(
        &mut self,
        kind: EditKind,
        initial: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project_root.is_none() {
            self.error = Some(EMPTY_MESSAGE.into());
            cx.notify();
            return;
        }
        if let EditKind::New { parent_id, .. } = kind {
            self.expanded.insert(parent_id);
        }
        let editor = cx.new(|cx| Editor::single_line(window, cx));
        editor.update(cx, |editor, cx| {
            editor.set_text(initial, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        let subscription = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            // Clicking elsewhere commits, like the project panel's rename.
            if matches!(event, EditorEvent::Blurred) && window.is_window_active() {
                this.confirm_edit(window, cx);
            }
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.edit = Some(EditState {
            kind,
            editor,
            _subscription: subscription,
        });
        self.rebuild_rows();
        cx.notify();
    }

    fn confirm_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        let name = edit.editor.read(cx).text(cx).trim().to_string();
        window.focus(&self.focus_handle, cx);
        self.rebuild_rows();
        cx.notify();
        if name.is_empty() {
            return;
        }
        match edit.kind {
            EditKind::New { parent_id, kind } => self.mutate(
                window,
                cx,
                move |connection| match kind {
                    NodeKind::Folder => design_docs::create_folder(connection, parent_id, &name),
                    _ => design_docs::create_doc(connection, parent_id, &name),
                },
                move |this, id, window, cx| {
                    this.selected = Some(id);
                    if kind == NodeKind::Doc {
                        // `reload` hasn't run yet, so `open_doc` can't find
                        // the node in memory; open straight from the id.
                        this.open_doc_by_id(id, window, cx);
                    }
                },
            ),
            EditKind::Rename { id } => {
                let renamed = name.clone();
                self.mutate(
                    window,
                    cx,
                    move |connection| design_docs::rename_node(connection, id, &name),
                    move |this, (), _window, cx| this.retitle_views(id, renamed.into(), cx),
                )
            }
        }
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            window.focus(&self.focus_handle, cx);
            self.rebuild_rows();
            cx.notify();
        }
    }

    fn new_node(&mut self, kind: NodeKind, window: &mut Window, cx: &mut Context<Self>) {
        let parent_id = self.target_folder();
        self.begin_edit(EditKind::New { parent_id, kind }, "", window, cx);
    }

    fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected.and_then(|id| self.node(id)) else {
            return;
        };
        if node.id == ROOT_ID {
            return;
        }
        let (id, name) = (node.id, node.name.clone());
        self.begin_edit(EditKind::Rename { id }, &name, window, cx);
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.selected.and_then(|id| self.node(id)).cloned() else {
            return;
        };
        if node.id == ROOT_ID {
            return;
        }
        let cascade: Vec<String> = self.subtree_ids(node.id)[1..]
            .iter()
            .filter_map(|id| self.node(*id))
            .map(|n| n.name.clone())
            .collect();
        let confirm = ggo_common::confirm_destructive_cascade(
            &format!("Delete \"{}\"?", node.name),
            &cascade,
            "Delete",
            false,
            window,
            cx,
        );
        let subtree = self.subtree_ids(node.id);
        cx.spawn_in(window, async move |this, cx| {
            if !confirm.await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.mutate(
                    window,
                    cx,
                    move |connection| design_docs::delete_node(connection, node.id),
                    move |this, (), window, cx| this.close_views(&subtree, window, cx),
                )
            })
            .ok();
        })
        .detach();
    }

    /// `id` followed by everything below it, from the in-memory tree.
    fn subtree_ids(&self, id: i64) -> Vec<i64> {
        let mut out = vec![id];
        let mut index = 0;
        while index < out.len() {
            let parent = out[index];
            out.extend(
                self.nodes
                    .iter()
                    .filter(|n| n.parent_id == Some(parent))
                    .map(|n| n.id),
            );
            index += 1;
        }
        out
    }

    // ------------------------------------------------------- context menu

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(id);
        let kind = self.node(id).map(|n| n.kind);
        let is_root = id == ROOT_ID;
        let is_folder = kind == Some(NodeKind::Folder);
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .when(is_folder, |menu| {
                    menu.action("New Document", Box::new(NewDoc))
                        .action("New Folder", Box::new(NewFolder))
                        .action("Import Files…", Box::new(ImportFiles))
                })
                .when(!is_root, |menu| {
                    menu.when(is_folder, |menu| menu.separator())
                        .action("Rename", Box::new(Rename))
                        .action("Delete", Box::new(Delete))
                })
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    // ------------------------------------------------------------ doc tabs

    fn open_doc(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.node(id).is_some_and(|n| n.kind == NodeKind::Doc) {
            self.open_doc_by_id(id, window, cx);
        }
    }

    /// Open (or re-activate) the editor tab for doc `id`. Name and body are
    /// read from the DB, so this works before `reload` has caught up.
    fn open_doc_by_id(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(workspace)) = (self.project_root.clone(), self.workspace.clone())
        else {
            return;
        };
        doc_view::open_doc(workspace, root, id, window, cx);
    }

    fn open_views(&self, cx: &App) -> Vec<Entity<DesignDocView>> {
        self.workspace
            .as_ref()
            .and_then(|w| w.upgrade())
            .map(|w| w.read(cx).items_of_type::<DesignDocView>(cx).collect())
            .unwrap_or_default()
    }

    fn retitle_views(&mut self, id: i64, name: SharedString, cx: &mut Context<Self>) {
        for view in self.open_views(cx) {
            if view.read(cx).doc_id() == id {
                view.update(cx, |view, cx| view.set_name(name.clone(), cx));
            }
        }
    }

    /// Close the tabs of docs in `ids` (just deleted -- their content is
    /// gone, so no save prompt).
    fn close_views(&mut self, ids: &[i64], window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        let doomed: Vec<Entity<DesignDocView>> = self
            .open_views(cx)
            .into_iter()
            .filter(|view| ids.contains(&view.read(cx).doc_id()))
            .collect();
        workspace.update(cx, |workspace, cx| {
            for view in doomed {
                if let Some(pane) = workspace.pane_for(&view) {
                    pane.update(cx, |pane, cx| {
                        pane.close_item_by_id(
                            view.entity_id(),
                            workspace::SaveIntent::Skip,
                            window,
                            cx,
                        )
                        .detach();
                    });
                }
            }
        });
    }

    // -------------------------------------------------------------- render

    fn render_row(&self, ix: usize, row: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let id = row.id;
        let is_folder = row.kind == NodeKind::Folder;
        let editing = self.edit.as_ref().and_then(|edit| match edit.kind {
            EditKind::Rename { id: edit_id } if edit_id == id && !row.pending => {
                Some(edit.editor.clone())
            }
            EditKind::New { .. } if row.pending => Some(edit.editor.clone()),
            _ => None,
        });
        let expanded = self.expanded.contains(&id);
        let icon = match row.kind {
            NodeKind::Folder if id == ROOT_ID => IconName::Book,
            NodeKind::Folder if expanded => IconName::FolderOpen,
            NodeKind::Folder => IconName::Folder,
            NodeKind::Doc => IconName::FileDoc,
            NodeKind::File => IconName::File,
        };
        let selected = self.selected == Some(id) && !row.pending;
        let name = row.name.clone();
        let item = ListItem::new(("zedgg-design-row", ix))
            .indent_level(row.depth)
            .indent_step_size(INDENT)
            .toggle_state(selected)
            .toggle((is_folder && id != ROOT_ID).then_some(expanded))
            .on_toggle(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_expanded(id, cx)
            }))
            .when(editing.is_none() && !row.pending, |item| {
                item.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.click_row(id, window, cx)
                }))
                .on_secondary_mouse_down(cx.listener(
                    move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.deploy_context_menu(event.position, id, window, cx);
                    },
                ))
            })
            .child(
                h_flex()
                    .gap_1()
                    .h_6()
                    .w_full()
                    .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                    .child(match editing {
                        Some(editor) => div().flex_1().child(editor).into_any_element(),
                        None => Label::new(row.name.clone())
                            .size(LabelSize::Small)
                            .into_any_element(),
                    }),
            );
        div()
            .id(("zedgg-design-row-drag", ix))
            .when(!row.pending && id != ROOT_ID, |this| {
                this.on_drag(
                    DraggedNode {
                        id,
                        name: name.clone(),
                    },
                    |dragged, _, _, cx| cx.new(|_| DragPreview(dragged.name.clone())),
                )
            })
            .when(is_folder && !row.pending, |this| {
                this.drag_over::<DraggedNode>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .drag_over::<ExternalPaths>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedNode, window, cx| {
                    cx.stop_propagation();
                    this.move_node(dragged.id, id, window, cx);
                }))
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                    cx.stop_propagation();
                    this.import_paths(id, paths.paths().to_vec(), window, cx);
                }))
            })
            .child(item)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.project_root.is_some();
        h_flex()
            .gap_1()
            .px_1()
            .py_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Design Docs")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("zedgg-design-new-doc", IconName::FileDoc)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("New Document"))
                    .disabled(!enabled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_node(NodeKind::Doc, window, cx)
                    })),
            )
            .child(
                IconButton::new("zedgg-design-new-folder", IconName::FolderAdd)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("New Folder"))
                    .disabled(!enabled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_node(NodeKind::Folder, window, cx)
                    })),
            )
            .child(
                IconButton::new("zedgg-design-import", IconName::Download)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Import Files…"))
                    .disabled(!enabled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.import_files(window, cx)
                    })),
            )
    }

    fn import_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.project_root.is_none() {
            self.error = Some(EMPTY_MESSAGE.into());
            cx.notify();
            return;
        }
        let parent_id = self.target_folder();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                this.update_in(cx, |this, window, cx| {
                    this.import_paths(parent_id, paths, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Store each file's bytes as a `file` node under `parent_id`, named
    /// by its file name. Directories are skipped.
    fn import_paths(
        &mut self,
        parent_id: i64,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.expanded.insert(parent_id);
        self.mutate(
            window,
            cx,
            move |connection| {
                for path in paths.iter().filter(|p| p.is_file()) {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| anyhow::anyhow!("bad file name {}", path.display()))?;
                    let bytes = std::fs::read(path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    if is_markdown_name(name) {
                        match String::from_utf8(bytes) {
                            Ok(text) => {
                                design_docs::create_doc_with_body(
                                    connection, parent_id, name, &text,
                                )?;
                            }
                            Err(error) => {
                                design_docs::create_file(
                                    connection,
                                    parent_id,
                                    name,
                                    error.as_bytes(),
                                )?;
                            }
                        }
                    } else {
                        design_docs::create_file(connection, parent_id, name, &bytes)?;
                    }
                }
                Ok(())
            },
            |this, (), _, cx| {
                for view in this.open_views(cx) {
                    view.update(cx, |view, cx| view.clear_image_cache(cx));
                }
            },
        );
    }

    fn move_node(&mut self, id: i64, new_parent_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if id == new_parent_id || self.node(id).is_some_and(|n| n.parent_id == Some(new_parent_id)) {
            return;
        }
        self.expanded.insert(new_parent_id);
        self.mutate(
            window,
            cx,
            move |connection| design_docs::move_node(connection, id, new_parent_id),
            move |this, (), _, _| this.selected = Some(id),
        );
    }
}

fn is_markdown_name(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        })
}

impl Render for DesignPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.rows.len();
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.confirm_edit(window, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.cancel_edit(window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewDoc, window, cx| {
                this.new_node(NodeKind::Doc, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewFolder, window, cx| {
                this.new_node(NodeKind::Folder, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ImportFiles, window, cx| {
                this.import_files(window, cx)
            }))
            .on_action(cx.listener(|this, _: &Rename, window, cx| {
                this.rename_selected(window, cx)
            }))
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
                div()
                    .id("zedgg-design-tree")
                    .flex_1()
                    .min_h_0()
                    .on_drop(cx.listener(|this, dragged: &DraggedNode, window, cx| {
                        this.move_node(dragged.id, ROOT_ID, window, cx);
                    }))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        this.import_paths(ROOT_ID, paths.paths().to_vec(), window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            // Click on empty space: focus the panel, keep
                            // whatever edit is open (its blur commits it).
                            window.focus(&this.focus_handle, cx);
                            cx.notify();
                        }),
                    )
                    .child(
                        uniform_list(
                            "zedgg-design-rows",
                            row_count,
                            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                range
                                    .filter_map(|ix| {
                                        let row = this.rows.get(ix)?.clone();
                                        Some(this.render_row(ix, &row, cx).into_any_element())
                                    })
                                    .collect()
                            }),
                        )
                        .size_full(),
                    ),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(3)
            }))
    }
}

impl Focusable for DesignPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DesignPanel {}

impl Panel for DesignPanel {
    fn persistent_name() -> &'static str {
        "ZedGG Design Docs"
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
        Some(IconName::Book)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("ZedGG Design Docs")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Built-ins 0-7, GGO panels 8-15 (grep activation_priority).
        16
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use language::{Language, LanguageConfig};
    use project::{FakeFs, Project};
    use std::sync::Arc;
    use workspace::item::Item as _;
    use workspace::{AppState, MultiWorkspace};
    use zedgg_project_db::db_path;

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// A workspace with the panel registered and pointed at `root` (a real
    /// temp dir) via `root_override`, so DB reads/writes hit disk while
    /// the project itself is a `FakeFs`.
    pub(super) async fn design_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        Entity<Workspace>,
        Entity<DesignPanel>,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            markdown_preview::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(Arc::new(Language::new(
                LanguageConfig {
                    name: "Markdown".into(),
                    ..Default::default()
                },
                None,
            )));
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<DesignPanel>(cx)
                .expect("DesignPanel should have been added by init()")
        });
        panel.update(cx, |panel, cx| {
            panel.root_override = Some(root.to_path_buf());
            panel.refresh_root(cx);
        });
        cx.run_until_parked();
        (workspace, panel, cx)
    }

    pub(super) fn row_names(panel: &DesignPanel) -> Vec<(usize, String)> {
        panel
            .rows
            .iter()
            .map(|row| (row.depth, row.name.to_string()))
            .collect()
    }

    #[gpui::test]
    async fn test_toggle_focus_opens_left_dock(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, cx) = design_workspace(cx, dir.path()).await;
        workspace.update(cx, |workspace, cx| {
            assert!(!workspace.left_dock().read(cx).is_open());
        });
        cx.dispatch_action(ToggleFocus);
        workspace.update(cx, |workspace, cx| {
            assert_eq!(panel.read(cx).position, DockPosition::Left);
            assert!(workspace.left_dock().read(cx).is_open());
        });
    }

    #[gpui::test]
    async fn test_reload_lists_seeded_tree_and_never_creates_db(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, cx) = design_workspace(cx, dir.path()).await;
        panel.read_with(cx, |panel, _| {
            assert_eq!(row_names(panel), [(0, "Design Docs".to_string())]);
            assert!(!panel.db_exists);
        });
        assert!(!db_path(dir.path()).exists(), "browsing must not create the DB");

        {
            let c = open(dir.path()).unwrap();
            let specs = design_docs::create_folder(&c, ROOT_ID, "specs").unwrap();
            design_docs::create_doc(&c, specs, "combat.md").unwrap();
            design_docs::create_doc(&c, ROOT_ID, "overview.md").unwrap();
        }
        panel.update(cx, |panel, cx| panel.reload(cx));
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            assert!(panel.db_exists);
            // Folders collapsed by default: combat.md hidden.
            assert_eq!(
                row_names(panel),
                [
                    (0, "Design Docs".to_string()),
                    (1, "specs".to_string()),
                    (1, "overview.md".to_string()),
                ]
            );
            let specs = panel.nodes.iter().find(|n| n.name == "specs").unwrap().id;
            panel.toggle_expanded(specs, cx);
            assert_eq!(
                row_names(panel),
                [
                    (0, "Design Docs".to_string()),
                    (1, "specs".to_string()),
                    (2, "combat.md".to_string()),
                    (1, "overview.md".to_string()),
                ]
            );
        });
    }

    #[gpui::test]
    async fn test_create_doc_opens_tab_and_save_round_trips(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, cx) = design_workspace(cx, dir.path()).await;

        panel.update_in(cx, |panel, window, cx| {
            panel.new_node(NodeKind::Doc, window, cx);
            assert!(panel.rows.iter().any(|row| row.pending), "pending row shown");
            let editor = panel.edit.as_ref().unwrap().editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("gdd.md", window, cx));
            panel.confirm_edit(window, cx);
        });
        cx.run_until_parked();

        assert!(db_path(dir.path()).is_file(), "first create makes the DB");
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                row_names(panel),
                [(0, "Design Docs".to_string()), (1, "gdd.md".to_string())]
            );
        });
        let view = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<DesignDocView>(cx)
                .next()
                .expect("creating a doc opens its tab")
        });
        view.read_with(cx, |view, cx| {
            assert!(!view.is_dirty(cx));
            assert_eq!(view.tab_content_text(0, cx), "gdd.md");
        });

        // Preview sees it as a markdown editor.
        workspace.update(cx, |workspace, cx| {
            let editor = markdown_preview::markdown_preview_view::MarkdownPreviewView::resolve_active_item_as_markdown_editor(workspace, cx);
            assert!(editor.is_some(), "active DesignDocView acts as a Markdown editor");
        });

        view.update_in(cx, |view, window, cx| {
            view.editor()
                .update(cx, |editor, cx| editor.set_text("# Hello\n", window, cx));
            assert!(view.is_dirty(cx));
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        let doc_id = view.read_with(cx, |view, cx| {
            assert!(!view.is_dirty(cx), "saved");
            view.doc_id()
        });
        let c = open(dir.path()).unwrap();
        assert_eq!(design_docs::load_body(&c, doc_id).unwrap(), "# Hello\n");

        // Reload restores DB text and clears dirty.
        design_docs::save_body(&c, doc_id, "external\n").unwrap();
        view.update_in(cx, |view, window, cx| {
            view.editor()
                .update(cx, |editor, cx| editor.set_text("typing", window, cx));
            let project = workspace.read(cx).project().clone();
            view.reload(project, window, cx)
        })
        .await
        .unwrap();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.buffer().read(cx).text(), "external\n");
            assert!(!view.is_dirty(cx));
        });
    }

    #[gpui::test]
    async fn test_preview_image_resolver_reads_blobs(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, cx) = design_workspace(cx, dir.path()).await;
        let doc = {
            let c = open(dir.path()).unwrap();
            let specs = design_docs::create_folder(&c, ROOT_ID, "specs").unwrap();
            let images = design_docs::create_folder(&c, specs, "images").unwrap();
            design_docs::create_file(&c, images, "hero.png", b"\x89PNG").unwrap();
            design_docs::create_doc(&c, specs, "combat.md").unwrap()
        };
        panel.update_in(cx, |panel, window, cx| panel.open_doc_by_id(doc, window, cx));
        cx.run_until_parked();
        let view = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<DesignDocView>(cx).next().unwrap()
        });
        let buffer_id = view.read_with(cx, |view, _| view.buffer().entity_id());
        let resolver = cx
            .update(|_, cx| markdown_preview::buffer_image_resolver(cx, buffer_id))
            .expect("view registers a resolver for its buffer");
        assert!(matches!(
            resolver("images/hero.png"),
            Some(gpui::ImageSource::Image(_))
        ));
        assert!(resolver("images/missing.png").is_none());
        assert!(resolver("../specs/images/hero.png").is_some());
        assert!(resolver("https://example.com/x.png").is_none(), "left to the path resolver");
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::tests::*;
    use super::*;
    use gpui::TestAppContext;
    use workspace::item::Item as _;

    #[gpui::test]
    async fn test_rename_retitles_open_tab_and_delete_closes_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, cx) = design_workspace(cx, dir.path()).await;
        let (folder, doc) = {
            let c = open(dir.path()).unwrap();
            let folder = design_docs::create_folder(&c, ROOT_ID, "f").unwrap();
            (folder, design_docs::create_doc(&c, folder, "old.md").unwrap())
        };
        panel.update(cx, |panel, cx| panel.reload(cx));
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| panel.open_doc_by_id(doc, window, cx));
        cx.run_until_parked();
        let view = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<DesignDocView>(cx).next().unwrap()
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.selected = Some(doc);
            panel.rename_selected(window, cx);
            let editor = panel.edit.as_ref().unwrap().editor.clone();
            assert_eq!(editor.read(cx).text(cx), "old.md", "prefilled");
            editor.update(cx, |editor, cx| editor.set_text("new.md", window, cx));
            panel.confirm_edit(window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| assert_eq!(view.tab_content_text(0, cx), "new.md"));

        // Delete the FOLDER: cascade removes the doc, its tab closes.
        panel.update_in(cx, |panel, window, cx| {
            panel.selected = Some(folder);
            panel.delete_selected(window, cx);
        });
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<DesignDocView>(cx).count(), 0);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(row_names(panel), [(0, "Design Docs".to_string())]);
        });
    }

    #[gpui::test]
    async fn test_move_and_import(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, panel, cx) = design_workspace(cx, dir.path()).await;
        let (a, b, doc) = {
            let c = open(dir.path()).unwrap();
            let a = design_docs::create_folder(&c, ROOT_ID, "a").unwrap();
            let b = design_docs::create_folder(&c, ROOT_ID, "b").unwrap();
            (a, b, design_docs::create_doc(&c, a, "d.md").unwrap())
        };
        panel.update(cx, |panel, cx| panel.reload(cx));
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| panel.move_node(doc, b, window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.node(doc).unwrap().parent_id, Some(b));
            assert!(panel.expanded.contains(&b), "drop target expands");
            assert!(panel.error.is_none());
        });

        // Illegal move surfaces an error and changes nothing.
        panel.update_in(cx, |panel, window, cx| panel.move_node(a, doc, window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_some());
            assert_eq!(panel.node(a).unwrap().parent_id, Some(ROOT_ID));
        });

        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNGdata").unwrap();
        panel.update_in(cx, |panel, window, cx| {
            panel.import_paths(a, vec![png, dir.path().to_path_buf()], window, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            let file = panel.nodes.iter().find(|n| n.name == "shot.png").unwrap();
            assert_eq!((file.kind, file.parent_id), (NodeKind::File, Some(a)));
        });
        let c = open(dir.path()).unwrap();
        let file = design_docs::resolve_path(&c, a, "shot.png").unwrap().unwrap();
        assert_eq!(design_docs::load_file(&c, file.id).unwrap(), b"\x89PNGdata");

        // Markdown imports become editable docs, not blobs.
        let markdown = dir.path().join("notes.md");
        std::fs::write(&markdown, "# Notes\n").unwrap();
        panel.update_in(cx, |panel, window, cx| {
            panel.import_paths(a, vec![markdown], window, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.error.is_none(), "{:?}", panel.error);
            let doc = panel.nodes.iter().find(|n| n.name == "notes.md").unwrap();
            assert_eq!((doc.kind, doc.parent_id), (NodeKind::Doc, Some(a)));
        });
        let doc = design_docs::resolve_path(&c, a, "notes.md").unwrap().unwrap();
        assert_eq!(design_docs::load_body(&c, doc.id).unwrap(), "# Notes\n");
    }

    #[gpui::test]
    async fn test_click_legacy_markdown_file_converts_and_opens(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, panel, cx) = design_workspace(cx, dir.path()).await;
        let file_id = {
            let c = open(dir.path()).unwrap();
            design_docs::create_file(&c, ROOT_ID, "legacy.md", b"# Legacy\n").unwrap()
        };
        panel.update(cx, |panel, cx| panel.reload(cx));
        cx.run_until_parked();

        panel.update_in(cx, |panel, window, cx| panel.click_row(file_id, window, cx));
        cx.run_until_parked();

        let c = open(dir.path()).unwrap();
        let node = design_docs::get_node(&c, file_id).unwrap().unwrap();
        assert_eq!(node.kind, NodeKind::Doc, "click converts legacy .md file");
        assert_eq!(design_docs::load_body(&c, file_id).unwrap(), "# Legacy\n");
        let view = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<DesignDocView>(cx)
                .next()
                .expect("conversion opens the doc tab")
        });
        assert_eq!(view.read_with(cx, |view, _| view.doc_id()), file_id);
    }
}
