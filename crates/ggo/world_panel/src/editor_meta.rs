//! Per-world editor-settings sidecar: the things the user names in the
//! panel that the `.wrld.toml` has no field for. A `WorldEntity` is just
//! a component map -- the file carries no entity id -- so an entity's
//! identity here is its INDEX, and the names are parallel to
//! `WorldState::entities`. Written whenever a name changes, read once at
//! open; a missing or corrupt file is simply the defaults.
//! Lives at `<asset root>/.ggo-ide/<rel>.editor.json` -- the same hidden
//! dir the sprite panel's sidecars use, so editor droppings stay out of
//! the asset tree.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The sidecar's whole schema. Every field is optional-with-default so
/// files written by older builds (or hand-edited ones missing keys) load
/// as "keep the default" rather than an error.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EditorMeta {
    /// Index-parallel to the world's entities; empty string = unnamed.
    /// May be shorter than the entity list (missing tail = unnamed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entity_names: Vec<String>,
}

/// `<rel>.editor.json` under the hidden `.ggo-ide/` dir, preserving the
/// rel's subdirectories.
pub fn meta_rel_path(rel: &str) -> String {
    format!(".ggo-ide/{rel}.editor.json")
}

/// Read the sidecar for `rel`, or the defaults when it's missing or
/// unreadable -- editor settings are never worth failing an open over.
pub fn load(root: &Path, rel: &str) -> EditorMeta {
    let path = root.join(meta_rel_path(rel));
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => EditorMeta::default(),
    }
}

/// Write the sidecar for `rel`, creating `.ggo-ide/` subdirs as needed.
pub fn save(root: &Path, rel: &str, meta: &EditorMeta) -> Result<(), String> {
    let path = root.join(meta_rel_path(rel));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(meta).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_rel_path_nests_the_whole_rel_under_the_hidden_dir() {
        assert_eq!(
            meta_rel_path("worlds/main.wrld.toml"),
            ".ggo-ide/worlds/main.wrld.toml.editor.json"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let meta = EditorMeta {
            entity_names: vec!["boss".to_string(), String::new(), "door".to_string()],
        };
        save(dir.path(), "worlds/main.wrld.toml", &meta).unwrap();
        assert_eq!(load(dir.path(), "worlds/main.wrld.toml"), meta);
    }

    #[test]
    fn load_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(dir.path(), "worlds/main.wrld.toml"),
            EditorMeta::default()
        );
    }

    #[test]
    fn load_corrupt_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(meta_rel_path("main.wrld.toml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load(dir.path(), "main.wrld.toml"), EditorMeta::default());
    }
}
