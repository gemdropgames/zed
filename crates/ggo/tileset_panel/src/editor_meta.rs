//! Per-tileset EDITOR-layout sidecar: the tooling column's dragged width
//! and its two section eyes. Deliberately separate from worldlib's
//! `tileset_meta` (which carries `zoom`/`cols`/terrains and is shared with
//! the map editor and ggo-ide): layout is this panel's own concern, and
//! worldlib lives in another repo. Same shape and same hidden directory as
//! `ggo_sprite_panel::editor_meta` -- `<project>/.ggo-ide/<rel>.editor.json`
//! -- so editor droppings stay out of the asset tree.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The sidecar's whole schema. Every field is optional-with-default so
/// files written by older builds (or hand-edited ones missing keys) load
/// as "keep the default" rather than an error.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EditorMeta {
    /// The tooling column's dragged width in px; `None` = the panel
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_width: Option<f32>,
    /// The tooling column's two section eyes; `None` = shown
    /// ([`visible`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette_visible: Option<bool>,
}

/// Whether a stored section eye means "shown". An absent flag is shown:
/// a sidecar written before these keys existed must not hide anything.
pub fn visible(flag: Option<bool>) -> bool {
    flag.unwrap_or(true)
}

/// `<rel>.editor.json` under the hidden `.ggo-ide/` dir, preserving the
/// rel's subdirectories.
pub fn meta_rel_path(rel: &str) -> String {
    format!(".ggo-ide/{rel}.editor.json")
}

/// Read the sidecar for `rel`, or the defaults when it's missing or
/// unreadable -- a layout preference is never worth failing an open over.
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
            meta_rel_path("tiles/world.til"),
            ".ggo-ide/tiles/world.til.editor.json"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let meta = EditorMeta {
            tools_width: Some(412.5),
            info_visible: Some(false),
            palette_visible: Some(true),
        };
        save(dir.path(), "tiles/world.til", &meta).unwrap();
        assert_eq!(load(dir.path(), "tiles/world.til"), meta);
    }

    #[test]
    fn load_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), "tiles/world.til"), EditorMeta::default());
    }

    #[test]
    fn load_corrupt_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(meta_rel_path("world.til"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load(dir.path(), "world.til"), EditorMeta::default());
    }

    /// An unset eye is "show it" -- a sidecar written before these keys
    /// existed, or none at all, must not hide a section.
    #[test]
    fn an_unset_visibility_flag_is_visible() {
        let meta = EditorMeta::default();
        assert!(visible(meta.info_visible));
        assert!(visible(meta.palette_visible));
        assert!(!visible(Some(false)));
        assert!(visible(Some(true)));
    }
}
