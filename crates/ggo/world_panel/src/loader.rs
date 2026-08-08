//! Off-thread world loading: enumerate `worlds/**.toml`, read + resolve a
//! world, and compose every referenced asset into `render::AssetLoads`.
//!
//! The compose functions (`compose_sprite_rgba`/`compose_meta_sprite_rgba`/
//! `compose_map_rgba`) and `resolve_world_value` are ports of ggo-ide's
//! `pages/world/mod.rs` driver (same stems, same per-target
//! `Loadable::Error` fallback -- a failed stem renders as a placeholder,
//! never fails the whole load). The orchestration differs deliberately:
//! ggo-ide dispatches one iced task per target and re-runs
//! `dispatch_new_asset_loads` after every message because its doc is
//! live-editable; this viewer loads a world exactly once per selection, so
//! [`load_world`] runs the whole pipeline in one background pass (the
//! panel guards staleness with a load-generation counter instead of
//! ggo-ide's project epoch).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ggo_worldlib::backgrounds::{MergedBackground, merge_backgrounds};
use ggo_worldlib::render::{self, AssetLoads, Loadable, collect_load_targets};
use ggo_worldlib::sprites::{io, map_doc, palette565, preview};
use ggo_worldlib::world_doc::{Background, WorldDocStore, WorldDocWire};
use ggo_worldlib::world_file::read_world;
use ggo_worldlib::world_files::{self, WORLD_EXT, WorldListing};

/// Everything the panel needs to enter its Ready state, assembled entirely
/// off the UI thread.
pub struct LoadedWorld {
    pub store: WorldDocStore,
    pub sprite_loads: AssetLoads,
    pub map_loads: AssetLoads,
    pub meta_sprite_loads: AssetLoads,
    pub merged: Vec<MergedBackground>,
}

// ------------------------------------------------------------ worlds list

/// Recursively collect the project-relative paths (forward-slash
/// separated) of every file under `<root>/worlds`, sorted, then filter
/// through `world_files::world_files` -- the panel's picker feed.
pub fn list_worlds(root: &Path) -> Vec<WorldListing> {
    let mut rels = Vec::new();
    walk_files(&root.join("worlds"), &PathBuf::from("worlds"), &mut rels);
    rels.sort();
    world_files::world_files(&rels)
}

fn walk_files(dir: &Path, rel: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let child_rel = rel.join(&name);
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, &child_rel, out);
        } else {
            // Rel paths are built with `/` regardless of platform -- that's
            // the shape `world_files` (and every worldlib rel-path API)
            // expects.
            let mut s = String::new();
            for comp in child_rel.components() {
                if !s.is_empty() {
                    s.push('/');
                }
                s.push_str(&comp.as_os_str().to_string_lossy());
            }
            out.push(s);
        }
    }
}

// ---------------------------------------------------------- one-shot load

/// Read `rel`, resolve every `[[instance]]` subtree, merge backgrounds,
/// and compose every load target `collect_load_targets` (plus the merged
/// background set) names. Per-stem failures become `Loadable::Error`
/// entries (placeholder rendering); only a failure to read the world file
/// itself fails the load.
pub fn load_world(project_dir: &Path, rel: &str) -> Result<LoadedWorld, String> {
    let file = read_world(project_dir, rel).map_err(|e| e.to_string())?;
    let mut store = WorldDocStore::new(WorldDocWire::from(file));

    // Instance-subtree resolution -- ggo-ide's `resolve_instance_task`
    // stamped via `set_instances_resolved` (overwrite: this is the fresh
    // load; there is no cache to fill from).
    let instance_worlds = dedup_instance_worlds(&store);
    for world in &instance_worlds {
        let result = resolve_world_value(project_dir, world, &mut Vec::new());
        store.set_instances_resolved(world, &result, true);
    }

    let state = store.state();

    // Instance `[[background]]` sets -- ggo-ide's
    // `instance_backgrounds_task`; a stem whose world fails to read
    // contributes nothing to the merge (same `filter_map(ok)` semantics as
    // ggo-ide's `merged_backgrounds`).
    let mut loaded_bgs: HashMap<String, Vec<Background>> = HashMap::new();
    for world in &instance_worlds {
        let bg_rel = format!("{world}{WORLD_EXT}");
        if let Ok(w) = read_world(project_dir, &bg_rel) {
            loaded_bgs.insert(world.clone(), w.backgrounds);
        }
    }
    let instances: Vec<(String, bool)> = state
        .instances
        .iter()
        .map(|i| (i.world.clone(), i.background_priority))
        .collect();
    let merged = merge_backgrounds(&state.backgrounds, &instances, &loaded_bgs);

    // Asset composition -- ggo-ide's `dispatch_new_asset_loads` target set:
    // `collect_load_targets` for entities/subtrees, plus the merged
    // background stems (the merged set is the only source of background map
    // loads; `collect_load_targets` never reports background stems).
    let (sprite_stems, map_stems, meta_targets) = collect_load_targets(&state);
    let mut sprite_loads = AssetLoads::new();
    for stem in sprite_stems {
        let load = settle(compose_sprite_rgba(project_dir, &stem));
        sprite_loads.insert(stem, load);
    }
    let mut map_loads = AssetLoads::new();
    for stem in map_stems {
        let load = settle(compose_map_rgba(project_dir, &stem));
        map_loads.insert(stem, load);
    }
    for bg in &merged {
        if !map_loads.contains_key(&bg.stem) {
            let load = settle(compose_map_rgba(project_dir, &bg.stem));
            map_loads.insert(bg.stem.clone(), load);
        }
    }
    let mut meta_sprite_loads = AssetLoads::new();
    for (stem, clip) in meta_targets {
        let key = render::meta_sprite_load_key(&stem, &clip);
        let load = settle(compose_meta_sprite_rgba(project_dir, &stem, &clip));
        meta_sprite_loads.insert(key, load);
    }

    Ok(LoadedWorld {
        store,
        sprite_loads,
        map_loads,
        meta_sprite_loads,
        merged,
    })
}

fn settle(result: Result<render::RgbaImage, String>) -> Loadable<render::RgbaImage> {
    result.map_or(Loadable::Error, Loadable::Ready)
}

fn dedup_instance_worlds(store: &WorldDocStore) -> Vec<String> {
    let mut worlds: Vec<String> = Vec::new();
    for inst in &store.state().instances {
        if !worlds.contains(&inst.world) {
            worlds.push(inst.world.clone());
        }
    }
    worlds
}

// ------------------------------------------------- ported compose drivers

/// A `Sprite` component's plain frame-0 composed image -- ggo-ide's
/// `compose_sprite_rgba`, verbatim (no LCD filter: world-canvas preview).
fn compose_sprite_rgba(project_dir: &Path, stem: &str) -> Result<render::RgbaImage, String> {
    let rel = format!("{stem}.spr");
    let opened = io::open_sprite(project_dir, &rel).map_err(|e| e.to_string())?;
    let rgba = preview::compose_frame_rgba(&opened.state, 0, false);
    let (w, h) = rgba.dimensions();
    Ok(render::RgbaImage {
        rgba: rgba.into_raw().into(),
        w,
        h,
    })
}

/// A `MetaSprite` component's clip-resolved composed image -- ggo-ide's
/// `compose_meta_sprite_rgba`, verbatim (frame from `resolve_clip_frame`,
/// not always 0).
fn compose_meta_sprite_rgba(
    project_dir: &Path,
    stem: &str,
    clip: &str,
) -> Result<render::RgbaImage, String> {
    let rel = format!("{stem}.spr");
    let opened = io::open_sprite(project_dir, &rel).map_err(|e| e.to_string())?;
    let frame_idx = preview::resolve_clip_frame(&opened.state.clips, clip);
    let rgba = preview::compose_frame_rgba(&opened.state, frame_idx, false);
    let (w, h) = rgba.dimensions();
    Ok(render::RgbaImage {
        rgba: rgba.into_raw().into(),
        w,
        h,
    })
}

/// A map entry's composed image (Tilemap entities and `[[background]]`s
/// alike) -- ggo-ide's `compose_map_rgba`, verbatim: `open_map` for cells +
/// tileset binding, `open_tileset` for palette-resolved pixel data,
/// `compose_map_indices` -> RGBA with `TRANSPARENT_SLOT` drawn transparent.
fn compose_map_rgba(project_dir: &Path, stem: &str) -> Result<render::RgbaImage, String> {
    let rel = format!("{stem}.map");
    let map = io::open_map(project_dir, &rel).map_err(|e| e.to_string())?;
    let til = io::open_tileset(project_dir, &map.til_path).map_err(|e| e.to_string())?;
    let (indices, px_w, px_h) =
        map_doc::compose_map_indices(&map.cells, map.w, map.h, &til.indices, til.tile_count);

    let mut rgba = Vec::with_capacity(indices.len() * 4);
    for idx in indices {
        let (r, g, b) = ggo_asset_formats::pixel::rgb888(til.palette[idx as usize]);
        let alpha = if idx as usize == palette565::TRANSPARENT_SLOT {
            0
        } else {
            255
        };
        rgba.extend_from_slice(&[r, g, b, alpha]);
    }
    Ok(render::RgbaImage {
        rgba: rgba.into(),
        w: px_w as u32,
        h: px_h as u32,
    })
}

/// Read `stem`'s world file and shape it as render's resolved-subtree JSON
/// (`render::as_resolved_node`'s expected shape), resolving nested
/// `[[instance]]`s recursively -- ggo-ide's `resolve_world_value`,
/// verbatim. `visited` is the stem chain above this call: a repeat becomes
/// that instance's `error` instead of recursing forever.
fn resolve_world_value(
    dir: &Path,
    stem: &str,
    visited: &mut Vec<String>,
) -> Result<serde_json::Value, String> {
    if visited.iter().any(|s| s == stem) {
        return Err(format!("instance cycle: {stem}"));
    }
    visited.push(stem.to_string());
    let rel = format!("{stem}{WORLD_EXT}");
    let result = read_world(dir, &rel).map_err(|e| e.to_string()).map(|w| {
        let entities: Vec<serde_json::Value> = w
            .entities
            .into_iter()
            .map(|e| serde_json::json!({ "components": e.components }))
            .collect();
        let instances: Vec<serde_json::Value> = w
            .instances
            .into_iter()
            .map(|inst| {
                let mut node = serde_json::json!({
                    "world": inst.world,
                    "pos": inst.pos,
                });
                match resolve_world_value(dir, &inst.world, visited) {
                    Ok(resolved) => node["resolved"] = resolved,
                    Err(e) => node["error"] = serde_json::Value::String(e),
                }
                node
            })
            .collect();
        serde_json::json!({ "entities": entities, "instances": instances })
    });
    visited.pop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picker filter: only `worlds/**.toml` files survive, nested dirs
    /// included, and listings come back path-sorted.
    #[test]
    fn list_worlds_filters_to_world_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("worlds/nested")).unwrap();
        std::fs::create_dir_all(root.join("sprites")).unwrap();
        std::fs::write(root.join("worlds/b.toml"), "").unwrap();
        std::fs::write(root.join("worlds/a.toml"), "").unwrap();
        std::fs::write(root.join("worlds/nested/c.toml"), "").unwrap();
        std::fs::write(root.join("worlds/notes.txt"), "").unwrap();
        std::fs::write(root.join("sprites/d.toml"), "").unwrap();

        let listings = list_worlds(root);
        let stems: Vec<&str> = listings.iter().map(|l| l.stem.as_str()).collect();
        assert_eq!(stems, ["worlds/a", "worlds/b", "worlds/nested/c"]);
        assert_eq!(listings[0].rel_path, "worlds/a.toml");
    }

    #[test]
    fn list_worlds_of_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_worlds(dir.path()).is_empty());
    }
}
