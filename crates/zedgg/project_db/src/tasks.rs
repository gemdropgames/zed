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
        (None, None) => 0,
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

    #[test]
    fn move_nonexistent_task_fails() {
        let c = open_memory("tasks_move_missing");
        let a = create_task(&c, "a").unwrap();
        let missing_id = 9999;
        assert!(move_task_between(&c, missing_id, TaskState::Backlog, None, Some(a)).is_err());
    }
}
