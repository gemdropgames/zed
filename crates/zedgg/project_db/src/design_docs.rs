//! Design Docs: a tree of folders, markdown documents and opaque files
//! (images the documents reference), all rows of one `design_nodes` table.
//! Plain functions on a [`Connection`] so the panel crate holds no SQL and
//! this logic tests without gpui.

use anyhow::{Context as _, Result, bail};
use sqlez::connection::Connection;

/// The fixed root folder. Inserted by the migration; every other row has a
/// non-NULL `parent_id`, which is what makes `UNIQUE (parent_id, name)`
/// bite (SQLite treats NULLs as distinct in UNIQUE constraints).
pub const ROOT_ID: i64 = 1;

pub const MIGRATION: &str = "
CREATE TABLE design_nodes (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER REFERENCES design_nodes(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('folder', 'doc', 'file')),
    name TEXT NOT NULL,
    body TEXT,
    data BLOB,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK ((kind = 'doc') = (body IS NOT NULL)),
    CHECK ((kind = 'file') = (data IS NOT NULL)),
    UNIQUE (parent_id, name)
);
INSERT INTO design_nodes (id, parent_id, kind, name) VALUES (1, NULL, 'folder', 'Design Docs');
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Folder,
    Doc,
    File,
}

impl NodeKind {
    fn as_str(self) -> &'static str {
        match self {
            NodeKind::Folder => "folder",
            NodeKind::Doc => "doc",
            NodeKind::File => "file",
        }
    }

    fn parse(kind: &str) -> Result<Self> {
        Ok(match kind {
            "folder" => NodeKind::Folder,
            "doc" => NodeKind::Doc,
            "file" => NodeKind::File,
            other => bail!("unknown design_nodes.kind {other:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignNode {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub kind: NodeKind,
    pub name: String,
}

type NodeRow = (i64, Option<i64>, String, String);

fn node_from_row((id, parent_id, kind, name): NodeRow) -> Result<DesignNode> {
    Ok(DesignNode {
        id,
        parent_id,
        kind: NodeKind::parse(&kind)?,
        name,
    })
}

/// Every node, without bodies/blobs. Sorted so that siblings come out
/// folders-first, then case-insensitively by name -- the panel's display
/// order, done once here rather than re-sorted per render.
pub fn list_nodes(connection: &Connection) -> Result<Vec<DesignNode>> {
    connection.select::<NodeRow>(
        "SELECT id, parent_id, kind, name FROM design_nodes \
         ORDER BY parent_id, kind = 'folder' DESC, name COLLATE NOCASE",
    )?()?
    .into_iter()
    .map(node_from_row)
    .collect()
}

pub fn get_node(connection: &Connection, id: i64) -> Result<Option<DesignNode>> {
    connection.select_row_bound::<i64, NodeRow>(
        "SELECT id, parent_id, kind, name FROM design_nodes WHERE id = ?",
    )?(id)?
    .map(node_from_row)
    .transpose()
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("name must not be empty");
    }
    // Names double as path segments in markdown references
    // (`![](images/x.png)`), so they can't contain separators or be the
    // relative-path tokens `resolve_path` interprets.
    if name.contains(['/', '\\']) || name == "." || name == ".." {
        bail!("name {name:?} may not contain path separators or be `.`/`..`");
    }
    Ok(())
}

fn require_folder(connection: &Connection, id: i64) -> Result<()> {
    match get_node(connection, id)? {
        Some(node) if node.kind == NodeKind::Folder => Ok(()),
        Some(node) => bail!("{:?} is not a folder", node.name),
        None => bail!("no design node with id {id}"),
    }
}

fn insert(
    connection: &Connection,
    parent_id: i64,
    kind: NodeKind,
    name: &str,
    body: Option<&str>,
    data: Option<&[u8]>,
) -> Result<i64> {
    validate_name(name)?;
    require_folder(connection, parent_id)?;
    connection
        .select_row_bound::<(i64, &str, &str, Option<&str>, Option<&[u8]>), i64>(
            "INSERT INTO design_nodes (parent_id, kind, name, body, data) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )?((parent_id, kind.as_str(), name, body, data))?
        .context("INSERT ... RETURNING id produced no row")
}

pub fn create_folder(connection: &Connection, parent_id: i64, name: &str) -> Result<i64> {
    insert(connection, parent_id, NodeKind::Folder, name, None, None)
}

pub fn create_doc(connection: &Connection, parent_id: i64, name: &str) -> Result<i64> {
    insert(connection, parent_id, NodeKind::Doc, name, Some(""), None)
}

pub fn create_file(
    connection: &Connection,
    parent_id: i64,
    name: &str,
    data: &[u8],
) -> Result<i64> {
    insert(connection, parent_id, NodeKind::File, name, None, Some(data))
}

pub fn rename_node(connection: &Connection, id: i64, name: &str) -> Result<()> {
    if id == ROOT_ID {
        bail!("the root folder cannot be renamed");
    }
    validate_name(name)?;
    connection.exec_bound::<(&str, i64)>(
        "UPDATE design_nodes SET name = ?, updated_at = datetime('now') WHERE id = ?",
    )?((name, id))
}

/// Ids of every node below `id` (not including `id`), depth-first.
pub fn descendant_ids(connection: &Connection, id: i64) -> Result<Vec<i64>> {
    connection.select_bound::<i64, i64>(
        "WITH RECURSIVE below(id) AS (\
            SELECT id FROM design_nodes WHERE parent_id = ? \
            UNION ALL \
            SELECT n.id FROM design_nodes n JOIN below ON n.parent_id = below.id\
         ) SELECT id FROM below",
    )?(id)
}

pub fn move_node(connection: &Connection, id: i64, new_parent_id: i64) -> Result<()> {
    if id == ROOT_ID {
        bail!("the root folder cannot be moved");
    }
    require_folder(connection, new_parent_id)?;
    if new_parent_id == id || descendant_ids(connection, id)?.contains(&new_parent_id) {
        bail!("cannot move a folder into itself");
    }
    connection.exec_bound::<(i64, i64)>(
        "UPDATE design_nodes SET parent_id = ?, updated_at = datetime('now') WHERE id = ?",
    )?((new_parent_id, id))
}

/// Deletes `id` and, via `ON DELETE CASCADE`, everything below it.
pub fn delete_node(connection: &Connection, id: i64) -> Result<()> {
    if id == ROOT_ID {
        bail!("the root folder cannot be deleted");
    }
    connection.exec_bound::<i64>("DELETE FROM design_nodes WHERE id = ?")?(id)
}

pub fn load_body(connection: &Connection, id: i64) -> Result<String> {
    connection
        .select_row_bound::<i64, Option<String>>(
            "SELECT body FROM design_nodes WHERE id = ?",
        )?(id)?
        .flatten()
        .with_context(|| format!("no design doc with id {id}"))
}

/// Errors if `id` is not a document (deleted under an open tab, say) so
/// the editor's save fails visibly instead of silently updating nothing.
pub fn save_body(connection: &Connection, id: i64, body: &str) -> Result<()> {
    match get_node(connection, id)? {
        Some(node) if node.kind == NodeKind::Doc => {}
        Some(node) => bail!("{:?} is not a document", node.name),
        None => bail!("design doc {id} no longer exists"),
    }
    connection.exec_bound::<(&str, i64)>(
        "UPDATE design_nodes SET body = ?, updated_at = datetime('now') WHERE id = ?",
    )?((body, id))
}

pub fn load_file(connection: &Connection, id: i64) -> Result<Vec<u8>> {
    connection
        .select_row_bound::<i64, Option<Vec<u8>>>(
            "SELECT data FROM design_nodes WHERE id = ?",
        )?(id)?
        .flatten()
        .with_context(|| format!("no design file with id {id}"))
}

/// Resolve a markdown-style relative reference (`images/x.png`,
/// `../shared/y.png`, `/from/root.png`) starting at folder `folder_id`.
/// `Ok(None)` when any segment is missing.
pub fn resolve_path(
    connection: &Connection,
    folder_id: i64,
    reference: &str,
) -> Result<Option<DesignNode>> {
    let mut current = if reference.starts_with('/') {
        ROOT_ID
    } else {
        folder_id
    };
    let mut node = None;
    for segment in reference.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let next = if segment == ".." {
            match get_node(connection, current)?.and_then(|n| n.parent_id) {
                Some(parent) => get_node(connection, parent)?,
                None => return Ok(None),
            }
        } else {
            connection.select_row_bound::<(i64, &str), NodeRow>(
                "SELECT id, parent_id, kind, name FROM design_nodes \
                 WHERE parent_id = ? AND name = ?",
            )?((current, segment))?
            .map(node_from_row)
            .transpose()?
        };
        let Some(next) = next else {
            return Ok(None);
        };
        current = next.id;
        node = Some(next);
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    fn names(connection: &Connection, parent: i64) -> Vec<String> {
        list_nodes(connection)
            .unwrap()
            .into_iter()
            .filter(|n| n.parent_id == Some(parent))
            .map(|n| n.name)
            .collect()
    }

    #[test]
    fn root_exists_and_is_protected() {
        let c = open_memory("design_docs_root");
        let root = get_node(&c, ROOT_ID).unwrap().unwrap();
        assert_eq!(root.kind, NodeKind::Folder);
        assert_eq!(root.parent_id, None);
        assert!(rename_node(&c, ROOT_ID, "x").is_err());
        assert!(delete_node(&c, ROOT_ID).is_err());
        assert!(move_node(&c, ROOT_ID, ROOT_ID).is_err());
    }

    #[test]
    fn create_orders_folders_first_then_name_nocase() {
        let c = open_memory("design_docs_order");
        create_doc(&c, ROOT_ID, "zeta.md").unwrap();
        create_folder(&c, ROOT_ID, "b-folder").unwrap();
        create_doc(&c, ROOT_ID, "Alpha.md").unwrap();
        create_folder(&c, ROOT_ID, "A-folder").unwrap();
        assert_eq!(
            names(&c, ROOT_ID),
            ["A-folder", "b-folder", "Alpha.md", "zeta.md"]
        );
    }

    #[test]
    fn names_unique_per_parent_and_validated() {
        let c = open_memory("design_docs_unique");
        let f = create_folder(&c, ROOT_ID, "f").unwrap();
        create_doc(&c, ROOT_ID, "a").unwrap();
        assert!(create_doc(&c, ROOT_ID, "a").is_err(), "duplicate sibling");
        create_doc(&c, f, "a").unwrap(); // same name, other folder: fine
        assert!(create_doc(&c, ROOT_ID, "").is_err());
        assert!(create_doc(&c, ROOT_ID, "a/b").is_err());
        assert!(create_doc(&c, ROOT_ID, "..").is_err());
        let doc = create_doc(&c, ROOT_ID, "d").unwrap();
        assert!(create_doc(&c, doc, "child").is_err(), "docs are not folders");
    }

    #[test]
    fn move_rejects_cycles_and_delete_cascades() {
        let c = open_memory("design_docs_move");
        let a = create_folder(&c, ROOT_ID, "a").unwrap();
        let b = create_folder(&c, a, "b").unwrap();
        let doc = create_doc(&c, b, "doc").unwrap();
        assert!(move_node(&c, a, b).is_err(), "into own child");
        assert!(move_node(&c, a, a).is_err(), "into itself");
        assert!(move_node(&c, b, doc).is_err(), "onto a doc");
        move_node(&c, b, ROOT_ID).unwrap();
        assert_eq!(names(&c, ROOT_ID), ["a", "b"]);
        assert_eq!(descendant_ids(&c, b).unwrap(), [doc]);

        delete_node(&c, b).unwrap();
        assert!(get_node(&c, doc).unwrap().is_none(), "cascade");
        assert!(load_body(&c, doc).is_err());
        assert!(save_body(&c, doc, "late").is_err(), "save to a deleted doc must fail");
    }

    #[test]
    fn body_and_file_round_trip() {
        let c = open_memory("design_docs_body");
        let doc = create_doc(&c, ROOT_ID, "d").unwrap();
        assert_eq!(load_body(&c, doc).unwrap(), "");
        save_body(&c, doc, "# hi\n").unwrap();
        assert_eq!(load_body(&c, doc).unwrap(), "# hi\n");

        let png = [0x89u8, b'P', b'N', b'G', 0, 1, 2];
        let file = create_file(&c, ROOT_ID, "x.png", &png).unwrap();
        assert_eq!(load_file(&c, file).unwrap(), png);
        assert!(load_body(&c, file).is_err(), "files have no body");
        assert!(load_file(&c, doc).is_err(), "docs have no data");
        rename_node(&c, file, "y.png").unwrap();
        assert_eq!(get_node(&c, file).unwrap().unwrap().name, "y.png");
    }

    #[test]
    fn resolve_path_walks_segments() {
        let c = open_memory("design_docs_resolve");
        let specs = create_folder(&c, ROOT_ID, "specs").unwrap();
        let images = create_folder(&c, specs, "images").unwrap();
        let shared = create_folder(&c, ROOT_ID, "shared").unwrap();
        let hero = create_file(&c, images, "hero.png", b"1").unwrap();
        let logo = create_file(&c, shared, "logo.png", b"2").unwrap();

        let id = |r: Option<DesignNode>| r.map(|n| n.id);
        assert_eq!(id(resolve_path(&c, specs, "images/hero.png").unwrap()), Some(hero));
        assert_eq!(id(resolve_path(&c, specs, "./images/hero.png").unwrap()), Some(hero));
        assert_eq!(id(resolve_path(&c, images, "../../shared/logo.png").unwrap()), Some(logo));
        assert_eq!(id(resolve_path(&c, images, "/shared/logo.png").unwrap()), Some(logo));
        assert_eq!(id(resolve_path(&c, images, "hero.png").unwrap()), Some(hero));
        assert_eq!(id(resolve_path(&c, specs, "images/nope.png").unwrap()), None);
        assert_eq!(id(resolve_path(&c, ROOT_ID, "../x").unwrap()), None, "above root");
        assert_eq!(id(resolve_path(&c, specs, "images").unwrap()), Some(images));
    }
}
