//! Tasks: a fixed four-state kanban stored in `zedgg.sqlite`.
//! Plain functions on a [`Connection`], mirroring `design_docs`.

use anyhow::{Context as _, Result, bail};
use sqlez::connection::Connection;
use std::collections::HashMap;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFile {
    pub id: i64,
    pub name: String,
}

type TasksSelectRow = (i64, String, i64, String);

fn task_from_row((id, state, rank, title): TasksSelectRow) -> Result<TaskRow> {
    Ok(TaskRow {
        id,
        state: TaskState::parse(&state)?,
        rank,
        title,
        tag_ids: Vec::new(),
    })
}

pub fn list_tasks(connection: &Connection) -> Result<Vec<TaskRow>> {
    let tag_map = tag_ids_by_task(connection)?;
    connection.select::<TasksSelectRow>(
        "SELECT id, state, rank, title FROM tasks \
         ORDER BY CASE state \
             WHEN 'backlog' THEN 0 WHEN 'in_progress' THEN 1 \
             WHEN 'review' THEN 2 WHEN 'done' THEN 3 END, rank, id",
    )?()?
    .into_iter()
    .map(|row| {
        let mut task = task_from_row(row)?;
        task.tag_ids = tag_map.get(&task.id).cloned().unwrap_or_default();
        Ok(task)
    })
    .collect()
}

pub fn get_task(connection: &Connection, id: i64) -> Result<Option<TaskRow>> {
    let tag_map = tag_ids_by_task(connection)?;
    connection
        .select_row_bound::<i64, TasksSelectRow>(
            "SELECT id, state, rank, title FROM tasks WHERE id = ?",
        )?(id)?
        .map(|row| {
            let mut task = task_from_row(row)?;
            task.tag_ids = tag_map.get(&task.id).cloned().unwrap_or_default();
            Ok(task)
        })
        .transpose()
}

fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty() {
        bail!("task title may not be empty");
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("tag name may not be empty");
    }
    Ok(())
}

fn validate_file_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("file name must not be empty");
    }
    if name.contains(['/', '\\']) || name == "." || name == ".." {
        bail!("file name {name:?} may not contain path separators or be `.`/`..`");
    }
    Ok(())
}

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

pub fn rename_task(connection: &Connection, id: i64, title: &str) -> Result<()> {
    validate_title(title)?;
    match get_task(connection, id)? {
        Some(_) => {}
        None => bail!("task {id} no longer exists"),
    }
    connection.exec_bound::<(&str, i64)>(
        "UPDATE tasks SET title = ?, updated_at = datetime('now') WHERE id = ?",
    )?((title, id))
}

pub fn delete_task(connection: &Connection, id: i64) -> Result<()> {
    connection.exec_bound::<i64>("DELETE FROM tasks WHERE id = ?")?(id)
}

pub fn load_description(connection: &Connection, id: i64) -> Result<String> {
    connection
        .select_row_bound::<i64, Option<String>>(
            "SELECT description FROM tasks WHERE id = ?",
        )?(id)?
        .flatten()
        .with_context(|| format!("no task with id {id}"))
}

pub fn save_description(connection: &Connection, id: i64, description: &str) -> Result<()> {
    match get_task(connection, id)? {
        Some(_) => {}
        None => bail!("task {id} no longer exists"),
    }
    connection.exec_bound::<(&str, i64)>(
        "UPDATE tasks SET description = ?, updated_at = datetime('now') WHERE id = ?",
    )?((description, id))
}

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
    match get_task(connection, id)? {
        Some(_) => {}
        None => bail!("task {id} no longer exists"),
    }
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
        (None, None) => connection
            .select_row_bound::<&str, i64>(
                "SELECT COALESCE((SELECT MAX(rank) FROM tasks WHERE state = ?), -1000) + 1000",
            )?(state.as_str())?
            .context("SELECT COALESCE(...) produced no row")?,
    };
    connection.exec_bound::<(&str, i64, i64)>(
        "UPDATE tasks SET state = ?, rank = ?, updated_at = datetime('now') WHERE id = ?",
    )?((state.as_str(), rank, id))?;
    Ok(())
}

fn renumber_column(connection: &Connection, state: TaskState) -> Result<()> {
    connection.exec_bound::<&str>(
        "WITH ordered AS (SELECT id, ROW_NUMBER() OVER (ORDER BY rank) AS n \
         FROM tasks WHERE state = ?1) \
         UPDATE tasks SET rank = (SELECT n * 1000 FROM ordered WHERE ordered.id = tasks.id) \
         WHERE id IN (SELECT id FROM ordered)",
    )?(state.as_str())?;
    Ok(())
}

pub fn create_tag(connection: &Connection, name: &str, color: &str) -> Result<i64> {
    validate_name(name)?;
    connection
        .select_row_bound::<(&str, &str), i64>(
            "INSERT INTO task_tags (name, color) VALUES (?, ?) RETURNING id",
        )?((name, color))?
        .context("INSERT ... RETURNING id produced no row")
}

/// Creates a tag named `name`, colored by cycling through `colors` in
/// creation order (`colors[list_tags(connection)?.len() % colors.len()]`).
/// The count-read and the insert are two statements on one `connection`,
/// not one atomic transaction -- two *different* connections racing this
/// at the same instant could still pick the same color (each reads the
/// count before either has committed its insert). Acceptable for a
/// single-user desktop app; a caller needing true atomicity across
/// concurrent writers would need to wrap both in one `BEGIN IMMEDIATE`.
pub fn create_tag_with_next_color(
    connection: &Connection,
    name: &str,
    colors: &[&str],
) -> Result<i64> {
    if colors.is_empty() {
        bail!("colors must not be empty");
    }
    let color = colors[list_tags(connection)?.len() % colors.len()];
    create_tag(connection, name, color)
}

pub fn list_tags(connection: &Connection) -> Result<Vec<Tag>> {
    connection.select::<(i64, String, String)>(
        "SELECT id, name, color FROM task_tags ORDER BY name COLLATE NOCASE",
    )?()?
    .into_iter()
    .map(|(id, name, color)| Ok(Tag { id, name, color }))
    .collect()
}

pub fn assign_tag(connection: &Connection, task_id: i64, tag_id: i64) -> Result<()> {
    connection.exec_bound::<(i64, i64)>(
        "INSERT OR IGNORE INTO task_tag_assignments (task_id, tag_id) VALUES (?, ?)",
    )?((task_id, tag_id))
}

pub fn unassign_tag(connection: &Connection, task_id: i64, tag_id: i64) -> Result<()> {
    connection.exec_bound::<(i64, i64)>(
        "DELETE FROM task_tag_assignments WHERE task_id = ? AND tag_id = ?",
    )?((task_id, tag_id))
}

pub fn delete_tag(connection: &Connection, tag_id: i64) -> Result<()> {
    connection.exec_bound::<i64>("DELETE FROM task_tags WHERE id = ?")?(tag_id)
}

pub fn add_file(connection: &Connection, task_id: i64, name: &str, data: &[u8]) -> Result<i64> {
    validate_file_name(name)?;
    connection
        .select_row_bound::<(i64, &str, &[u8]), i64>(
            "INSERT INTO task_files (task_id, name, data) VALUES (?, ?, ?) RETURNING id",
        )?((task_id, name, data))?
        .context("INSERT ... RETURNING id produced no row")
}

pub fn list_files(connection: &Connection, task_id: i64) -> Result<Vec<TaskFile>> {
    connection.select_bound::<i64, (i64, String)>(
        "SELECT id, name FROM task_files WHERE task_id = ? ORDER BY name COLLATE NOCASE",
    )?(task_id)?
    .into_iter()
    .map(|(id, name)| Ok(TaskFile { id, name }))
    .collect()
}

pub fn load_file_by_name(connection: &Connection, task_id: i64, name: &str) -> Result<Option<Vec<u8>>> {
    connection.select_row_bound::<(i64, &str), Option<Vec<u8>>>(
        "SELECT data FROM task_files WHERE task_id = ? AND name = ?",
    )?((task_id, name))?
    .flatten()
    .map(Ok)
    .transpose()
}

pub fn load_file(connection: &Connection, file_id: i64) -> Result<Vec<u8>> {
    connection
        .select_row_bound::<i64, Option<Vec<u8>>>(
            "SELECT data FROM task_files WHERE id = ?",
        )?(file_id)?
        .flatten()
        .with_context(|| format!("no task file with id {file_id}"))
}

pub fn delete_file(connection: &Connection, file_id: i64) -> Result<()> {
    connection.exec_bound::<i64>("DELETE FROM task_files WHERE id = ?")?(file_id)
}

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

    #[test]
    fn move_between_neighbors_takes_the_midpoint() {
        let c = open_memory("tasks_move_mid");
        let a = create_task(&c, "a").unwrap();
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
    fn move_with_no_neighbors_into_nonempty_column_lands_last() {
        let c = open_memory("tasks_move_none_none");
        let a = create_task(&c, "a").unwrap();
        let b = create_task(&c, "b").unwrap();
        move_task_between(&c, a, TaskState::Review, None, None).unwrap();
        move_task_between(&c, b, TaskState::Review, None, None).unwrap();
        let rows = list_tasks(&c).unwrap();
        let rank_of = |id: i64| rows.iter().find(|t| t.id == id).unwrap().rank;
        assert!(
            rank_of(b) > rank_of(a),
            "(None, None) into a non-empty column must land after the existing tasks, not at rank 0"
        );
        let review: Vec<i64> = rows
            .iter()
            .filter(|t| t.state == TaskState::Review)
            .map(|t| t.id)
            .collect();
        assert_eq!(review, [a, b]);
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

    #[test]
    fn move_nonexistent_task_fails() {
        let c = open_memory("tasks_move_missing");
        let a = create_task(&c, "a").unwrap();
        let missing_id = 9999;
        assert!(move_task_between(&c, missing_id, TaskState::Backlog, None, Some(a)).is_err());
    }

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

    #[test]
    fn create_tag_with_next_color_cycles_the_palette() {
        let c = open_memory("tasks_tag_palette");
        let colors = ["#a", "#b", "#c"];
        let first = create_tag_with_next_color(&c, "one", &colors).unwrap();
        let second = create_tag_with_next_color(&c, "two", &colors).unwrap();
        let third = create_tag_with_next_color(&c, "three", &colors).unwrap();
        let fourth = create_tag_with_next_color(&c, "four", &colors).unwrap();
        let color_of = |id: i64| list_tags(&c).unwrap().into_iter().find(|t| t.id == id).unwrap().color;
        assert_eq!(color_of(first), "#a");
        assert_eq!(color_of(second), "#b");
        assert_eq!(color_of(third), "#c");
        assert_eq!(color_of(fourth), "#a", "wraps back around");
        assert!(create_tag_with_next_color(&c, "five", &[]).is_err());
    }

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
}
