//! Off-thread world loading: enumerate `worlds/**.toml`, read + resolve a
//! world, and compose every referenced asset into `render::AssetLoads`.
//!
//! The compose functions (`compose_sprite_rgba`/`compose_meta_sprite_rgba`
//! here, `ggo_worldlib::sprites::io::compose_map_rgba` for maps) and
//! `resolve_world_value` are ports of ggo-ide's `pages/world/mod.rs`
//! driver (same stems, same per-target `Loadable::Error` fallback -- a
//! failed stem renders as a placeholder, never fails the whole load). The
//! orchestration differs deliberately: ggo-ide dispatches one iced task
//! per target and re-runs `dispatch_new_asset_loads` after every message
//! because its doc is live-editable; this viewer loads a world exactly
//! once per selection, so [`load_world`] runs the whole pipeline in one
//! background pass (the panel guards staleness with a load-generation
//! counter instead of ggo-ide's project epoch). `compose_map_rgba` itself
//! lives in `ggo-worldlib` (F5.1 Task M1), not ported here a second time
//! -- `ggo_map_panel` (M2) shares the exact same fn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ggo_worldlib::backgrounds::{MergedBackground, merge_backgrounds};
use ggo_worldlib::render::{self, AssetLoads, Loadable, collect_load_targets};
use ggo_worldlib::schemas::{ComponentSchema, ManifestComponent, all_schemas, manifest_components};
use ggo_worldlib::sprites::{io, preview};
use ggo_worldlib::world_doc::{Background, WorldDocStore, WorldDocWire, WorldState};
use ggo_worldlib::world_file::{WorldInstance, read_world};
use ggo_worldlib::world_files::{self, WORLD_EXT, WorldListing};

/// Everything the panel needs to enter its Ready state, assembled entirely
/// off the UI thread.
pub struct LoadedWorld {
    pub store: WorldDocStore,
    pub sprite_loads: AssetLoads,
    pub map_loads: AssetLoads,
    pub meta_sprite_loads: AssetLoads,
    pub merged: Vec<MergedBackground>,
    /// Each `[[instance]]`'d world's own `[[background]]` set, by stem --
    /// the third input to [`merge_backgrounds`]. Retained (rather than
    /// dropped once `merged` is computed) because editing the BASE
    /// world's slots has to re-run the merge against exactly the same
    /// instance sets, and re-reading every instance world file on the UI
    /// thread for that would be a second, drifting copy of the load-time
    /// read below.
    pub instance_backgrounds: HashMap<String, Vec<Background>>,
    /// Inspector schema set: builtins + this project's manifest
    /// components (see [`manifest_schemas`]).
    pub schemas: Vec<ComponentSchema>,
}

// ------------------------------------------------------------ worlds list

/// Recursively collect the project-relative paths (forward-slash
/// separated) of every file under `<root>/worlds`, sorted, then filter
/// through `world_files::world_files`. Feeds the panel's `AddInstance`
/// candidate set (F4 X1 removed the picker this also used to feed).
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
    let merged = merged_backgrounds(&state, &loaded_bgs);

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
        let load = settle(io::compose_map_rgba(project_dir, &stem).map_err(|e| e.to_string()));
        map_loads.insert(stem, load);
    }
    fill_missing_background_loads(project_dir, &merged, &mut map_loads);
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
        instance_backgrounds: loaded_bgs,
        schemas: schemas_near(project_dir),
    })
}

/// The merged `[[background]]` slot set for `state`, given every
/// instance world's own slot list. Shared by [`load_world`] and by the
/// panel's post-edit re-merge so that "which map fills which layer" has
/// exactly one caller shape -- an add/clear/undo of a base-world slot has
/// to answer it identically to the load that opened the world.
pub fn merged_backgrounds(
    state: &WorldState,
    instance_backgrounds: &HashMap<String, Vec<Background>>,
) -> Vec<MergedBackground> {
    let instances: Vec<(String, bool)> = state
        .instances
        .iter()
        .map(|i| (i.world.clone(), i.background_priority))
        .collect();
    merge_backgrounds(&state.backgrounds, &instances, instance_backgrounds)
}

/// Compose every merged background stem missing from `map_loads`, in
/// place -- the background half of [`fill_missing_asset_loads`], and the
/// only source of background map loads (`collect_load_targets` never
/// reports background stems). Already-present stems are never recomposed:
/// their pointer identity keys the panel's `RenderImage` cache.
pub fn fill_missing_background_loads(
    project_dir: &Path,
    merged: &[MergedBackground],
    map_loads: &mut AssetLoads,
) {
    for bg in merged {
        map_loads.entry(bg.stem.clone()).or_insert_with(|| {
            settle(io::compose_map_rgba(project_dir, &bg.stem).map_err(|e| e.to_string()))
        });
    }
}

// ------------------------------------------------------- live-mode payloads

const TILESET_EXT: &str = ".til";

/// One background slot's cells, ready for the cart's `CMD_LOAD_LAYER`
/// (`live::layer_bytes`) plus the tileset the slot's map is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerPayload {
    pub slot: u8,
    /// Asset-root-relative, extension-less -- the `.til` path the map
    /// carries minus its suffix, i.e. the same shape every other stem in
    /// this module has.
    pub tileset_stem: String,
    pub w: u16,
    pub h: u16,
    pub cells: Vec<u16>,
}

/// One payload per merged background slot whose `.map` opens. A slot
/// whose map is missing (or fails to decode) is SKIPPED rather than sent
/// as an empty layer: the panel already surfaces that slot's failure on
/// the layers rail, and pushing a zero-sized layer to the cart would
/// blank a slot the user can still see composed on the canvas.
pub fn layer_payloads(root: &Path, merged: &[MergedBackground]) -> Vec<LayerPayload> {
    merged
        .iter()
        .filter_map(|background| {
            let rel = format!("{}.map", background.stem);
            let map = io::open_map(root, &rel).ok()?;
            let tileset_stem = map
                .til_path
                .strip_suffix(TILESET_EXT)
                .unwrap_or(&map.til_path)
                .to_string();
            Some(LayerPayload {
                slot: background.layer,
                tileset_stem,
                w: map.w,
                h: map.h,
                cells: map.cells,
            })
        })
        .collect()
}

/// How many entities each top-level instance contributes once flattened,
/// in `[[instance]]` order -- the counts `live::IndexMap::new` needs to
/// map a cart index back to a document selection. Recursion follows
/// emerald's `collect_world` encoder order (each nested `[[instance]]`
/// depth-first, in file order); a world that fails to read counts 0, the
/// same "contributes nothing" rule the background merge uses.
pub fn instance_entity_counts(root: &Path, instances: &[WorldInstance]) -> Vec<usize> {
    instances
        .iter()
        .map(|instance| subtree_entity_count(root, &instance.world, &mut Vec::new()))
        .collect()
}

/// `seen` is the stem chain above this call; a repeat contributes 0
/// instead of recursing forever, matching `resolve_world_value`'s
/// `visited` guard (a cycle there becomes that instance's `error`, which
/// the encoder likewise flattens to nothing).
fn subtree_entity_count(root: &Path, stem: &str, seen: &mut Vec<String>) -> usize {
    if seen.iter().any(|s| s == stem) {
        return 0;
    }
    seen.push(stem.to_string());
    let count = match read_world(root, &format!("{stem}{WORLD_EXT}")) {
        Ok(world) => {
            world.entities.len()
                + world
                    .instances
                    .iter()
                    .map(|nested| subtree_entity_count(root, &nested.world, seen))
                    .sum::<usize>()
        }
        Err(_) => 0,
    };
    seen.pop();
    count
}

// --------------------------------------------------- incremental (add time)

/// Resolve ONE instance stem's subtree, exactly as [`load_world`] does for
/// every stem at load -- the add-instance path's incremental resolver.
/// ggo-ide re-resolves after every message (`dispatch_new_asset_loads`);
/// without this a freshly added instance renders only its origin marker
/// until the world is re-selected (M7 review, fix round 1).
pub fn resolve_instance(project_dir: &Path, stem: &str) -> Result<serde_json::Value, String> {
    resolve_world_value(project_dir, stem, &mut Vec::new())
}

/// Compose any `collect_load_targets` stems missing from the given load
/// maps, in place -- the incremental version of [`load_world`]'s asset
/// pass, for targets a freshly resolved instance subtree introduced.
/// Already-present stems are never recomposed (same image, and their
/// pointer identity keys the panel's `RenderImage` cache).
pub fn fill_missing_asset_loads(
    project_dir: &Path,
    state: &WorldState,
    sprite_loads: &mut AssetLoads,
    map_loads: &mut AssetLoads,
    meta_sprite_loads: &mut AssetLoads,
) {
    let (sprite_stems, map_stems, meta_targets) = collect_load_targets(state);
    for stem in sprite_stems {
        sprite_loads
            .entry(stem.clone())
            .or_insert_with(|| settle(compose_sprite_rgba(project_dir, &stem)));
    }
    for stem in map_stems {
        map_loads.entry(stem.clone()).or_insert_with(|| {
            settle(io::compose_map_rgba(project_dir, &stem).map_err(|e| e.to_string()))
        });
    }
    for (stem, clip) in meta_targets {
        let key = render::meta_sprite_load_key(&stem, &clip);
        meta_sprite_loads
            .entry(key)
            .or_insert_with(|| settle(compose_meta_sprite_rgba(project_dir, &stem, &clip)));
    }
}

// -------------------------------------------------------- manifest schemas

/// emerald's `MANIFEST_VERSION` -- a `components.toml` whose `version`
/// exceeds this was written by a newer `emd` than this build knows about
/// (same gate as ggo-ide's `emerald::read_one_manifest`).
const MANIFEST_SCHEMA_VERSION: u64 = 1;

/// The inspector's schema set: builtins plus `manifests/components.toml`'s
/// `[[component]]` entries. Ports ggo-ide's `manifests_task` ->
/// `ManifestsResult` feed (`manifest_components` on `components.toml` only
/// -- the other manifest files carry no `component` array); any failure
/// (unmanaged project, parse error, missing/newer `version`) falls back to
/// builtins only, exactly like ggo-ide's `Err` arm. Adaptation noted: this
/// reads just `components.toml` instead of enumerating every
/// `manifests/*.toml`, since the schema feed never consumed the others.
///
/// **PRIVATE on purpose, and it is the footgun half of this pair**: it
/// takes the emerald PROJECT root, and handing it an asset root instead
/// silently yields builtins only (the bug S3 fixed -- see
/// [`schemas_near`], which is what every caller outside this module
/// should use).
fn manifest_schemas(project_dir: &Path) -> Vec<ComponentSchema> {
    all_schemas(&read_components_manifest(project_dir).unwrap_or_default())
}

/// [`manifest_schemas`] for a directory that may be an ASSET ROOT rather
/// than the project root -- it walks up to the nearest ancestor holding
/// `emerald.toml` first (`ggo_common::emerald_project_root`, emerald's own
/// `Project::discover` rule), and falls back to `dir` itself when there is
/// no such ancestor.
///
/// This exists because the panel loads a world against its DERIVED asset
/// root -- `<worktree>/assets` for `assets/worlds/main.toml`, see
/// `split_world_path` -- and `manifests/` is NOT under that root, it is
/// its sibling. Calling `manifest_schemas` with the asset root looked for
/// `<worktree>/assets/manifests/components.toml`, which no emerald project
/// has, so an asset-rooted world silently got builtins only. Found while
/// wiring F5.2/S3's "a component created in the emerald panel becomes
/// available here without a restart"; the fallback is what keeps the
/// worktree-rooted `worlds/main.toml` layout (and this module's own tests)
/// working unchanged.
pub fn schemas_near(dir: &Path) -> Vec<ComponentSchema> {
    match ggo_common::emerald_project_root(dir) {
        Some(root) => manifest_schemas(&root),
        None => manifest_schemas(dir),
    }
}

fn read_components_manifest(project_dir: &Path) -> Option<Vec<ManifestComponent>> {
    let path = project_dir.join("manifests").join("components.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = toml::from_str(&content).ok()?;
    let version = json.get("version").and_then(serde_json::Value::as_u64)?;
    if version > MANIFEST_SCHEMA_VERSION {
        return None;
    }
    Some(manifest_components(&json))
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
    use ggo_worldlib::sprites::map_doc::MapState;

    /// A `w`x`h` `.map` at `rel`, bound to `til_rel`, carrying `cells`.
    /// Returns the map's asset-root-relative STEM (what a
    /// `[[background]]` slot names).
    fn write_small_map(
        root: &Path,
        rel: &str,
        til_rel: &str,
        w: u16,
        h: u16,
        cells: &[u16],
    ) -> String {
        if let Some(parent) = root.join(rel).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let state = MapState {
            w,
            h,
            cells: cells.to_vec(),
            til_path: til_rel.to_string(),
            pal_path: String::new(),
            dirty: false,
        };
        io::save_map(root, rel, &state).unwrap();
        rel.strip_suffix(".map").unwrap().to_string()
    }

    #[test]
    fn instance_entity_counts_recurse_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("worlds")).unwrap();
        std::fs::write(
            dir.path().join("worlds/leaf.toml"),
            "[[entity]]\nTransform = { pos = [0, 0] }\n[[entity]]\nTransform = { pos = [1, 1] }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("worlds/mid.toml"),
            "[[entity]]\nTransform = { pos = [0, 0] }\n\n[[instance]]\nworld = \"worlds/leaf\"\npos = [0, 0]\n",
        )
        .unwrap();
        let instances = vec![
            WorldInstance {
                world: "worlds/mid".into(),
                pos: [0.0, 0.0],
                background_priority: false,
            },
            WorldInstance {
                world: "worlds/missing".into(),
                pos: [0.0, 0.0],
                background_priority: false,
            },
            WorldInstance {
                world: "worlds/leaf".into(),
                pos: [0.0, 0.0],
                background_priority: false,
            },
        ];
        assert_eq!(instance_entity_counts(dir.path(), &instances), [3, 0, 2]);
    }

    /// A world that instances itself (directly or through a chain) must
    /// terminate rather than recurse forever -- same guard as
    /// `resolve_world_value`'s `visited` chain.
    #[test]
    fn instance_entity_counts_stop_at_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("worlds")).unwrap();
        std::fs::write(
            dir.path().join("worlds/a.toml"),
            "[[entity]]\nTransform = { pos = [0, 0] }\n\n[[instance]]\nworld = \"worlds/b\"\npos = [0, 0]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("worlds/b.toml"),
            "[[entity]]\nTransform = { pos = [0, 0] }\n\n[[instance]]\nworld = \"worlds/a\"\npos = [0, 0]\n",
        )
        .unwrap();
        let instances = vec![WorldInstance {
            world: "worlds/a".into(),
            pos: [0.0, 0.0],
            background_priority: false,
        }];
        assert_eq!(instance_entity_counts(dir.path(), &instances), [2]);
    }

    #[test]
    fn layer_payloads_carry_cells_and_tileset_stem_per_slot() {
        let dir = tempfile::tempdir().unwrap();
        let stem = write_small_map(dir.path(), "maps/m.map", "tiles/a.til", 2, 1, &[7, 1023]);
        let merged = vec![MergedBackground { layer: 2, stem }];
        let payloads = layer_payloads(dir.path(), &merged);
        assert_eq!(payloads.len(), 1);
        assert_eq!((payloads[0].slot, payloads[0].w, payloads[0].h), (2, 2, 1));
        assert_eq!(payloads[0].cells, [7, 1023]);
        assert_eq!(payloads[0].tileset_stem, "tiles/a");
    }

    /// A slot whose `.map` is missing is skipped, not defaulted -- the
    /// panel already reports that slot's failure on the layers rail.
    #[test]
    fn layer_payloads_skip_a_slot_whose_map_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let stem = write_small_map(dir.path(), "maps/there.map", "tiles/a.til", 1, 1, &[0]);
        let merged = vec![
            MergedBackground {
                layer: 0,
                stem: "maps/gone".to_string(),
            },
            MergedBackground { layer: 3, stem },
        ];
        let payloads = layer_payloads(dir.path(), &merged);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].slot, 3);
    }

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

    #[test]
    fn manifest_schemas_unmanaged_project_is_builtins_only() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            manifest_schemas(dir.path()),
            ggo_worldlib::schemas::builtin_schemas()
        );
    }

    #[test]
    fn manifest_schemas_appends_manifest_components_and_gates_on_version() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).unwrap();
        let toml = "version = 1\n\n[[component]]\nname = \"Health\"\n\n[[component.field]]\nname = \"hp\"\nkind = \"int\"\n";
        std::fs::write(manifests.join("components.toml"), toml).unwrap();

        let schemas = manifest_schemas(dir.path());
        let health = schemas.iter().find(|s| s.name == "Health").unwrap();
        assert_eq!(health.fields[0].name, "hp");

        // A newer-than-known version falls back to builtins (same gate as
        // ggo-ide's read_one_manifest).
        std::fs::write(
            manifests.join("components.toml"),
            "version = 99\n\n[[component]]\nname = \"Health\"\n",
        )
        .unwrap();
        assert_eq!(
            manifest_schemas(dir.path()),
            ggo_worldlib::schemas::builtin_schemas()
        );
    }

    /// `manifests/` is the project root's child, NOT the asset root's, so
    /// a world loaded against `<project>/assets` must still find it.
    #[test]
    fn schemas_near_walks_up_from_an_asset_root_to_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(ggo_common::EMERALD_MANIFEST), "").unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        std::fs::create_dir_all(root.join("assets/worlds")).unwrap();
        std::fs::write(
            root.join("manifests/components.toml"),
            "version = 1\n\n[[component]]\nname = \"Health\"\n\n[[component.field]]\nname = \"hp\"\nkind = \"int\"\n",
        )
        .unwrap();

        assert!(
            manifest_schemas(&root.join("assets"))
                .iter()
                .all(|s| s.name != "Health"),
            "the asset root itself has no manifests/ -- this is the bug"
        );
        assert!(
            schemas_near(&root.join("assets"))
                .iter()
                .any(|s| s.name == "Health")
        );
        assert!(schemas_near(root).iter().any(|s| s.name == "Health"));
    }

    /// The load path itself, not just the helper: a world loaded against
    /// its DERIVED asset root (`<project>/assets`, which is what
    /// `split_world_path` hands `load_world`) must still carry the
    /// project's manifest components.
    #[test]
    fn load_world_from_an_asset_root_still_finds_the_projects_components() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(ggo_common::EMERALD_MANIFEST), "").unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        std::fs::create_dir_all(root.join("assets/worlds")).unwrap();
        std::fs::write(
            root.join("manifests/components.toml"),
            "version = 1\n\n[[component]]\nname = \"Health\"\n\n[[component.field]]\nname = \"hp\"\nkind = \"int\"\n",
        )
        .unwrap();
        std::fs::write(root.join("assets/worlds/main.toml"), "version = 1\n").unwrap();

        let loaded = load_world(&root.join("assets"), "worlds/main.toml").unwrap();
        assert!(
            loaded.schemas.iter().any(|s| s.name == "Health"),
            "an asset-rooted load must see <project>/manifests, not <project>/assets/manifests"
        );
    }

    /// No `emerald.toml` anywhere: `schemas_near` is exactly
    /// `manifest_schemas` of the directory it was handed, which is what
    /// keeps the worktree-rooted `worlds/main.toml` layout working.
    #[test]
    fn schemas_near_falls_back_to_the_directory_itself() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join("components.toml"),
            "version = 1\n\n[[component]]\nname = \"Health\"\n",
        )
        .unwrap();
        assert_eq!(schemas_near(dir.path()), manifest_schemas(dir.path()));
        assert!(schemas_near(dir.path()).iter().any(|s| s.name == "Health"));
    }
}
