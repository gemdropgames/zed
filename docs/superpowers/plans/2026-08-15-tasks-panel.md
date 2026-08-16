# ZedGG Tasks Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kanban-style task management (Backlog / In Progress / Review / Done) stored in the project's `zedgg.sqlite`, surfaced as a left-dock panel, a board tab, and per-task detail tabs.

**Architecture:** New schema module `tasks` in `zedgg_project_db` (plain functions on `Connection`, one append-only migration). New crate `zedgg_tasks_panel` with three units: `TasksPanel` (dock list), `TaskBoard` (singleton workspace tab), `TaskView` (detail tab wrapping a markdown `Editor`, DesignDocView pattern). Every UI pattern is copied from `zedgg_design_panel` — that crate is the reference implementation for panel registration, `mutate` background writes, DB-backed editor tabs, and gpui tests.

**Tech Stack:** Rust, gpui, sqlez (via `zedgg_project_db`), existing `markdown_preview` per-buffer image resolver.

**Spec:** `docs/superpowers/specs/2026-08-15-tasks-panel-design.md`

## Global Constraints

- States fixed, in this order: `backlog`, `in_progress`, `review`, `done`. UI labels: Backlog, In Progress, Review, Done.
- Ranks are spaced integers, step 1000; ordering within a column is `rank ASC`.
- No `mod.rs`; lib root is `src/zedgg_tasks_panel.rs` via `[lib] path`.
- No `unwrap()` outside tests; errors propagate with `?` or surface in the panel `error` field.
- Migrations in `zedgg_project_db` are append-only; the tasks migration is ONE new entry after `design_docs::MIGRATION`, never edited afterwards.
- Actions namespace: `zedgg_tasks`.
- Run tests with `cargo test -p <crate>`; lint with `./script/clippy -p <crate>`.
- Commit after every task with the repo's style (`zedgg tasks: ...` subject, `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` trailer).

---

### Task 1: Schema + task CRUD + description (`zedgg_project_db::tasks`)

**Files:**
- Create: `crates/zedgg/project_db/src/tasks.rs`
- Modify: `crates/zedgg/project_db/src/zedgg_project_db.rs` (add `pub mod tasks;`, append migration)
- Test: same file, `#[cfg(test)] mod tests` (pattern: `design_docs.rs` tests use `crate::open_memory`)

**Interfaces:**
- Consumes: `sqlez::connection::Connection`, `crate::open_memory` (tests).
- Produces (later tasks call these exactly):
  - `pub enum TaskState { Backlog, InProgress, Review, Done }` with `pub const ALL: [TaskState; 4]`, `pub fn as_str(self) -> &'static str` (`"backlog"` etc.), `pub fn label(self) -> &'static str` ("Backlog", "In Progress", "Review", "Done").
  - `pub struct TaskRow { pub id: i64, pub state: TaskState, pub rank: i64, pub title: String, pub tag_ids: Vec<i64> }`
  - `pub fn list_tasks(connection: &Connection) -> Result<Vec<TaskRow>>` — ordered by state column order then `rank ASC`; `tag_ids` empty until Task 3 fills them.
  - `pub fn create_task(connection: &Connection, title: &str) -> Result<i64>` — Backlog, rank = `min(backlog ranks) - 1000` (0 when empty).
  - `pub fn get_task(connection: &Connection, id: i64) -> Result<Option<TaskRow>>`
  - `pub fn rename_task(connection: &Connection, id: i64, title: &str) -> Result<()>` — errors on empty/whitespace title.
  - `pub fn delete_task(connection: &Connection, id: i64) -> Result<()>`
  - `pub fn load_description(connection: &Connection, id: i64) -> Result<String>`
  - `pub fn save_description(connection: &Connection, id: i64, description: &str) -> Result<()>` — errors if the task no longer exists (contract of `design_docs::save_body`).

- [ ] **Step 1: Write the migration and module skeleton**

`tasks.rs` starts with the full migration (ALL four tables now — later tasks add code, never schema):

```rust
//! Tasks: a fixed four-state kanban stored in `zedgg.sqlite`.
//! Plain functions on a [`Connection`], mirroring `design_docs`.

use anyhow::{Context as _, Result, bail};
use sqlez::connection::Connection;

pub const MIGRATION: &str = "
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
";
```

In `zedgg_project_db.rs`: add `pub mod tasks;` under `pub mod design_docs;` and change

```rust
const MIGRATIONS: &[&str] = &[design_docs::MIGRATION, tasks::MIGRATION];
```

- [ ] **Step 2: Write the failing tests**

In `tasks.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[test]
    fn create_lands_new_tasks_at_the_top_of_backlog() {
        let c = open_memory("tasks_create");
        let first = create_task(&c, "first").unwrap();
        let second = create_task(&c, "second").unwrap();
        let rows = list_tasks(&c).unwrap();
        assert_eq!(
            rows.iter().map(|t| t.id).collect::<Vec<_>>(),
            [second, first],
            "newest on top"
        );
        assert!(rows.iter().all(|t| t.state == TaskState::Backlog));
        assert_eq!(rows[0].rank, rows[1].rank - 1000);
    }

    #[test]
    fn rename_validates_and_delete_removes() {
        let c = open_memory("tasks_rename");
        let id = create_task(&c, "t").unwrap();
        rename_task(&c, id, "better").unwrap();
        assert_eq!(get_task(&c, id).unwrap().unwrap().title, "better");
        assert!(rename_task(&c, id, "  ").is_err());
        delete_task(&c, id).unwrap();
        assert!(get_task(&c, id).unwrap().is_none());
    }

    #[test]
    fn description_round_trips_and_save_fails_after_delete() {
        let c = open_memory("tasks_description");
        let id = create_task(&c, "t").unwrap();
        assert_eq!(load_description(&c, id).unwrap(), "");
        save_description(&c, id, "# Plan\n").unwrap();
        assert_eq!(load_description(&c, id).unwrap(), "# Plan\n");
        delete_task(&c, id).unwrap();
        assert!(save_description(&c, id, "late").is_err());
        assert!(load_description(&c, id).is_err());
    }
}
```

- [ ] **Step 3: Run tests, verify they fail to compile (functions missing)**

Run: `cargo test -p zedgg_project_db tasks`
Expected: compile errors — `create_task` etc. not found.

- [ ] **Step 4: Implement**

Follow `design_docs.rs` shapes exactly (`select_bound`, `select_row_bound`, `exec_bound`). Key pieces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Backlog,
    InProgress,
    Review,
    Done,
}

impl TaskState {
    pub const ALL: [TaskState; 4] = [
        TaskState::Backlog,
        TaskState::InProgress,
        TaskState::Review,
        TaskState::Done,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Backlog => "backlog",
            TaskState::InProgress => "in_progress",
            TaskState::Review => "review",
            TaskState::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskState::Backlog => "Backlog",
            TaskState::InProgress => "In Progress",
            TaskState::Review => "Review",
            TaskState::Done => "Done",
        }
    }

    fn parse(state: &str) -> Result<Self> {
        Ok(match state {
            "backlog" => TaskState::Backlog,
            "in_progress" => TaskState::InProgress,
            "review" => TaskState::Review,
            "done" => TaskState::Done,
            other => bail!("unknown tasks.state {other:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: i64,
    pub state: TaskState,
    pub rank: i64,
    pub title: String,
    pub tag_ids: Vec<i64>,
}
```

`list_tasks` orders with a CASE expression (SQLite has no enum order):

```sql
SELECT id, state, rank, title FROM tasks
ORDER BY CASE state
    WHEN 'backlog' THEN 0 WHEN 'in_progress' THEN 1
    WHEN 'review' THEN 2 WHEN 'done' THEN 3 END, rank
```

`create_task`:

```rust
pub fn create_task(connection: &Connection, title: &str) -> Result<i64> {
    validate_title(title)?;
    connection
        .select_row_bound::<&str, i64>(
            "INSERT INTO tasks (state, rank, title) VALUES ('backlog', \
             COALESCE((SELECT MIN(rank) FROM tasks WHERE state = 'backlog'), 1000) - 1000, ?) \
             RETURNING id",
        )?(title)?
        .context("INSERT ... RETURNING id produced no row")
}

fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty() {
        bail!("task title may not be empty");
    }
    Ok(())
}
```

`save_description` / `rename_task` guard existence first like `design_docs::save_body` (SELECT the row, `bail!("task {id} no longer exists")` on None), then `UPDATE ... SET updated_at = datetime('now')`. `load_description` uses `select_row_bound::<i64, Option<String>>` + `.flatten().with_context(...)`. `tag_ids` stays `Vec::new()` in this task.

- [ ] **Step 5: Run tests, verify pass**

Run: `cargo test -p zedgg_project_db` — new tests pass, existing `design_docs` + `open_creates_file` tests still pass (migration append is compatible).

- [ ] **Step 6: Clippy + commit**

Run: `./script/clippy -p zedgg_project_db`

```bash
git add crates/zedgg/project_db
git commit -m "zedgg tasks: schema and task CRUD in project_db"
```

---

### Task 2: Rank math + `move_task_between`

**Files:**
- Modify: `crates/zedgg/project_db/src/tasks.rs`

**Interfaces:**
- Consumes: Task 1's `TaskState`, `list_tasks`, `create_task`.
- Produces:
  - `pub fn move_task_between(connection: &Connection, id: i64, state: TaskState, above: Option<i64>, below: Option<i64>) -> Result<()>` — `above`/`below` are the NEIGHBOR TASK IDS in the target column after the drop (None = dropped at that edge). Computes the rank internally; renumbers the whole column to multiples of 1000 first when no integer gap exists. The board (Task 8) calls exactly this.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn move_between_neighbors_takes_the_midpoint() {
    let c = open_memory("tasks_move_mid");
    let a = create_task(&c, "a").unwrap(); // ranks: c=-2000, b=-1000, a=0
    let b = create_task(&c, "b").unwrap();
    let d = create_task(&c, "d").unwrap();
    move_task_between(&c, a, TaskState::Backlog, Some(d), Some(b)).unwrap();
    let order: Vec<i64> = list_tasks(&c).unwrap().iter().map(|t| t.id).collect();
    assert_eq!(order, [d, a, b]);
}

#[test]
fn move_to_column_edges_and_across_states() {
    let c = open_memory("tasks_move_edge");
    let a = create_task(&c, "a").unwrap();
    let b = create_task(&c, "b").unwrap();
    move_task_between(&c, a, TaskState::Review, None, None).unwrap();
    let rows = list_tasks(&c).unwrap();
    assert_eq!(rows.iter().find(|t| t.id == a).unwrap().state, TaskState::Review);
    // Drop b above a in Review.
    move_task_between(&c, b, TaskState::Review, None, Some(a)).unwrap();
    let review: Vec<i64> = list_tasks(&c).unwrap().iter()
        .filter(|t| t.state == TaskState::Review).map(|t| t.id).collect();
    assert_eq!(review, [b, a]);
}

#[test]
fn dense_column_renumbers_instead_of_failing() {
    let c = open_memory("tasks_move_dense");
    let a = create_task(&c, "a").unwrap();
    let b = create_task(&c, "b").unwrap();
    let d = create_task(&c, "d").unwrap();
    // Force adjacent ranks so no midpoint exists between b and a.
    c.exec_bound::<(i64, i64)>("UPDATE tasks SET rank = ? WHERE id = ?").unwrap()((0, d)).unwrap();
    c.exec_bound::<(i64, i64)>("UPDATE tasks SET rank = ? WHERE id = ?").unwrap()((1, b)).unwrap();
    c.exec_bound::<(i64, i64)>("UPDATE tasks SET rank = ? WHERE id = ?").unwrap()((2, a)).unwrap();
    move_task_between(&c, d, TaskState::Backlog, Some(b), Some(a)).unwrap();
    let order: Vec<i64> = list_tasks(&c).unwrap().iter().map(|t| t.id).collect();
    assert_eq!(order, [b, d, a]);
    let ranks: Vec<i64> = list_tasks(&c).unwrap().iter().map(|t| t.rank).collect();
    assert!(ranks.windows(2).all(|w| w[1] - w[0] >= 2), "gaps restored");
}
```

- [ ] **Step 2: Run, verify compile failure** (`move_task_between` missing)

- [ ] **Step 3: Implement**

```rust
/// Move `id` into `state`, dropped between neighbor tasks `above` and
/// `below` (either may be None at a column edge). Neighbor ranks are read
/// fresh inside the call, and a dense column is renumbered to multiples of
/// 1000 before computing the midpoint.
pub fn move_task_between(
    connection: &Connection,
    id: i64,
    state: TaskState,
    above: Option<i64>,
    below: Option<i64>,
) -> Result<()> {
    let rank_of = |task: i64| -> Result<i64> {
        connection
            .select_row_bound::<i64, i64>("SELECT rank FROM tasks WHERE id = ?")?(task)?
            .with_context(|| format!("no task with id {task}"))
    };
    let rank = match (above, below) {
        (Some(above), Some(below)) => {
            let (mut high, mut low) = (rank_of(above)?, rank_of(below)?);
            if low - high < 2 {
                renumber_column(connection, state)?;
                high = rank_of(above)?;
                low = rank_of(below)?;
            }
            high + (low - high) / 2
        }
        (Some(above), None) => rank_of(above)? + 1000,
        (None, Some(below)) => rank_of(below)? - 1000,
        (None, None) => 0,
    };
    let updated = connection.exec_bound::<(&str, i64, i64)>(
        "UPDATE tasks SET state = ?, rank = ?, updated_at = datetime('now') WHERE id = ?",
    )?((state.as_str(), rank, id));
    updated
}

fn renumber_column(connection: &Connection, state: TaskState) -> Result<()> {
    connection.exec_bound::<&str>(
        "WITH ordered AS (SELECT id, ROW_NUMBER() OVER (ORDER BY rank) AS n \
         FROM tasks WHERE state = ?1) \
         UPDATE tasks SET rank = (SELECT n * 1000 FROM ordered WHERE ordered.id = tasks.id) \
         WHERE id IN (SELECT id FROM ordered)",
    )?(state.as_str())
}
```

Note: the moved task may still be in another column during renumber — that is fine, renumber touches only the TARGET column and midpoints are computed after.

- [ ] **Step 4: Run tests, verify pass** — `cargo test -p zedgg_project_db tasks`

- [ ] **Step 5: Clippy + commit**

```bash
git add crates/zedgg/project_db
git commit -m "zedgg tasks: rank math and move_task_between"
```

---

### Task 3: Tags

**Files:**
- Modify: `crates/zedgg/project_db/src/tasks.rs`

**Interfaces:**
- Consumes: Task 1's tables and `TaskRow`.
- Produces:
  - `pub struct Tag { pub id: i64, pub name: String, pub color: String }`
  - `pub fn create_tag(connection: &Connection, name: &str, color: &str) -> Result<i64>` — name validated non-empty, unique (UNIQUE constraint surfaces as error).
  - `pub fn list_tags(connection: &Connection) -> Result<Vec<Tag>>` — by name, case-insensitive.
  - `pub fn assign_tag(connection: &Connection, task_id: i64, tag_id: i64) -> Result<()>` — idempotent (`INSERT OR IGNORE`).
  - `pub fn unassign_tag(connection: &Connection, task_id: i64, tag_id: i64) -> Result<()>`
  - `pub fn delete_tag(connection: &Connection, tag_id: i64) -> Result<()>`
  - `list_tasks` / `get_task` now fill `tag_ids` (ordered by tag name).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn tags_assign_list_and_cascade() {
    let c = open_memory("tasks_tags");
    let task = create_task(&c, "t").unwrap();
    let art = create_tag(&c, "art", "#e06c75").unwrap();
    let code = create_tag(&c, "code", "#61afef").unwrap();
    assert!(create_tag(&c, "art", "#ffffff").is_err(), "unique names");
    assert!(create_tag(&c, " ", "#ffffff").is_err());

    assign_tag(&c, task, code).unwrap();
    assign_tag(&c, task, art).unwrap();
    assign_tag(&c, task, art).unwrap(); // idempotent
    assert_eq!(get_task(&c, task).unwrap().unwrap().tag_ids, [art, code], "by name");

    unassign_tag(&c, task, code).unwrap();
    assert_eq!(get_task(&c, task).unwrap().unwrap().tag_ids, [art]);

    delete_tag(&c, art).unwrap();
    assert_eq!(get_task(&c, task).unwrap().unwrap().tag_ids, [] as [i64; 0]);
    delete_task(&c, task).unwrap();
    let orphans: Option<i64> = c
        .select_row("SELECT COUNT(*) FROM task_tag_assignments").unwrap()().unwrap();
    assert_eq!(orphans, Some(0), "cascade");
}
```

- [ ] **Step 2: Run, verify compile failure**

- [ ] **Step 3: Implement**

`tag_ids` loading: one query for all assignments, merged in Rust:

```rust
fn tag_ids_by_task(connection: &Connection) -> Result<HashMap<i64, Vec<i64>>> {
    let rows: Vec<(i64, i64)> = connection.select(
        "SELECT a.task_id, a.tag_id FROM task_tag_assignments a \
         JOIN task_tags t ON t.id = a.tag_id ORDER BY t.name COLLATE NOCASE",
    )?()?;
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    for (task_id, tag_id) in rows {
        map.entry(task_id).or_default().push(tag_id);
    }
    Ok(map)
}
```

(`use std::collections::HashMap;` at module top.) `list_tasks` and `get_task` populate from this map. Rest are single statements mirroring Task 1's shapes.

- [ ] **Step 4: Run tests, verify pass**

- [ ] **Step 5: Clippy + commit**

```bash
git add crates/zedgg/project_db
git commit -m "zedgg tasks: tags"
```

---

### Task 4: Attachments (`task_files`)

**Files:**
- Modify: `crates/zedgg/project_db/src/tasks.rs`

**Interfaces:**
- Consumes: Task 1's tables.
- Produces:
  - `pub struct TaskFile { pub id: i64, pub name: String }`
  - `pub fn add_file(connection: &Connection, task_id: i64, name: &str, data: &[u8]) -> Result<i64>` — duplicate name for the same task errors (UNIQUE).
  - `pub fn list_files(connection: &Connection, task_id: i64) -> Result<Vec<TaskFile>>` — by name.
  - `pub fn load_file_by_name(connection: &Connection, task_id: i64, name: &str) -> Result<Option<Vec<u8>>>` — the image resolver's lookup (Task 10).
  - `pub fn load_file(connection: &Connection, file_id: i64) -> Result<Vec<u8>>`
  - `pub fn delete_file(connection: &Connection, file_id: i64) -> Result<()>`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn files_round_trip_unique_per_task_and_cascade() {
    let c = open_memory("tasks_files");
    let task = create_task(&c, "t").unwrap();
    let other = create_task(&c, "u").unwrap();
    let shot = add_file(&c, task, "shot.png", b"\x89PNG").unwrap();
    add_file(&c, other, "shot.png", b"elsewhere").unwrap(); // same name, other task: fine
    assert!(add_file(&c, task, "shot.png", b"dupe").is_err());

    assert_eq!(load_file(&c, shot).unwrap(), b"\x89PNG");
    assert_eq!(load_file_by_name(&c, task, "shot.png").unwrap().unwrap(), b"\x89PNG");
    assert!(load_file_by_name(&c, task, "missing.png").unwrap().is_none());
    assert_eq!(
        list_files(&c, task).unwrap(),
        [TaskFile { id: shot, name: "shot.png".into() }]
    );

    delete_task(&c, task).unwrap();
    assert!(load_file(&c, shot).is_err(), "cascade");
}
```

- [ ] **Step 2: Run, verify compile failure**

- [ ] **Step 3: Implement** — single statements, same shapes as Task 1; `TaskFile` derives `Debug, Clone, PartialEq, Eq`. File name validation reuses `validate_title`-style non-empty check plus the `design_docs` rule (no `/`, `\`, `.`, `..`) since names double as markdown reference segments — copy `design_docs::validate_name`'s checks into a private `validate_file_name`.

- [ ] **Step 4: Run tests, verify pass**

- [ ] **Step 5: Clippy + commit**

```bash
git add crates/zedgg/project_db
git commit -m "zedgg tasks: attachments"
```

---

### Task 5: Crate scaffold + `TaskView` detail tab

**Files:**
- Create: `crates/zedgg/tasks_panel/Cargo.toml`
- Create: `crates/zedgg/tasks_panel/src/zedgg_tasks_panel.rs` (lib root; panel comes in Task 6 — for now: `mod task_view; pub use task_view::{TaskView, open_task};` plus an empty `pub fn init(cx: &mut App)` that only registers nothing yet)
- Create: `crates/zedgg/tasks_panel/src/task_view.rs`
- Modify: root `Cargo.toml` (`zedgg_tasks_panel = { path = "crates/zedgg/tasks_panel" } # ZedGG` in workspace deps; members glob `crates/zedgg/*` already covers the dir)

**Interfaces:**
- Consumes: `zedgg_project_db::tasks::{get_task, load_description, save_description, rename_task, TaskState}`, `zedgg_project_db::open`.
- Produces:
  - `pub struct TaskView` — workspace `Item`, `act_as::<Editor>`, `buffer_kind == Singleton`, save/reload against the DB.
  - `pub fn open_task(workspace: WeakEntity<Workspace>, project_root: PathBuf, id: i64, window: &mut Window, cx: &mut App)` — re-activates an existing tab for the id, else loads title+description off-thread and opens the tab. Panel (Task 6) and board (Task 7) call exactly this.
  - Test-support accessors `pub fn editor(&self)`, `pub fn task_id(&self) -> i64`, `pub fn title(&self) -> &SharedString`.

**Reference implementation:** `crates/zedgg/design_panel/src/doc_view.rs` — `TaskView` is `DesignDocView` with (a) task tables instead of design nodes, (b) a header strip above the editor, (c) title saved together with the description. Copy its structure function-for-function; differences are spelled out below.

- [ ] **Step 1: Cargo.toml**

```toml
[package]
name = "zedgg_tasks_panel"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/zedgg_tasks_panel.rs"

[dependencies]
anyhow.workspace = true
editor.workspace = true
ggo_common.workspace = true
gpui.workspace = true
keymap_editor.workspace = true
language.workspace = true
markdown_preview.workspace = true
menu.workspace = true
project.workspace = true
ui.workspace = true
workspace.workspace = true
zedgg_project_db.workspace = true

[dev-dependencies]
gpui = { workspace = true, features = ["test-support"] }
project = { workspace = true, features = ["test-support"] }
tempfile.workspace = true
workspace = { workspace = true, features = ["test-support"] }
```

- [ ] **Step 2: Write the failing gpui test**

In `zedgg_tasks_panel.rs` tests module. Test harness mirrors `design_panel`'s `design_workspace` (AppState::test, `markdown_preview::init`, FakeFs project, registered Markdown language, `MultiWorkspace::test_new`) but without a panel yet — build the workspace inline:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use language::{Language, LanguageConfig};
    use project::{FakeFs, Project};
    use std::sync::Arc;
    use workspace::item::Item as _;
    use workspace::{AppState, MultiWorkspace, Workspace};
    use zedgg_project_db::tasks;
    use zedgg_project_db::open;

    pub(super) async fn task_workspace(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<Workspace>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            markdown_preview::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(Arc::new(Language::new(
                LanguageConfig { name: "Markdown".into(), ..Default::default() },
                None,
            )));
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        (workspace, cx)
    }

    #[gpui::test]
    async fn test_open_task_edits_and_saves_description_and_title(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, cx) = task_workspace(cx).await;
        let id = {
            let c = open(dir.path()).unwrap();
            let id = tasks::create_task(&c, "Ship it").unwrap();
            tasks::save_description(&c, id, "# Steps\n").unwrap();
            id
        };
        workspace.update_in(cx, |_, window, cx| {
            open_task(
                cx.weak_entity(),
                dir.path().to_path_buf(),
                id,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let view = workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<TaskView>(cx).next().expect("tab opened")
        });
        view.read_with(cx, |view, cx| {
            assert_eq!(view.task_id(), id);
            assert_eq!(view.tab_content_text(0, cx), "Ship it");
            assert!(!view.is_dirty(cx));
        });

        view.update_in(cx, |view, window, cx| {
            view.editor()
                .update(cx, |editor, cx| editor.set_text("# Steps\n- one\n", window, cx));
            assert!(view.is_dirty(cx));
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let c = open(dir.path()).unwrap();
        assert_eq!(tasks::load_description(&c, id).unwrap(), "# Steps\n- one\n");
        view.read_with(cx, |view, cx| assert!(!view.is_dirty(cx)));

        // Second open re-activates, no duplicate tab.
        workspace.update_in(cx, |_, window, cx| {
            open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<TaskView>(cx).count(), 1);
        });
    }
}
```

Note `open_task`'s first parameter in the test: `cx.weak_entity()` inside `workspace.update_in` IS the `WeakEntity<Workspace>`.

- [ ] **Step 3: Run, verify failure** — `cargo test -p zedgg_tasks_panel` fails: `TaskView`/`open_task` unresolved.

- [ ] **Step 4: Implement `task_view.rs`**

Copy `doc_view.rs` wholesale, then adjust. The deltas (everything else stays structurally identical, including `EventEmitter<EditorEvent>`, `Focusable`, `act_as_type`, `ItemBufferKind::Singleton`, save-version handling, `reload`):

```rust
pub struct TaskView {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    task_id: i64,
    title: SharedString,
    state: tasks::TaskState,
    project_root: PathBuf,
    _editor_event_subscription: Subscription,
}
```

- `open_task` load closure: `let task = tasks::get_task(&connection, id)?.with_context(|| format!("no task with id {id}"))?; let description = tasks::load_description(&connection, id)?;`
- `save` writes description via `tasks::save_description` (title editing arrives in Task 9; `save` stays description-only until then).
- `reload` re-reads description via `tasks::load_description` and title via `tasks::get_task`, updates `self.title`, emits `EditorEvent::TitleChanged`.
- `tab_content_text` returns the title; `tab_icon` uses `IconName::ListTodo` (grep `ui::IconName` for the exact todo/check icon variant available; `SquareCheck`-style fallback fine — pick whichever exists, this is display-only).
- Render (header grows in Task 9; minimal now):

```rust
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
```

- No image resolver registration yet (Task 10). No `images` cache field yet.
- `zedgg_tasks_panel.rs` for now:

```rust
mod task_view;

use gpui::App;

pub use task_view::{TaskView, open_task};

pub fn init(_cx: &mut App) {}
```

- [ ] **Step 5: Run tests, verify pass** — `cargo test -p zedgg_tasks_panel`

- [ ] **Step 6: Clippy + commit**

```bash
git add Cargo.toml Cargo.lock crates/zedgg/tasks_panel
git commit -m "zedgg tasks: tasks_panel crate with TaskView detail tab"
```

---

### Task 6: `TasksPanel` dock + app wiring

**Files:**
- Create: `crates/zedgg/tasks_panel/src/panel.rs` (the `TasksPanel` — kept out of the lib root so the root stays the small registration surface, mirroring how `design_panel` would be split today; lib root re-exports)
- Modify: `crates/zedgg/tasks_panel/src/zedgg_tasks_panel.rs` (real `init`, actions, re-exports)
- Modify: `crates/zed/Cargo.toml` (`zedgg_tasks_panel.workspace = true # ZedGG`)
- Modify: `crates/zed/src/main.rs` (`zedgg_tasks_panel::init(cx); // ZedGG` next to `zedgg_design_panel::init(cx)`)

**Interfaces:**
- Consumes: `tasks::{list_tasks, create_task, delete_task, TaskRow, TaskState}`, `open_task`, `open_existing`/`open`.
- Produces:
  - `pub struct TasksPanel` implementing `workspace::dock::Panel` (position Left, `PANEL_KEY = "ZedGGTasksPanel"`, `KEY_CONTEXT = "ZedGGTasksPanel"`, activation_priority 17).
  - Actions in `zedgg_tasks`: `ToggleFocus`, `NewTask`, `Delete`, `OpenBoard` (board handler lands in Task 7; register the action now with a no-op TODO-free body that only focuses the panel — replaced in Task 7).
  - Test hook `root_override: Option<PathBuf>` + `refresh_root`, exactly like `DesignPanel`.

**Reference implementation:** `crates/zedgg/design_panel/src/zedgg_design_panel.rs`. Copy: `init` (observe_new + KeymapEventChannel rebinding + `add_panel` + ToggleFocus registration), `mutate`, `reload`/`refresh_root` generation guard, `EMPTY_MESSAGE` handling, `Panel` impl (including deferred `refresh_root` in `set_active`), uniform_list rendering, error row. Drop: tree depth/expansion, drag-move, import, rename-in-place (tasks rename from the tab in Task 9).

Panel-specific model (replaces the tree):

```rust
struct PanelRow {
    header: Option<TaskState>,      // Some = section header row
    task: Option<(i64, SharedString, Vec<i64>)>, // id, title, tag_ids
}
```

built by grouping `list_tasks` under the four states in `TaskState::ALL` order (headers always shown, even for empty groups). Clicking a task row calls `open_task`; clicking a header toggles collapse (a `HashSet<TaskState>`-equivalent using `u8` bitmask or `HashSet<&'static str>` — use `HashSet<TaskState>` after deriving `Hash` on `TaskState` in project_db).

`NewTask` behavior: `mutate` with `tasks::create_task(connection, "New task")`, then in the success callback `open_task` for the new id (title renamed later from the tab, Task 9 — until then rename via SQL or re-create; acceptable interim).

- [ ] **Step 1: Write the failing tests** (extend the Task 5 test module; panel construction mirrors `design_panel`'s `design_workspace` including `panel.root_override = Some(root)` + `refresh_root`)

```rust
#[gpui::test]
async fn test_panel_lists_tasks_grouped_and_click_opens_tab(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, panel, cx) = tasks_workspace(cx, dir.path()).await; // like design_workspace
    let (a, b) = {
        let c = open(dir.path()).unwrap();
        let a = tasks::create_task(&c, "draw tiles").unwrap();
        let b = tasks::create_task(&c, "fix jump").unwrap();
        tasks::move_task_between(&c, b, tasks::TaskState::Review, None, None).unwrap();
        (a, b)
    };
    panel.update(cx, |panel, cx| panel.reload(cx));
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        let rows: Vec<String> = panel.visible_row_labels(); // test helper: header label or title
        assert_eq!(rows, ["Backlog", "draw tiles", "In Progress", "Review", "fix jump", "Done"]);
    });

    panel.update_in(cx, |panel, window, cx| panel.click_task(a, window, cx));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, cx| {
        let view = workspace.items_of_type::<TaskView>(cx).next().expect("tab");
        assert_eq!(view.read(cx).task_id(), a);
    });
    let _ = b;
}

#[gpui::test]
async fn test_toggle_focus_opens_left_dock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, _panel, cx) = tasks_workspace(cx, dir.path()).await;
    workspace.update(cx, |workspace, cx| {
        assert!(!workspace.left_dock().read(cx).is_open());
    });
    cx.dispatch_action(ToggleFocus);
    workspace.update(cx, |workspace, cx| {
        assert!(workspace.left_dock().read(cx).is_open());
    });
}
```

(`visible_row_labels` and `click_task` are small `pub(crate)`/test-visible helpers on the panel — `click_task(id)` is also the real row on_click handler body.)

- [ ] **Step 2: Run, verify failure**

- [ ] **Step 3: Implement** panel.rs + init wiring as specified above.

- [ ] **Step 4: Run tests, verify pass** — `cargo test -p zedgg_tasks_panel`; also `cargo check -p zed` (main.rs wiring compiles).

- [ ] **Step 5: Clippy + commit**

```bash
git add Cargo.lock crates/zedgg/tasks_panel crates/zed
git commit -m "zedgg tasks: TasksPanel dock and app wiring"
```

---

### Task 7: `TaskBoard` tab (columns, cards, open, inline create)

**Files:**
- Create: `crates/zedgg/tasks_panel/src/board.rs`
- Modify: `crates/zedgg/tasks_panel/src/zedgg_tasks_panel.rs` (`mod board; pub use board::TaskBoard;`, register `OpenBoard` for real)

**Interfaces:**
- Consumes: `tasks::{list_tasks, list_tags, create_task, TaskRow, Tag, TaskState}`, `open_task`.
- Produces:
  - `pub struct TaskBoard` — workspace `Item` (`tab_content_text` = "Task Board", `buffer_kind` default, not dirty-able, no save).
  - `pub fn open_board(workspace: &mut Workspace, project_root: PathBuf, window: &mut Window, cx: &mut Context<Workspace>)` — activates the existing `TaskBoard` item if any pane has one, else adds one to the active pane.
  - `TaskBoard::reload(cx)` — re-reads tasks + tags off-thread (generation guard like the panel).
  - Test/board helpers used by Task 8's tests: `pub(crate) fn column(&self, state: TaskState) -> Vec<i64>` (card ids in order).

Board state: `columns: [Vec<CardRow>; 4]` where `CardRow { id: i64, title: SharedString, tags: Vec<(SharedString, SharedString)> /* name, color */ }`, plus `tags: HashMap<i64, Tag>` resolved at reload. Render: `h_flex` of four `v_flex` columns, each `.flex_1()` with header (`TaskState::label` + count + "+" IconButton) and a scrollable card list; each card a bordered `div` with title label + `h_flex` of tag chips (chip = small rounded div, `bg` from tag color via `gpui::Rgba::try_from(color_str)` with fallback to theme muted on parse failure). Card `on_click` → `open_task`. Column "+" → inline single-line editor row at the top of the column (same `EditState` pattern as the panel's pending row; Enter = `create_task` + `move_task_between` to that column edge when the column is not Backlog + `open_task`; Escape cancels).

The board re-activates and reloads on workspace item activation (`Item::added_to_workspace` + reload on `set_active`-equivalent: simplest correct hook is reloading in `TaskBoard::added_to_workspace` and after every mutation the board itself makes; panel-driven changes show after switching back to the tab via `Pane` activation events — subscribe to `workspace::Event` is NOT needed; instead the board reloads in `fn tab_content_text`? No — reload belongs in `Item::deactivated`/activation: use `cx.on_focus` of the board's focus handle to trigger reload, matching how the panel refreshes on `set_active`).

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn test_board_groups_cards_and_click_opens_tab(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, _panel, cx) = tasks_workspace(cx, dir.path()).await;
    let (a, b) = {
        let c = open(dir.path()).unwrap();
        let a = tasks::create_task(&c, "draw tiles").unwrap();
        let b = tasks::create_task(&c, "fix jump").unwrap();
        tasks::move_task_between(&c, b, tasks::TaskState::InProgress, None, None).unwrap();
        (a, b)
    };
    workspace.update_in(cx, |workspace, window, cx| {
        open_board(workspace, dir.path().to_path_buf(), window, cx);
    });
    cx.run_until_parked();

    let board = workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<TaskBoard>(cx).next().expect("board tab")
    });
    board.read_with(cx, |board, _| {
        assert_eq!(board.column(tasks::TaskState::Backlog), [a]);
        assert_eq!(board.column(tasks::TaskState::InProgress), [b]);
        assert_eq!(board.column(tasks::TaskState::Done), [] as [i64; 0]);
    });

    board.update_in(cx, |board, window, cx| board.open_card(a, window, cx));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, cx| {
        let view = workspace.items_of_type::<TaskView>(cx).next().expect("tab");
        assert_eq!(view.read(cx).task_id(), a);
    });

    // Second open_board re-activates, no duplicate.
    workspace.update_in(cx, |workspace, window, cx| {
        open_board(workspace, dir.path().to_path_buf(), window, cx);
    });
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(workspace.items_of_type::<TaskBoard>(cx).count(), 1);
    });
}
```

- [ ] **Step 2: Run, verify failure**

- [ ] **Step 3: Implement** board.rs as specified; `open_card(id, window, cx)` is the card click handler body (calls `open_task` with the board's stored `project_root` + workspace weak handle captured at construction).

- [ ] **Step 4: Run tests, verify pass**

- [ ] **Step 5: Clippy + commit**

```bash
git add crates/zedgg/tasks_panel
git commit -m "zedgg tasks: TaskBoard tab"
```

---

### Task 8: Board drag-and-drop

**Files:**
- Modify: `crates/zedgg/tasks_panel/src/board.rs`

**Interfaces:**
- Consumes: Task 2's `move_task_between`, Task 7's board state.
- Produces: `pub(crate) fn drop_card(&mut self, id: i64, state: TaskState, before: Option<i64>, window: &mut Window, cx: &mut Context<Self>)` — `before` = the card the drop landed ON (drop above it) or None for column end. Computes `(above, below)` neighbor ids from the CURRENT column vec (excluding the dragged card), then `mutate`-style background `move_task_between` + reload.

**Reference:** the design panel's node drag (`crates/zedgg/design_panel/src/zedgg_design_panel.rs`, grep `on_drag` / `drag_over` / `on_drop`) for the gpui drag typing pattern (a `Clone` payload struct + `.on_drag(payload, |payload, _, _, cx| cx.new(|_| payload.clone()))` ghost + `.drag_over::<CardDrag>(...)` styling + `.on_drop(cx.listener(...))`).

Drag payload:

```rust
#[derive(Clone)]
struct CardDrag {
    id: i64,
    title: SharedString,
}
```

Cards get `.on_drag(...)` + `.on_drop(...)` (drop above the hovered card); column bodies get `.on_drop(...)` for end-of-column drops.

- [ ] **Step 1: Write the failing test** (logic-level: call `drop_card` directly, the same body the on_drop closures call)

```rust
#[gpui::test]
async fn test_drop_card_moves_state_and_orders_within_column(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, _panel, cx) = tasks_workspace(cx, dir.path()).await;
    let (a, b, d) = {
        let c = open(dir.path()).unwrap();
        let a = tasks::create_task(&c, "a").unwrap();
        let b = tasks::create_task(&c, "b").unwrap();
        let d = tasks::create_task(&c, "d").unwrap();
        (a, b, d)
    };
    workspace.update_in(cx, |workspace, window, cx| {
        open_board(workspace, dir.path().to_path_buf(), window, cx);
    });
    cx.run_until_parked();
    let board = workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<TaskBoard>(cx).next().expect("board")
    });

    // d, b, a in Backlog (newest on top). Drag a to Review (empty column).
    board.update_in(cx, |board, window, cx| {
        board.drop_card(a, tasks::TaskState::Review, None, window, cx)
    });
    cx.run_until_parked();
    board.read_with(cx, |board, _| {
        assert_eq!(board.column(tasks::TaskState::Review), [a]);
        assert_eq!(board.column(tasks::TaskState::Backlog), [d, b]);
    });

    // Drag d below-target: drop ON b => lands above b.
    board.update_in(cx, |board, window, cx| {
        board.drop_card(d, tasks::TaskState::Backlog, Some(b), window, cx)
    });
    cx.run_until_parked();
    board.read_with(cx, |board, _| {
        assert_eq!(board.column(tasks::TaskState::Backlog), [d, b]);
    });

    // Drop b at Review column end => after a.
    board.update_in(cx, |board, window, cx| {
        board.drop_card(b, tasks::TaskState::Review, None, window, cx)
    });
    cx.run_until_parked();
    board.read_with(cx, |board, _| {
        assert_eq!(board.column(tasks::TaskState::Review), [a, b]);
    });
}
```

- [ ] **Step 2: Run, verify failure**

- [ ] **Step 3: Implement** `drop_card` (neighbor computation below) + wire `.on_drag`/`.on_drop` in render.

```rust
let column: Vec<i64> = self.column(state).into_iter().filter(|c| *c != id).collect();
let (above, below) = match before {
    Some(before) => {
        let index = column.iter().position(|c| *c == before);
        match index {
            Some(index) => (index.checked_sub(1).map(|i| column[i]), Some(column[index])),
            None => (column.last().copied(), None),
        }
    }
    None => (column.last().copied(), None),
};
```

- [ ] **Step 4: Run tests, verify pass**

- [ ] **Step 5: Clippy + commit**

```bash
git add crates/zedgg/tasks_panel
git commit -m "zedgg tasks: board drag and drop"
```

---

### Task 9: TaskView header — title edit, state dropdown, tags

**Files:**
- Modify: `crates/zedgg/tasks_panel/src/task_view.rs`
- Modify: `crates/zedgg/tasks_panel/src/zedgg_tasks_panel.rs` (retitle propagation helper if needed)

**Interfaces:**
- Consumes: `tasks::{rename_task, move_task_between, list_tags, create_tag, assign_tag, unassign_tag, Tag, TaskState}`.
- Produces:
  - Title: click the title label → swaps to a single-line `Editor` (the panel's `EditState` pattern); Enter commits — sets `self.title`, marks a `title_dirty: bool`, emits `TitleChanged`. `save` now writes BOTH: `rename_task` when `title_dirty` + `save_description`, clearing both dirty bits; `is_dirty = buffer.is_dirty() || title_dirty`.
  - State: a `ui::DropdownMenu`/`PopoverMenu` (grep `ui::` for the current dropdown component; `ContextMenu`-on-click is acceptable and already used by design_panel) listing `TaskState::ALL` labels; selecting runs a background `move_task_between(id, state, None, None)` (end of target column) then updates `self.state`.
  - Tags: chip row shows the task's tags (name on color); each chip has a small ✕ (unassign). An "+ tag" button opens a `ContextMenu` listing all existing tags (click = assign) plus a free-text entry row (single-line editor; Enter = `create_tag(name, next_palette_color)` + assign). Palette:

```rust
const TAG_COLORS: [&str; 8] = [
    "#e06c75", "#61afef", "#98c379", "#e5c07b",
    "#c678dd", "#56b6c2", "#d19a66", "#abb2bf",
];
// next color: TAG_COLORS[list_tags(connection)?.len() % TAG_COLORS.len()]
```

  - All three mutations follow the `mutate` shape: background write on `open(&project_root)`, error string surfaced (add `error: Option<SharedString>` to `TaskView`, rendered as a thin red row under the header like the panel's error row), then `reload`.

- [ ] **Step 1: Write the failing tests**

```rust
#[gpui::test]
async fn test_title_edit_saves_with_description(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, cx) = task_workspace(cx).await;
    let id = { tasks::create_task(&open(dir.path()).unwrap(), "old").unwrap() };
    workspace.update_in(cx, |_, window, cx| {
        open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
    });
    cx.run_until_parked();
    let view = workspace.read_with(cx, |w, cx| w.items_of_type::<TaskView>(cx).next().unwrap());

    view.update_in(cx, |view, window, cx| view.set_title_for_save("new title".into(), window, cx));
    view.read_with(cx, |view, cx| assert!(view.is_dirty(cx), "title change dirties"));
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let c = open(dir.path()).unwrap();
    assert_eq!(tasks::get_task(&c, id).unwrap().unwrap().title, "new title");
    view.read_with(cx, |view, cx| {
        assert!(!view.is_dirty(cx));
        assert_eq!(view.tab_content_text(0, cx), "new title");
    });
}

#[gpui::test]
async fn test_state_change_and_tag_assign_from_view(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, cx) = task_workspace(cx).await;
    let id = { tasks::create_task(&open(dir.path()).unwrap(), "t").unwrap() };
    workspace.update_in(cx, |_, window, cx| {
        open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
    });
    cx.run_until_parked();
    let view = workspace.read_with(cx, |w, cx| w.items_of_type::<TaskView>(cx).next().unwrap());

    view.update_in(cx, |view, window, cx| {
        view.set_state(tasks::TaskState::InProgress, window, cx)
    });
    cx.run_until_parked();
    let c = open(dir.path()).unwrap();
    assert_eq!(
        tasks::get_task(&c, id).unwrap().unwrap().state,
        tasks::TaskState::InProgress
    );

    view.update_in(cx, |view, window, cx| view.add_tag("art".into(), window, cx));
    cx.run_until_parked();
    let c = open(dir.path()).unwrap();
    let tags = tasks::list_tags(&c).unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "art");
    assert_eq!(
        tasks::get_task(&c, id).unwrap().unwrap().tag_ids,
        [tags[0].id]
    );
    view.read_with(cx, |view, _| assert_eq!(view.tag_names(), ["art"]));
}
```

(`set_title_for_save`, `set_state`, `add_tag`, `tag_names` are the real handler bodies the header widgets call; public for tests like the panel's helpers.)

- [ ] **Step 2: Run, verify failure**

- [ ] **Step 3: Implement** as specified. `TaskView` gains `title_dirty: bool`, `tags: Vec<Tag>`, `error: Option<SharedString>`; `reload` refreshes tags too.

- [ ] **Step 4: Run tests, verify pass** (all crate tests)

- [ ] **Step 5: Clippy + commit**

```bash
git add crates/zedgg/tasks_panel
git commit -m "zedgg tasks: TaskView header with title, state, tags"
```

---

### Task 10: Attachments UI + markdown image resolver

**Files:**
- Modify: `crates/zedgg/tasks_panel/src/task_view.rs`

**Interfaces:**
- Consumes: `tasks::{add_file, list_files, delete_file, load_file_by_name, TaskFile}`, `markdown_preview::{set_buffer_image_resolver, remove_buffer_image_resolver, BufferImageResolver}`.
- Produces: attachments row in the header — file names as chips with ✕ (delete, confirm via `ggo_common::confirm_destructive_cascade` with empty cascade list), an import IconButton opening the OS file picker (`cx.prompt_for_paths` via the same `PathPromptOptions` flow as the design panel's `import_files`), and drag-drop of `ExternalPaths` onto the view root importing too. `![](shot.png)` in the description preview resolves through the task's files.

**Reference:** `doc_view.rs`'s `image_resolver`/`load_image`/`image_format` — copy, swapping the lookup:

```rust
fn load_image(project_root: &Path, task_id: i64, reference: &str) -> Result<Option<Arc<Image>>> {
    let connection = open_db(project_root)?;
    let Some(format) = image_format(reference) else {
        return Ok(None);
    };
    let Some(bytes) = tasks::load_file_by_name(&connection, task_id, reference)? else {
        return Ok(None);
    };
    Ok(Some(Arc::new(Image::from_bytes(format, bytes))))
}
```

(`image_format` keyed off the reference's own extension — same table as doc_view's.) Resolver registered per buffer in `TaskView::new`, removed `on_release`, with the same per-view `ImageCache` including negative caching, and cache cleared after every import/delete.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn test_import_lists_attachment_and_resolver_finds_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, cx) = task_workspace(cx).await;
    let id = { tasks::create_task(&open(dir.path()).unwrap(), "t").unwrap() };
    workspace.update_in(cx, |_, window, cx| {
        open_task(cx.weak_entity(), dir.path().to_path_buf(), id, window, cx);
    });
    cx.run_until_parked();
    let view = workspace.read_with(cx, |w, cx| w.items_of_type::<TaskView>(cx).next().unwrap());

    let png = dir.path().join("shot.png");
    std::fs::write(&png, b"\x89PNGdata").unwrap();
    view.update_in(cx, |view, window, cx| view.import_paths(vec![png], window, cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_eq!(view.attachment_names(), ["shot.png"]));

    let resolver = view.read_with(cx, |view, cx| {
        markdown_preview::buffer_image_resolver(cx, view.buffer().entity_id())
            .expect("resolver registered")
    });
    assert!(resolver("shot.png").is_some());
    assert!(resolver("missing.png").is_none());
    assert!(resolver("https://x/y.png").is_none(), "urls fall through");
}
```

- [ ] **Step 2: Run, verify failure**

- [ ] **Step 3: Implement** as specified (`import_paths`, `attachment_names`, `buffer()` accessor).

- [ ] **Step 4: Run tests, verify pass**

- [ ] **Step 5: Clippy + commit**

```bash
git add crates/zedgg/tasks_panel
git commit -m "zedgg tasks: attachments and preview image resolver"
```

---

### Task 11: Deletion + tab hygiene + final sweep

**Files:**
- Modify: `crates/zedgg/tasks_panel/src/panel.rs` (Delete action: confirm + `delete_task` + close open tab)
- Modify: `crates/zedgg/tasks_panel/src/board.rs` (card context menu: Delete, same path)
- Modify: `crates/zedgg/tasks_panel/src/zedgg_tasks_panel.rs` (shared `close_task_tabs(workspace, id)` helper)

**Interfaces:**
- Consumes: everything prior.
- Produces: `pub(crate) fn close_task_tabs(workspace: &mut Workspace, task_id: i64, window: &mut Window, cx: &mut Context<Workspace>)` — finds `TaskView` items with that id and closes them without save prompts (`Pane::remove_item`-based, grep design_panel's delete flow for the exact call used when a doc is deleted under an open tab; mirror it — if design_panel does NOT close tabs on delete, match tasks to the same behavior and let the tab's save fail visibly instead, per spec "tab save-after-delete errors visibly". In that case `close_task_tabs` is not built and this task only adds the confirms).

Check `design_panel`'s `delete_selected` first; whichever contract it implements, tasks copies. The spec allows either (it names the visible-error contract).

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn test_delete_task_from_panel_confirms_and_save_after_delete_errors(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, panel, cx) = tasks_workspace(cx, dir.path()).await;
    let id = { tasks::create_task(&open(dir.path()).unwrap(), "doomed").unwrap() };
    panel.update(cx, |panel, cx| panel.reload(cx));
    cx.run_until_parked();
    panel.update_in(cx, |panel, window, cx| panel.click_task(id, window, cx));
    cx.run_until_parked();

    panel.update_in(cx, |panel, window, cx| panel.delete_task(id, window, cx));
    cx.run_until_parked();
    cx.simulate_prompt_answer("Delete"); // ggo_common confirm dialog
    cx.run_until_parked();
    let c = open(dir.path()).unwrap();
    assert!(tasks::get_task(&c, id).unwrap().is_none());
}
```

(Adjust the prompt-answer call to whatever `design_panel`'s delete test does — copy its exact simulation calls.)

- [ ] **Step 2: Run, verify failure**

- [ ] **Step 3: Implement**, mirroring design_panel's delete flow verbatim.

- [ ] **Step 4: Full verification sweep**

Run: `cargo test -p zedgg_project_db -p zedgg_tasks_panel && cargo test -p zed --bin zed test_quick_action_bar && ./script/clippy -p zedgg_project_db -p zedgg_tasks_panel`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/zedgg/tasks_panel
git commit -m "zedgg tasks: deletion flows"
```

---

## Self-Review Notes (already applied)

- Spec coverage: states/schema (T1), rank+move (T2), tags (T3), files (T4), detail tab + markdown editor + preview compatibility (T5, eye button needs no work — act_as + Markdown language), dock panel (T6), board + inline create (T7), drag-drop (T8), header metadata (T9), attachments + image resolver (T10), delete + error contracts (T11). Out-of-scope list untouched.
- The board's "+" inline create and the panel's `NewTask` both funnel into `tasks::create_task`; titles are editable from the tab as of T9.
- Type names consistent across tasks: `TaskState`, `TaskRow`, `Tag`, `TaskFile`, `move_task_between`, `open_task`, `open_board`, `drop_card`.
