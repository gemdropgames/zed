//! Reading the project's manifests, and answering "what else breaks if
//! this goes away".
//!
//! The panel's LISTS (components, systems, schedules, grouped by module)
//! and the BLAST RADIUS its confirms quote both come from here. Everything
//! that decides what a manifest entry IS lives in
//! [`ggo_worldlib::emerald`] -- `components_of`/`systems_of`/
//! `schedules_of` do the JSON->typed shaping, `group_by_module` does the
//! bucketing, `schedules_using_system` does the system half of the cascade
//! -- so this module is only the file I/O around them, plus the one query
//! worldlib cannot answer because it needs the world files rather than the
//! manifests: [`worlds_using_component`].
//!
//! No gpui here, on purpose: the panel is glue, and "which schedules break
//! if this system goes" is exactly the kind of claim that should be
//! provable without a window.

use std::path::Path;

use ggo_worldlib::emerald::{
    ComponentEntry, ManifestKind, ScheduleEntry, SystemEntry, components_of, schedules_of,
    schedules_using_system, systems_of,
};
use ggo_worldlib::world_file::read_world;
use ggo_worldlib::world_files::world_files;

use crate::ops::{Cascade, ManifestOp};

/// The directory `emd` keeps the manifests in, under the project root.
pub const MANIFESTS_DIR: &str = "manifests";

/// The `assets/` directory name under an emerald project root -- where
/// `emd generate world` writes, and therefore the only tree
/// [`worlds_using_component`] scans.
pub const ASSETS_DIR: &str = "assets";

/// emerald's `MANIFEST_VERSION`. A manifest whose `version` exceeds this
/// was written by a newer `emd` than this build knows about, and is
/// treated as unreadable rather than half-understood -- the same gate
/// `ggo_world_panel`'s schema feed and ggo-ide's `read_one_manifest` make.
const MANIFEST_VERSION: u64 = 1;

/// Everything the three manifests say, typed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifests {
    pub components: Vec<ComponentEntry>,
    pub systems: Vec<SystemEntry>,
    pub schedules: Vec<ScheduleEntry>,
}

impl Manifests {
    pub fn is_empty(&self) -> bool {
        self.components.is_empty() && self.systems.is_empty() && self.schedules.is_empty()
    }

    pub fn component(&self, name: &str) -> Option<&ComponentEntry> {
        self.components.iter().find(|c| c.name == name)
    }

    pub fn system(&self, name: &str) -> Option<&SystemEntry> {
        self.systems.iter().find(|s| s.name == name)
    }

    pub fn schedule(&self, name: &str) -> Option<&ScheduleEntry> {
        self.schedules.iter().find(|s| s.name == name)
    }
}

/// Read `manifests/{components,systems,schedules}.toml` under
/// `project_dir`. A missing, unparseable or too-new file contributes an
/// empty list rather than an error: an unmanaged project (no `manifests/`
/// at all) is a legitimate state the panel renders as "nothing to list",
/// and one broken file must not blank the other two.
pub fn read_manifests(project_dir: &Path) -> Manifests {
    Manifests {
        components: components_of(read_manifest(project_dir, "components.toml").as_ref()),
        systems: systems_of(read_manifest(project_dir, "systems.toml").as_ref()),
        schedules: schedules_of(read_manifest(project_dir, "schedules.toml").as_ref()),
    }
}

/// One manifest file as JSON, version-gated. `toml::from_str` driving
/// `serde_json::Value`'s own `Deserialize` -- the same TOML->JSON hop
/// `ggo_worldlib::world_file` and ggo-ide's backend make, which is what
/// lets worldlib's `*_of` shapers take a `serde_json::Value`.
fn read_manifest(project_dir: &Path, file: &str) -> Option<serde_json::Value> {
    let path = project_dir.join(MANIFESTS_DIR).join(file);
    let text = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = toml::from_str(&text).ok()?;
    let version = json.get("version").and_then(serde_json::Value::as_u64)?;
    (version <= MANIFEST_VERSION).then_some(json)
}

// ------------------------------------------------------------- blast radius

/// Every world file under `<project_dir>/assets/worlds` that places an
/// entity carrying `component`, as asset-root-relative paths (e.g.
/// `worlds/arena.toml`).
///
/// **What this does NOT see, and why the confirm says so out loud.** A
/// component is referenced in two places: world files place it on
/// entities, and Rust sources name its type. This scan covers the first
/// and only the first -- it reads `<assets>/worlds/**.toml` and matches
/// the manifest's stored (PascalCase) name against each `[[entity]]`
/// table's component keys. Code references are deliberately out of scope:
/// finding them properly means compiling, `emd rm` already does exactly
/// that (`cargo check`, then a revert if it fails), and a grep-shaped
/// approximation that says "3 files" when the compiler says 11 would be
/// worse than admitting the limit. So the confirm quotes the worlds and
/// then says the compiler has the last word.
///
/// Worlds outside `<assets>/worlds` are not scanned either: that is the
/// only place `emd generate world` writes, and it is the tree
/// `ggo_world_panel` browses.
pub fn worlds_using_component(project_dir: &Path, component: &str) -> Vec<String> {
    let assets = project_dir.join(ASSETS_DIR);
    let mut rels: Vec<String> = ggo_worldlib::sprites::io::list_all_files(&assets);
    rels.sort();
    world_files(&rels)
        .into_iter()
        .filter(|listing| {
            read_world(&assets, &listing.rel_path).is_ok_and(|world| {
                world
                    .entities
                    .iter()
                    .any(|e| e.components.contains_key(component))
            })
        })
        .map(|listing| listing.rel_path)
        .collect()
}

/// What else breaks if `op` runs, ready for the confirm body.
///
/// Removing a SYSTEM: worldlib's [`schedules_using_system`] over the
/// schedules manifest -- exact, because a schedule's run list is the only
/// place a system is referenced by name.
///
/// Removing a COMPONENT: [`worlds_using_component`]'s world scan, with its
/// documented limit.
///
/// Removing a SCHEDULE, or editing a component's fields: no cascade this
/// side can compute. A schedule is referenced from Rust (`app.run(...)`)
/// and a field from wherever the struct is read -- both are the compiler's
/// department, which is what [`crate::ops::COMPILER_NOTE`] tells the user
/// instead of pretending there is nothing to lose.
pub fn cascade_for(op: &ManifestOp, manifests: &Manifests, project_dir: &Path) -> Cascade {
    match op {
        ManifestOp::Remove {
            kind: ManifestKind::System,
            name,
            module,
        } => Cascade {
            schedules: schedules_using_system(&manifests.schedules, module, name),
            ..Cascade::default()
        },
        ManifestOp::Remove {
            kind: ManifestKind::Component,
            name,
            ..
        } => Cascade {
            worlds: worlds_using_component(project_dir, name),
            ..Cascade::default()
        },
        _ => Cascade::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project with all three manifests and two worlds.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("emerald.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        std::fs::create_dir_all(root.join("assets/worlds/nested")).unwrap();
        std::fs::write(
            root.join("manifests/components.toml"),
            "version = 1\n\
             [[component]]\nname = \"HeroUnit\"\nmodule = \"gameplay\"\n\
             [[component.field]]\nname = \"hp\"\nkind = \"int\"\n\
             [[component]]\nname = \"Marker\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("manifests/systems.toml"),
            "version = 1\n\
             [[system]]\nname = \"spawn_enemies\"\nmodule = \"gameplay\"\n\
             [[system]]\nname = \"tick_clock\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("manifests/schedules.toml"),
            "version = 1\n\
             [[schedule]]\nname = \"update\"\nsystems = [\"gameplay/spawn_enemies\", \"tick_clock\"]\n\
             [[schedule]]\nname = \"render\"\nsystems = [\"gameplay/spawn_enemies@4\"]\n\
             [[schedule]]\nname = \"idle\"\nsystems = []\n",
        )
        .unwrap();
        std::fs::write(
            root.join("assets/worlds/arena.toml"),
            "[[entity]]\nHeroUnit = { hp = 3 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("assets/worlds/nested/deep.toml"),
            "[[entity]]\nMarker = {}\n[[entity]]\nHeroUnit = { hp = 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("assets/worlds/empty.toml"),
            "[[entity]]\nTransform = { pos = [0, 0] }\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn read_manifests_types_all_three_files() {
        let dir = project();
        let m = read_manifests(dir.path());
        assert_eq!(
            m.components.iter().map(|c| &c.name).collect::<Vec<_>>(),
            ["HeroUnit", "Marker"]
        );
        assert_eq!(m.component("HeroUnit").unwrap().module, "gameplay");
        assert_eq!(m.component("HeroUnit").unwrap().fields.len(), 1);
        assert_eq!(
            m.systems.iter().map(|s| &s.name).collect::<Vec<_>>(),
            ["spawn_enemies", "tick_clock"]
        );
        assert_eq!(
            m.schedule("update").unwrap().systems,
            ["gameplay/spawn_enemies", "tick_clock"]
        );
        assert!(!m.is_empty());
    }

    /// A project with no `manifests/` at all is empty, not an error -- and
    /// one unreadable file does not blank the other two.
    #[test]
    fn a_missing_or_too_new_manifest_is_an_empty_list_not_a_failure() {
        let empty = tempfile::tempdir().unwrap();
        assert!(read_manifests(empty.path()).is_empty());

        let dir = project();
        std::fs::write(
            dir.path().join("manifests/components.toml"),
            "version = 99\n[[component]]\nname = \"FromTheFuture\"\n",
        )
        .unwrap();
        let m = read_manifests(dir.path());
        assert!(
            m.components.is_empty(),
            "a newer manifest is not guessed at"
        );
        assert_eq!(m.systems.len(), 2, "the other two still load");

        std::fs::write(dir.path().join("manifests/systems.toml"), "not [ toml").unwrap();
        assert!(read_manifests(dir.path()).systems.is_empty());
    }

    /// The component half of the cascade: every world placing the
    /// component, nested ones included, and nothing else.
    #[test]
    fn worlds_using_component_finds_every_world_that_places_it() {
        let dir = project();
        assert_eq!(
            worlds_using_component(dir.path(), "HeroUnit"),
            ["worlds/arena.toml", "worlds/nested/deep.toml"]
        );
        assert_eq!(
            worlds_using_component(dir.path(), "Marker"),
            ["worlds/nested/deep.toml"]
        );
        assert!(worlds_using_component(dir.path(), "NotPlacedAnywhere").is_empty());
        // A project with no assets tree at all scans to nothing.
        let bare = tempfile::tempdir().unwrap();
        assert!(worlds_using_component(bare.path(), "HeroUnit").is_empty());
    }

    /// The system half is worldlib's, cadence suffixes included
    /// (`gameplay/spawn_enemies@4` still counts as a reference).
    #[test]
    fn cascade_for_a_system_names_its_schedules() {
        let dir = project();
        let m = read_manifests(dir.path());
        let cascade = cascade_for(
            &ManifestOp::remove(ManifestKind::System, "spawn_enemies", "gameplay"),
            &m,
            dir.path(),
        );
        assert_eq!(cascade.schedules, ["update", "render"]);
        assert!(cascade.worlds.is_empty());

        let unused = cascade_for(
            &ManifestOp::remove(ManifestKind::System, "tick_clock", ""),
            &m,
            dir.path(),
        );
        assert_eq!(unused.schedules, ["update"]);
    }

    #[test]
    fn cascade_for_a_component_names_its_worlds_and_for_a_schedule_names_nothing() {
        let dir = project();
        let m = read_manifests(dir.path());
        let component = cascade_for(
            &ManifestOp::remove(ManifestKind::Component, "HeroUnit", "gameplay"),
            &m,
            dir.path(),
        );
        assert_eq!(
            component.worlds,
            ["worlds/arena.toml", "worlds/nested/deep.toml"]
        );
        assert!(component.schedules.is_empty());

        let schedule = cascade_for(
            &ManifestOp::remove(ManifestKind::Schedule, "update", ""),
            &m,
            dir.path(),
        );
        assert_eq!(
            schedule,
            Cascade::default(),
            "nothing this side can compute for a schedule"
        );
        assert_eq!(
            cascade_for(
                &ManifestOp::field_remove("HeroUnit", "gameplay", "hp"),
                &m,
                dir.path()
            ),
            Cascade::default()
        );
    }
}
