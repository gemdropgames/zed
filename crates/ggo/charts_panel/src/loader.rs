//! Off-thread perf-run listing: reads `~/.ggo/ggo_ide.db`'s `run`/`cart`
//! tables directly -- the charts-panel analog of `ggo_world_panel::loader`
//! / `ggo_metasprite_panel::loader` (same one-shot background-pass
//! framing), reading the SAME database ggo-ide's `backend/db.rs` opens
//! and `backend/perf.rs::carts`/`cart_runs` query (read those two files
//! before touching this one -- this is a deliberately narrower port: one
//! flat "every run, newest first" query across all carts, id/date/cart
//! label, for the panel's run picker; `run_detail`/`run_frames` (the
//! per-run chart data) are C2's job).
//!
//! Unlike those loaders (a filesystem walk), this one opens a `turso`
//! connection, which needs an async runtime underneath. `ggo-ide`'s own
//! `backend/db.rs`/`backend/perf.rs` solve this by spinning up a private
//! single-threaded tokio runtime per call and blocking on it; [`list_runs`]
//! does the exact same thing. That block is safe here because it only
//! ever runs inside `cx.background_spawn` -- a background-executor
//! thread, not the UI thread (same rule `ggo_world_panel::loader::load_world`
//! leans on for its own synchronous fs walk).

use std::path::{Path, PathBuf};

use turso::Value;

/// Database filename under `~/.ggo/`, matching `ggo-ide`'s
/// `backend/db.rs::DB_FILE` exactly -- this reads the SAME file, not a
/// copy. Mirrored as a literal rather than imported: ggo-ide is a
/// separate (Iced/Tauri-facing) crate this fork does not depend on.
const DB_FILE: &str = "ggo_ide.db";
const DOT_GGO: &str = ".ggo";

/// `~/.ggo/ggo_ide.db`, matching `ggo-ide`'s `backend/db.rs::default_db_path`.
/// `None` only if neither `HOME` nor `USERPROFILE` resolves (mirrors that
/// function's `anyhow` error, downgraded to `Option` here since the
/// panel's only use for this is "where to look", not something worth a
/// hard error -- an unresolvable home directory reads the same as "no db
/// yet" to the picker).
pub fn default_db_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(DOT_GGO).join(DB_FILE))
}

/// One row of the runs list: enough to identify a run in the picker. C2
/// adds the per-run chart data (`run_frames`) once one is selected.
#[derive(Debug, Clone, PartialEq)]
pub struct RunListing {
    pub id: i64,
    pub started_at: String,
    pub cart_name: String,
    pub label: Option<String>,
}

impl RunListing {
    /// The picker's display text: `"<cart> — <label>"` when the run has
    /// a non-empty label, else just the cart name -- mirrors how
    /// `RunPage.tsx`'s row title falls back on the TS side.
    pub fn display_title(&self) -> String {
        match self.label.as_deref() {
            Some(label) if !label.is_empty() => format!("{} — {label}", self.cart_name),
            _ => self.cart_name.clone(),
        }
    }
}

/// One raw db row (`id, started_at, cart_name, label`, in that column
/// order) -> a [`RunListing`], or `None` if the row is short or the
/// leading columns aren't the types the schema promises. Pure -- no I/O
/// -- so it is unit-tested directly against hand-built `turso::Value`
/// rows below, no db required.
fn row_to_listing(row: &[Value]) -> Option<RunListing> {
    let [id, started_at, cart_name, label] = row else {
        return None;
    };
    let Value::Integer(id) = id else {
        return None;
    };
    let Value::Text(started_at) = started_at else {
        return None;
    };
    let Value::Text(cart_name) = cart_name else {
        return None;
    };
    let label = match label {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    };
    Some(RunListing {
        id: *id,
        started_at: started_at.clone(),
        cart_name: cart_name.clone(),
        label,
    })
}

/// Every run across every cart, newest first (`started_at DESC, id DESC`
/// -- same tie-break `perf.rs::cart_runs_async` uses) -- the panel's
/// picker feed.
///
/// A missing db file reads as a clean empty list, not an error: the file
/// doesn't exist until `ggo-ide`, `ggo-emu`, or `ggo-server` first
/// ingests a run, and that's an ordinary "nothing to show yet" state for
/// a freshly opened project, not a failure. This MUST be checked BEFORE
/// opening -- `turso::Builder::new_local` creates an empty, tableless
/// file on open otherwise, which would then fail the `SELECT` with "no
/// such table: run" instead of reading as "no runs yet".
pub fn list_runs(db_path: &Path) -> Result<Vec<RunListing>, String> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(list_runs_async(db_path))
}

async fn list_runs_async(db_path: &Path) -> Result<Vec<RunListing>, String> {
    let db = turso::Builder::new_local(&db_path.to_string_lossy())
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let conn = db.connect().map_err(|e| e.to_string())?;
    conn.busy_timeout(ggo_db::BUSY_TIMEOUT)
        .map_err(|e| e.to_string())?;

    let mut rows = conn
        .query(
            "SELECT r.id, r.started_at, c.name, r.label
             FROM run r JOIN cart c ON c.id = r.cart_id
             ORDER BY r.started_at DESC, r.id DESC",
            (),
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(|e| e.to_string())? {
        let n = row.column_count();
        let mut vals = Vec::with_capacity(n);
        for i in 0..n {
            vals.push(row.get_value(i).map_err(|e| e.to_string())?);
        }
        if let Some(listing) = row_to_listing(&vals) {
            out.push(listing);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_to_listing_parses_a_labeled_run() {
        let row = vec![
            Value::Integer(7),
            Value::Text("2026-08-01T00:00:00Z".to_string()),
            Value::Text("demo".to_string()),
            Value::Text("arena".to_string()),
        ];
        let listing = row_to_listing(&row).expect("row should parse");
        assert_eq!(listing.id, 7);
        assert_eq!(listing.started_at, "2026-08-01T00:00:00Z");
        assert_eq!(listing.cart_name, "demo");
        assert_eq!(listing.label.as_deref(), Some("arena"));
    }

    #[test]
    fn row_to_listing_treats_a_null_label_as_none() {
        let row = vec![
            Value::Integer(1),
            Value::Text("2026-08-01T00:00:00Z".to_string()),
            Value::Text("demo".to_string()),
            Value::Null,
        ];
        assert_eq!(row_to_listing(&row).unwrap().label, None);
    }

    #[test]
    fn row_to_listing_rejects_a_short_or_mistyped_row() {
        assert_eq!(row_to_listing(&[]), None);
        assert_eq!(row_to_listing(&[Value::Integer(1)]), None);
        assert_eq!(
            row_to_listing(&[
                Value::Text("not-an-id".to_string()),
                Value::Text("2026-08-01T00:00:00Z".to_string()),
                Value::Text("demo".to_string()),
                Value::Null,
            ]),
            None
        );
    }

    #[test]
    fn display_title_falls_back_to_cart_name_when_unlabeled_or_empty() {
        let unlabeled = RunListing {
            id: 1,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            cart_name: "demo".to_string(),
            label: None,
        };
        assert_eq!(unlabeled.display_title(), "demo");

        let empty_label = RunListing {
            label: Some(String::new()),
            ..unlabeled
        };
        assert_eq!(empty_label.display_title(), "demo");
    }

    #[test]
    fn display_title_shows_the_label_when_present() {
        let listing = RunListing {
            id: 1,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            cart_name: "demo".to_string(),
            label: Some("arena".to_string()),
        };
        assert_eq!(listing.display_title(), "demo — arena");
    }

    #[test]
    fn list_runs_returns_empty_for_a_missing_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.db");
        assert_eq!(list_runs(&missing).unwrap(), Vec::new());
        assert!(
            !missing.exists(),
            "a missing-file read must not create the db file"
        );
    }

    /// A fixture db authored through `ggo_db::migrate` (the real schema,
    /// same pattern ggo-ide's `backend/perf.rs` tests use), seeded with
    /// one cart and two runs -- pins both the newest-first ordering and
    /// the label/no-label column mapping in one pass.
    #[test]
    fn list_runs_reads_seeded_rows_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = ggo_db::open(&path).await.unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO cart (id, name) VALUES (1, 'demo')", ())
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', 3, 'first')",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO run (id, cart_id, started_at, frames, label)
                 VALUES (2, 1, '2026-08-02T00:00:00Z', 2, NULL)",
                (),
            )
            .await
            .unwrap();
        });

        let runs = list_runs(&path).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, 2, "newest started_at sorts first");
        assert_eq!(runs[0].label, None);
        assert_eq!(runs[1].id, 1);
        assert_eq!(runs[1].label.as_deref(), Some("first"));
    }

    #[test]
    fn list_runs_is_empty_for_a_freshly_migrated_db_with_no_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggo_ide.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            ggo_db::open(&path).await.unwrap();
        });
        assert_eq!(list_runs(&path).unwrap(), Vec::new());
    }
}
