# ZedGG Tasks Panel — Design

A task management system built into ZedGG the same way Design Docs is:
a left-dock panel, workspace tabs, and a schema in the per-project
`zedgg.sqlite`. Single-user, project-local; no accounts, no sync.

Reference points (lite research pass): Linear's model — workflow states
as board columns, issues with title/markdown description/labels/
attachments, board and list as two layouts over the same data. Kanban
UX convention — manual ordering via spaced numeric ranks so one drop
writes one row.

## States

Fixed, in column order: **Backlog, In Progress, Review, Done**.
No custom workflows.

## Schema — `zedgg_project_db/src/tasks.rs`

New migration in the existing `zedgg.sqlite`, alongside `design_docs`:

```sql
CREATE TABLE tasks (
    id INTEGER PRIMARY KEY,
    state TEXT NOT NULL DEFAULT 'backlog'
        CHECK (state IN ('backlog', 'in_progress', 'review', 'done')),
    rank INTEGER NOT NULL,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE task_tags (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    color TEXT NOT NULL
);
CREATE TABLE task_tag_assignments (
    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES task_tags(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, tag_id)
);
CREATE TABLE task_files (
    id INTEGER PRIMARY KEY,
    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    data BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (task_id, name)
);
```

Plain functions on `Connection`, mirroring `design_docs`:

- `list_tasks` (id, state, rank, title, tag ids — no descriptions),
  ordered by state column order then rank.
- `create_task(title) -> id` — lands at the top of Backlog
  (min rank − 1000).
- `get_task`, `rename_task`, `delete_task` (cascade wipes tags
  assignments + files).
- `load_description` / `save_description` — save errors visibly if the
  task was deleted (same contract as `design_docs::save_body`).
- `move_task(id, state, rank)` — caller computes the rank; helper
  `rank_between(above, below)` returns the midpoint, and when the gap
  is exhausted the column is renumbered to multiples of 1000 in one
  transaction.
- Tags: `create_tag(name, color)`, `list_tags`, `assign_tag`,
  `unassign_tag`, `delete_tag`. Names unique; validation like
  `design_docs::validate_name`.
- Files: `add_file(task_id, name, bytes)`, `list_files(task_id)`,
  `load_file`, `delete_file`. Names unique per task; importing a
  duplicate name errors visibly (delete the old attachment first to
  replace it).

Rank convention: spaced integers, step 1000. New-at-top = min − 1000;
drop between neighbors = midpoint; adjacent ranks (no gap) triggers
column renumber then recompute.

## Crate — `zedgg_tasks_panel`

Three units, every pattern copied from `zedgg_design_panel`:

### `src/zedgg_tasks_panel.rs` — `TasksPanel` (left dock)

- Compact list of tasks grouped under the four state headers; click
  opens the task's detail tab; state headers collapsible.
- Toolbar: new-task button, open-board button.
- `zedgg_tasks::ToggleFocus` action (dock behavior identical to
  DesignPanel, including `root_override` test hook and the
  DB-missing / empty states).
- Reloads on the same DB-change notification mechanism the design
  panel uses.

### `src/board.rs` — `TaskBoard` (workspace tab)

- Singleton workspace `Item` (re-activates the existing tab), opened
  via `zedgg_tasks::OpenBoard` or the panel button.
- Four fixed columns; cards show title and colored tag chips.
- Drag a card between/within columns → one `move_task` write
  (state + midpoint rank). Uses the same gpui drag/drop machinery as
  the design panel's node move.
- Click card → open task detail tab. Per-column “+” creates a task
  inline (title input in the column, like the panel's pending row).
- Card context menu: delete (shared `ggo_common` confirm dialog).

### `src/task_view.rs` — `TaskView` (task detail tab)

`DesignDocView` pattern: a workspace item wrapping a full `Editor`
over a fileless Markdown buffer.

- Header strip above the editor: inline-editable title, state
  dropdown, tag chips (type-ahead add; unknown name creates the tag
  with the next color from a fixed palette — no color picker),
  attachments row (names + import button; drag-drop of files also
  imports).
- Body: markdown `Editor` over `description`. Markdown language set,
  so the quick-action-bar eye button and preview-in-split work
  unchanged. Preview is on demand (no auto-open).
- Save (cmd-s and the close prompt) writes title + description in one
  transaction; `reload` re-reads. Dirty tracking identical to
  `DesignDocView` (`buffer_kind == Singleton`).
- `![](shot.png)` in the description resolves to that task's
  `task_files` blob via the existing `markdown_preview` per-buffer
  image resolver.

## Behaviors

- Every mutation goes through the panel/board's `mutate`-style helper:
  background write, then reload + UI refresh; errors surface in the
  panel like design_panel's error row.
- Board, panel, and open task tabs all refresh on DB change.
- Deleting a task closes its open tab if any (tab save-after-delete
  errors visibly, same as design docs).

## Out of scope (deliberate)

Priority, assignees, comments, due dates, swimlanes, WIP limits,
custom workflows, tag color picker. Schema does not block adding any
of these later.

## Testing

- `zedgg_project_db::tasks` unit tests: CRUD, new-task-at-top rank,
  `rank_between` midpoint + renumber-when-dense, delete cascades,
  tag name uniqueness, duplicate file name rejection.
- `zedgg_tasks_panel` gpui tests mirroring design_panel's: create
  task opens tab + save round-trips title/description; click card
  opens tab; `move_task` handler updates state/rank and board order;
  tag assignment shows chip; deleting a task with an open tab.
- The zed-crate quick-action-bar test already covers the eye button
  for editor-wrapping items.

## Wiring

- Workspace member `crates/zedgg/tasks_panel`, `[lib] path =
  "src/zedgg_tasks_panel.rs"`.
- `zedgg_tasks_panel::init(cx)` in `crates/zed/src/main.rs` next to
  the design panel init.
- Actions namespaced `zedgg_tasks`.
