//! Per-sprite editor-settings sidecar: things the user tunes in the panel
//! that the `.spr`/`.til`/`.pal` trio has no field for (tile-picker wrap
//! width, frame names). Written by the panel after each successful save,
//! read once at open; a missing or corrupt file is simply the defaults.
//! Lives at `<project>/.ggo-ide/<rel>.editor.json` -- the same hidden dir
//! ggo-ide's legacy `.meta.json` sidecars use, so editor droppings stay
//! out of the asset tree.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The sidecar's whole schema. Every field is optional-with-default so
/// files written by older builds (or hand-edited ones missing keys) load
/// as "keep the default" rather than an error.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EditorMeta {
    /// Tile-picker wrap width; `None` = the panel default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picker_cols: Option<usize>,
    /// Index-parallel to the sprite's frames; empty string = unnamed.
    /// May be shorter than the frame list (missing tail = unnamed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frame_names: Vec<String>,
}

/// `<rel>.editor.json` under the hidden `.ggo-ide/` dir, preserving the
/// rel's subdirectories -- the same layout as ggo-ide's legacy
/// `.meta.json` sidecars.
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

/// A frame's display label: its name, or `Frame N` (1-based, matching
/// how users count frames) when unnamed or past the list.
pub fn frame_label(names: &[String], ix: usize) -> String {
    match names.get(ix) {
        Some(name) if !name.is_empty() => name.clone(),
        _ => format!("Frame {}", ix + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_rel_path_nests_the_whole_rel_under_the_hidden_dir() {
        assert_eq!(
            meta_rel_path("sprites/hero.spr"),
            ".ggo-ide/sprites/hero.spr.editor.json"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let meta = EditorMeta {
            picker_cols: Some(6),
            frame_names: vec!["idle".to_string(), String::new(), "run".to_string()],
        };
        save(dir.path(), "sprites/hero.spr", &meta).unwrap();
        assert_eq!(load(dir.path(), "sprites/hero.spr"), meta);
    }

    #[test]
    fn load_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), "sprites/hero.spr"), EditorMeta::default());
    }

    #[test]
    fn load_corrupt_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(meta_rel_path("hero.spr"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load(dir.path(), "hero.spr"), EditorMeta::default());
    }

    #[test]
    fn frame_label_prefers_the_name_and_falls_back_to_the_index() {
        let names = vec!["idle".to_string(), String::new()];
        assert_eq!(frame_label(&names, 0), "idle");
        assert_eq!(frame_label(&names, 1), "Frame 2", "empty name falls back");
        assert_eq!(frame_label(&names, 5), "Frame 6", "past the list falls back");
    }
}
