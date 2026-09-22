//! Per-source editor-settings sidecar: what the user chose in this tab
//! that the audio file itself has no field for. Written after each
//! successful import, read when a source opens; a missing or corrupt
//! file is simply the defaults.
//!
//! Lives at `<project>/.ggo-ide/<rel>.editor.json`, the same hidden dir
//! and the same `<rel>.editor.json` layout the sprite and world panels'
//! sidecars use, so editor droppings stay out of the asset tree.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The sidecar's whole schema. Every field is optional-with-default so
/// files written by older builds (or hand-edited ones missing keys) load
/// as "keep the default" rather than an error.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct EditorMeta {
    /// Where the last successful Import of this source went,
    /// worktree-relative; `None` = the panel's computed default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) import_target: Option<String>,
}

/// `<rel>.editor.json` under the hidden `.ggo-ide/` dir, preserving the
/// rel's subdirectories.
pub(crate) fn meta_rel_path(rel: &str) -> String {
    format!(".ggo-ide/{rel}.editor.json")
}

/// Read the sidecar for `rel`, or the defaults when it is missing or
/// unreadable -- editor settings are never worth failing an open over.
pub(crate) fn load(root: &Path, rel: &str) -> EditorMeta {
    match std::fs::read(root.join(meta_rel_path(rel))) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => EditorMeta::default(),
    }
}

/// Write the sidecar for `rel`, creating `.ggo-ide/` subdirs as needed.
pub(crate) fn save(root: &Path, rel: &str, meta: &EditorMeta) -> Result<(), String> {
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
    fn the_path_nests_the_whole_rel_under_the_hidden_dir() {
        assert_eq!(
            meta_rel_path("audio-src/jump.wav"),
            ".ggo-ide/audio-src/jump.wav.editor.json"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let meta = EditorMeta {
            import_target: Some("assets/sfx/jump.adp".to_string()),
        };
        save(dir.path(), "audio-src/jump.wav", &meta).expect("the sidecar writes");
        assert_eq!(load(dir.path(), "audio-src/jump.wav"), meta);
    }

    #[test]
    fn a_missing_or_corrupt_file_is_the_defaults() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(
            load(dir.path(), "audio-src/jump.wav"),
            EditorMeta::default()
        );
        let path = dir.path().join(meta_rel_path("audio-src/jump.wav"));
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the hidden dir");
        std::fs::write(&path, b"not json").expect("the corrupt file");
        assert_eq!(
            load(dir.path(), "audio-src/jump.wav"),
            EditorMeta::default(),
            "a corrupt sidecar is not worth failing an open over"
        );
    }
}
