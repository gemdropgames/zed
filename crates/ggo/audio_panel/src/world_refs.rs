//! Who else is listening: the worlds that name an `.adp`'s stem.
//!
//! Both destructive routes out of this crate -- overwriting a `.adp` on
//! import, and deleting one from the project panel -- change what a
//! world SOUNDS like without touching the world file, so neither is
//! visible from the file being acted on. These are the cascade lines
//! that say so before the write happens.
//!
//! **A minimal reimplementation on purpose.** `ggo_world_panel`'s
//! `audio_budget` walks the same components for the toolbar's region
//! readout, but that crate is not on this one's dependency edge (and
//! must not be: the world panel already reaches the audio tab, not the
//! other way round). What is shared instead is the *schema*: the audio
//! fields come from `ggo_worldlib::schemas::builtin_schemas`, exactly
//! the `FieldKind::Asset("adp" | "wav" | "ogg")` rule `audio_budget`
//! applies, so the two cannot drift about which component field holds a
//! stem. Deliberately NOT ported from there: resolved `[[instance]]`
//! subtrees, which only exist in a loaded world document -- a prompt
//! reads files.

use std::path::{Path, PathBuf};

use ggo_worldlib::schemas::{ComponentSchema, FieldKind};
use ggo_worldlib::world_files::WORLD_EXT;
use ggo_worldlib::{schemas, world_file};
use serde_json::Value;

/// The asset directory emerald bakes from, under an emerald project root.
const ASSETS_DIR: &str = "assets";

/// The extensions a schema field must name to count as audio -- the
/// baked form and the two sources emerald bakes at pack time.
const AUDIO_EXTS: [&str; 3] = ["adp", "wav", "ogg"];

/// The asset root `abs` lives under, plus `abs`'s `/`-separated path
/// inside it. `None` for a path outside an emerald project's `assets/`.
pub(crate) fn split_asset_path(abs: &Path) -> Option<(PathBuf, String)> {
    let assets = ggo_common::emerald_project_root(abs.parent()?)?.join(ASSETS_DIR);
    if !assets.is_dir() {
        return None;
    }
    let under = abs.strip_prefix(&assets).ok()?;
    Some((
        assets,
        under.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"),
    ))
}

/// The stem a world names for the `.adp` at asset-root-relative `rel` --
/// the path under the asset root with the extension dropped, which is
/// what emerald's `Sfx`/`Music` `stem` fields hold.
pub(crate) fn adp_stem(rel: &str) -> Option<&str> {
    let (stem, ext) = rel.rsplit_once('.')?;
    ext.eq_ignore_ascii_case("adp").then_some(stem)
}

/// One prompt line per world under `asset_root` whose entities name
/// `stem` in an audio field.
///
/// Reads every world file under the root, which is why this runs once,
/// on the way to a prompt, and never per frame. An unreadable world
/// contributes nothing: a delete must not be blocked by a file that was
/// already broken.
pub(crate) fn worlds_playing(asset_root: &Path, stem: &str) -> Vec<String> {
    let schemas = schemas::builtin_schemas();
    let fields = audio_fields(&schemas);
    let mut lines: Vec<String> = ggo_worldlib::sprites::io::list_all_files(asset_root)
        .into_iter()
        .filter(|rel| rel.ends_with(WORLD_EXT))
        .filter(|rel| {
            world_file::read_world(asset_root, rel)
                .is_ok_and(|world| plays(&world.entities, &fields, stem))
        })
        .map(|rel| {
            let world = rel.strip_suffix(WORLD_EXT).unwrap_or(&rel);
            format!("{world} plays this audio")
        })
        .collect();
    lines.sort();
    lines
}

/// The `(component, field)` pairs whose value is an audio stem.
fn audio_fields(schemas: &[ComponentSchema]) -> Vec<(&str, &str)> {
    schemas
        .iter()
        .flat_map(|schema| {
            schema.fields.iter().filter_map(move |field| match &field.kind {
                FieldKind::Asset(ext) if AUDIO_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) => {
                    Some((schema.name.as_str(), field.name.as_str()))
                }
                _ => None,
            })
        })
        .collect()
}

fn plays(entities: &[world_file::WorldEntity], fields: &[(&str, &str)], stem: &str) -> bool {
    entities.iter().any(|entity| {
        fields.iter().any(|(component, field)| {
            entity
                .components
                .get(*component)
                .and_then(|c| c.get(*field))
                .and_then(Value::as_str)
                == Some(stem)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join(ggo_common::EMERALD_MANIFEST), "").expect("the manifest");
        for (rel, body) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("the parent dir");
            }
            std::fs::write(path, body).expect("the fixture file");
        }
        dir
    }

    #[test]
    fn the_asset_root_is_the_projects_assets_dir_and_the_rel_is_under_it() {
        let dir = project(&[("assets/sfx/jump.adp", "")]);
        let (root, rel) =
            split_asset_path(&dir.path().join("assets/sfx/jump.adp")).expect("inside assets/");
        assert_eq!(root, dir.path().join("assets"));
        assert_eq!(rel, "sfx/jump.adp");
        assert!(
            split_asset_path(&dir.path().join("audio-src/jump.wav")).is_none(),
            "a source outside assets/ has no asset-root path"
        );
    }

    #[test]
    fn the_stem_is_the_asset_rel_without_the_baked_extension() {
        assert_eq!(adp_stem("sfx/jump.adp"), Some("sfx/jump"));
        assert_eq!(adp_stem("theme.ADP"), Some("theme"));
        assert_eq!(adp_stem("theme.wav"), None);
        assert_eq!(adp_stem("theme"), None);
    }

    #[test]
    fn a_world_is_named_when_an_audio_field_holds_the_stem() {
        let dir = project(&[
            ("assets/sfx/jump.adp", ""),
            (
                "assets/arena.wrld.toml",
                "[[entity]]\nSfx = { stem = \"sfx/jump\" }\n",
            ),
            (
                "assets/levels/deep.wrld.toml",
                "[[entity]]\nMusic = { stem = \"sfx/jump\" }\n",
            ),
            (
                "assets/quiet.wrld.toml",
                "[[entity]]\nSprite = { stem = \"sfx/jump\" }\n",
            ),
            ("assets/broken.wrld.toml", "not toml ["),
        ]);
        assert_eq!(
            worlds_playing(&dir.path().join("assets"), "sfx/jump"),
            vec![
                "arena plays this audio".to_string(),
                "levels/deep plays this audio".to_string(),
            ],
            "a sprite stem that happens to match is not audio, and a \
             broken world blocks nothing"
        );
        assert!(
            worlds_playing(&dir.path().join("assets"), "sfx/other").is_empty(),
            "a stem nothing names has no cascade"
        );
    }
}
