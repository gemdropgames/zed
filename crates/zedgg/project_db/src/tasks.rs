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
    connection.select::<TasksSelectRow>(
        "SELECT id, state, rank, title FROM tasks \
         ORDER BY CASE state \
             WHEN 'backlog' THEN 0 WHEN 'in_progress' THEN 1 \
             WHEN 'review' THEN 2 WHEN 'done' THEN 3 END, rank",
    )?()?
    .into_iter()
    .map(task_from_row)
    .collect()
}

pub fn get_task(connection: &Connection, id: i64) -> Result<Option<TaskRow>> {
    connection
        .select_row_bound::<i64, TasksSelectRow>(
            "SELECT id, state, rank, title FROM tasks WHERE id = ?",
        )?(id)?
        .map(task_from_row)
        .transpose()
}

fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty() {
        bail!("task title may not be empty");
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
