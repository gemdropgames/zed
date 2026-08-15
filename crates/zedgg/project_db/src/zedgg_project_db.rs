//! `zedgg.sqlite` -- the ZedGG project-management database that lives at a
//! project's root and travels with it in git.
//!
//! This crate is the schema authority for that file: every ZedGG tool that
//! stores project data (design docs today; tasks, milestones, ... later)
//! appends a migration to [`MIGRATIONS`] here and calls [`open`]. Nothing
//! else may `CREATE TABLE` in `zedgg.sqlite`.
//!
//! Deliberately NOT ggo-db/turso: `~/.ggo/ggo_ide.db` is a per-user cache,
//! this is project data. `sqlez` (Zed's own libsqlite3 wrapper) writes a
//! plain SQLite file in the default DELETE journal mode, so a commit
//! contains one file, no `-wal`/`-shm` siblings.
//!
//! Connections are cheap and `Send`; the intended shape is open -> do one
//! operation -> drop, inside a background task. No long-lived handle exists,
//! so a `git checkout` swapping the file underneath is seen by the very
//! next operation.

pub mod design_docs;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
pub use sqlez::connection::Connection;

pub const DB_FILE: &str = "zedgg.sqlite";

/// The sqlez `migrate` domain. One domain for the whole file: migrations
/// are ordered across tools, not per tool.
const DOMAIN: &str = "zedgg";

/// Append-only. sqlez stores each applied migration's text and refuses to
/// start if an already-applied entry changed, so edit history by adding a
/// new step, never by rewriting an old one.
const MIGRATIONS: &[&str] = &[design_docs::MIGRATION];

pub fn db_path(project_root: &Path) -> PathBuf {
    project_root.join(DB_FILE)
}

/// Create-or-open `<root>/zedgg.sqlite` and bring it to the latest schema.
pub fn open(project_root: &Path) -> Result<Connection> {
    let path = db_path(project_root);
    let uri = path
        .to_str()
        .with_context(|| format!("non-UTF-8 database path {}", path.display()))?;
    let connection = Connection::open_file(uri);
    // sqlez's `open_file` silently falls back to a shared in-memory
    // database when the file can't be opened. For a per-user cache that is
    // a fine degradation; for project data it would be silent data loss,
    // so refuse.
    anyhow::ensure!(
        connection.persistent(),
        "could not open {} (read-only directory, or file is not a SQLite database?)",
        path.display()
    );
    connection.exec("PRAGMA foreign_keys = ON")?()?;
    connection
        .migrate(DOMAIN, MIGRATIONS, &mut |_, _, _| false)
        .with_context(|| format!("migrating {}", path.display()))?;
    Ok(connection)
}

/// Like [`open`], but `None` when the file does not exist yet. Readers use
/// this so that merely browsing a project in ZedGG never creates the file;
/// the first write goes through [`open`].
pub fn open_existing(project_root: &Path) -> Result<Option<Connection>> {
    if db_path(project_root).is_file() {
        open(project_root).map(Some)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
pub(crate) fn open_memory(name: &str) -> Connection {
    let connection = Connection::open_memory(Some(name));
    connection.exec("PRAGMA foreign_keys = ON").unwrap()().unwrap();
    connection
        .migrate(DOMAIN, MIGRATIONS, &mut |_, _, _| false)
        .unwrap();
    connection
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_existing(dir.path()).unwrap().is_none());
        assert!(!db_path(dir.path()).exists());

        open(dir.path()).unwrap();
        assert!(db_path(dir.path()).is_file());

        // Second open re-runs migrate against an up-to-date file: no-op.
        let connection = open_existing(dir.path()).unwrap().expect("file exists now");
        let root: Option<String> = connection
            .select_row("SELECT name FROM design_nodes WHERE id = 1")
            .unwrap()()
        .unwrap();
        assert_eq!(root.as_deref(), Some("Design Docs"));

        // Default journal mode: single file, nothing for git to trip on.
        assert!(!dir.path().join("zedgg.sqlite-wal").exists());
    }

    #[test]
    fn open_refuses_unwritable_location() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        assert!(open(&missing).is_err());
    }
}
