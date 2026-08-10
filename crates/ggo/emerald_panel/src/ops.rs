//! The pure half of the manifest mutations: what a remove/field edit IS,
//! the `emd` argv it becomes, and -- the substance of the task -- what the
//! user is told before it runs.
//!
//! Same split as [`crate::forms`] and for the same reason: the argv comes
//! from [`ggo_worldlib::emerald`]'s builders verbatim
//! (`build_rm_args`/`build_field_add_args`/`build_field_rm_args`), and the
//! confirm text is assembled here so "the schedules that break are named
//! in the prompt" is a claim provable without a window.
//!
//! Nothing here touches the filesystem. The cascade DATA is gathered by
//! [`crate::manifests::cascade_for`] (which needs the manifests and the
//! world files); this module only decides how it reads.

use ggo_worldlib::emerald::{
    ManifestKind, build_field_add_args, build_field_rm_args, build_rm_args, system_ref,
};

/// What every `cargo check`-backed mutation's confirm says about the part
/// this side cannot see.
///
/// It is not hedging: `emd rm` and `component field rm` apply the change,
/// run `cargo check`, and roll the whole thing back if the project stops
/// compiling (which is what
/// [`ggo_worldlib::emerald::emd_reverted`] reports and why the panel has a
/// Reverted state distinct from Failed). Saying so up front is also the
/// honest answer to "how long will this take" -- ggo-ide's own field-remove
/// confirm warned about 30s in the same words.
pub const COMPILER_NOTE: &str = "emd runs a compiler check and rolls the change back if the project stops \
     compiling; this can take 30s or more.";

/// The limit of the component blast radius, said out loud in the prompt --
/// see [`crate::manifests::worlds_using_component`] for why code
/// references are the compiler's job rather than this panel's.
pub const CODE_SCAN_NOTE: &str = "Rust code that names it is not scanned.";

/// One manifest mutation, ready to be confirmed and run.
///
/// A closed enum rather than a free-form argv because every variant has a
/// confirm, a success message and a post-run refresh that differ, and
/// because Task E3's `schedule set` will be the fourth variant -- the
/// panel's run path takes one of these, so adding it there is the only
/// change that will need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestOp {
    /// `emd rm <kind> <name> --module <m>`.
    Remove {
        kind: ManifestKind,
        name: String,
        module: String,
    },
    /// `emd component field add <component> <spec> --module <m>`. `spec`
    /// is the whole `name:kind` pair, as
    /// [`crate::forms::FieldDraft`] assembles it.
    FieldAdd {
        component: String,
        module: String,
        spec: String,
    },
    /// `emd component field rm <component> <field> --module <m>`.
    FieldRemove {
        component: String,
        module: String,
        field: String,
    },
}

impl ManifestOp {
    pub fn remove(kind: ManifestKind, name: &str, module: &str) -> Self {
        ManifestOp::Remove {
            kind,
            name: name.to_string(),
            module: module.to_string(),
        }
    }

    pub fn field_add(component: &str, module: &str, spec: &str) -> Self {
        ManifestOp::FieldAdd {
            component: component.to_string(),
            module: module.to_string(),
            spec: spec.to_string(),
        }
    }

    pub fn field_remove(component: &str, module: &str, field: &str) -> Self {
        ManifestOp::FieldRemove {
            component: component.to_string(),
            module: module.to_string(),
            field: field.to_string(),
        }
    }

    /// The `emd` argv, minus the `--json` flag
    /// [`crate::runner::EmdRequest::emd`] appends. Every arm is a worldlib
    /// builder call and nothing else -- the `--module ""` that
    /// disambiguates a shared item is theirs to emit, not this module's to
    /// remember.
    pub fn args(&self) -> Vec<String> {
        match self {
            ManifestOp::Remove { kind, name, module } => build_rm_args(*kind, name, module),
            ManifestOp::FieldAdd {
                component,
                module,
                spec,
            } => build_field_add_args(component, module, spec),
            ManifestOp::FieldRemove {
                component,
                module,
                field,
            } => build_field_rm_args(component, module, field),
        }
    }

    /// Does this need a confirm before it runs? Everything destructive
    /// does; adding a field is the one op that only ever ADDS, so it goes
    /// straight through.
    pub fn destructive(&self) -> bool {
        !matches!(self, ManifestOp::FieldAdd { .. })
    }

    /// What the run state says after this succeeds.
    pub fn done_message(&self) -> String {
        match self {
            ManifestOp::Remove { kind, name, module } => {
                format!("Removed {} {}", kind_noun(*kind), qualified(module, name))
            }
            ManifestOp::FieldAdd {
                component, spec, ..
            } => format!("Added field {spec} to {component}"),
            ManifestOp::FieldRemove {
                component, field, ..
            } => format!("Removed field {field} from {component}"),
        }
    }
}

/// What else breaks if a [`ManifestOp`] runs. Gathered by
/// [`crate::manifests::cascade_for`]; rendered by [`confirm_for`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cascade {
    /// Schedules whose run list references the system being removed
    /// (`ggo_worldlib::emerald::schedules_using_system`).
    pub schedules: Vec<String>,
    /// Asset-root-relative world files placing the component being removed
    /// (`crate::manifests::worlds_using_component`).
    pub worlds: Vec<String>,
}

/// A confirmation to raise before a [`ManifestOp`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    /// The prompt's title: what is about to happen.
    pub message: String,
    /// The prompt's body, one claim per line -- the cascade, then the
    /// limits of what was checked. `ggo_common::confirm_destructive_cascade`
    /// appends "This cannot be undone." below it.
    pub cascade: Vec<String>,
    /// The go-ahead button's verb.
    pub label: &'static str,
}

/// The confirmation for `op`, or `None` when it needs none
/// ([`ManifestOp::destructive`]).
///
/// **Nothing destructive is ever silent**: even an op with an empty
/// [`Cascade`] gets a prompt, because "nothing this side could find" is
/// not the same claim as "nothing depends on it" -- which is exactly what
/// the body then says, naming what WAS checked and what was not.
pub fn confirm_for(op: &ManifestOp, cascade: &Cascade) -> Option<Confirm> {
    if !op.destructive() {
        return None;
    }
    let mut lines = Vec::new();
    let message = match op {
        ManifestOp::Remove {
            kind: ManifestKind::System,
            name,
            module,
        } => {
            if !cascade.schedules.is_empty() {
                lines.push(format!(
                    "Also removed from {}: {}.",
                    count(cascade.schedules.len(), "schedule"),
                    cascade.schedules.join(", ")
                ));
            } else {
                lines.push("No schedule's run list references it.".to_string());
            }
            format!("Remove the system {}?", qualified(module, name))
        }
        ManifestOp::Remove {
            kind: ManifestKind::Component,
            name,
            module,
        } => {
            if !cascade.worlds.is_empty() {
                lines.push(format!(
                    "Still placed in {}: {}.",
                    count(cascade.worlds.len(), "world"),
                    cascade.worlds.join(", ")
                ));
            } else {
                lines.push("No world under assets/worlds places it.".to_string());
            }
            lines.push(CODE_SCAN_NOTE.to_string());
            format!("Remove the component {}?", qualified(module, name))
        }
        ManifestOp::Remove {
            kind: ManifestKind::Schedule,
            name,
            module,
        } => format!("Remove the schedule {}?", qualified(module, name)),
        ManifestOp::FieldRemove {
            component,
            module,
            field,
        } => format!(
            "Remove the field {field} from {}?",
            qualified(module, component)
        ),
        // `destructive()` already returned early for this one.
        ManifestOp::FieldAdd { .. } => return None,
    };
    lines.push(COMPILER_NOTE.to_string());
    Some(Confirm {
        message,
        cascade: lines,
        label: "Remove",
    })
}

/// `<module>/<name>`, or a bare `<name>` for a shared item -- the manifest's
/// own ref shape, via worldlib's [`system_ref`].
///
/// Named for systems there because that is where the shape is
/// load-bearing (a schedule's run list is written in it), but the
/// module-qualification convention is the same for all three kinds, and
/// re-spelling `if module.is_empty()` here would be a second copy of it.
pub fn qualified(module: &str, name: &str) -> String {
    system_ref(module, name)
}

/// `"2 schedules"` / `"1 schedule"`.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Lowercase noun for a manifest kind, for messages. Display only -- the
/// argv's own spelling of the kind is `build_rm_args`' business (worldlib
/// keeps it private for exactly that reason), and nothing here feeds a
/// command line.
fn kind_noun(kind: ManifestKind) -> &'static str {
    match kind {
        ManifestKind::Component => "component",
        ManifestKind::System => "system",
        ManifestKind::Schedule => "schedule",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every op's argv must be byte-for-byte worldlib's builder output --
    /// the contract that keeps the fork and ggo-ide issuing the same
    /// commands, including the `--module ""` a shared item needs.
    #[test]
    fn args_match_worldlibs_builders_for_every_op() {
        assert_eq!(
            ManifestOp::remove(ManifestKind::Component, "HeroUnit", "gameplay").args(),
            build_rm_args(ManifestKind::Component, "HeroUnit", "gameplay")
        );
        assert_eq!(
            ManifestOp::remove(ManifestKind::Component, "HeroUnit", "gameplay").args(),
            ["rm", "component", "HeroUnit", "--module", "gameplay"]
        );
        assert_eq!(
            ManifestOp::remove(ManifestKind::System, "spawn_enemies", "").args(),
            ["rm", "system", "spawn_enemies", "--module", ""],
            "a shared item still passes --module, precisely empty"
        );
        assert_eq!(
            ManifestOp::remove(ManifestKind::Schedule, "update", "").args(),
            build_rm_args(ManifestKind::Schedule, "update", "")
        );
        assert_eq!(
            ManifestOp::field_add("HeroUnit", "gameplay", "hp:int").args(),
            build_field_add_args("HeroUnit", "gameplay", "hp:int")
        );
        assert_eq!(
            ManifestOp::field_add("HeroUnit", "gameplay", "hp:int").args(),
            [
                "component",
                "field",
                "add",
                "HeroUnit",
                "hp:int",
                "--module",
                "gameplay"
            ]
        );
        assert_eq!(
            ManifestOp::field_remove("HeroUnit", "gameplay", "hp").args(),
            build_field_rm_args("HeroUnit", "gameplay", "hp")
        );
    }

    /// **The cascade**: a system in two schedules names both, in the
    /// prompt body, before anything runs.
    #[test]
    fn a_system_in_schedules_names_them_in_the_confirm() {
        let cascade = Cascade {
            schedules: vec!["update".into(), "render".into()],
            ..Cascade::default()
        };
        let confirm = confirm_for(
            &ManifestOp::remove(ManifestKind::System, "spawn_enemies", "gameplay"),
            &cascade,
        )
        .expect("removing a system is destructive");
        assert_eq!(confirm.message, "Remove the system gameplay/spawn_enemies?");
        assert_eq!(confirm.label, "Remove");
        assert_eq!(
            confirm.cascade[0],
            "Also removed from 2 schedules: update, render."
        );
        assert_eq!(confirm.cascade.last().unwrap(), COMPILER_NOTE);

        let one = confirm_for(
            &ManifestOp::remove(ManifestKind::System, "spawn_enemies", ""),
            &Cascade {
                schedules: vec!["update".into()],
                ..Cascade::default()
            },
        )
        .unwrap();
        assert_eq!(one.message, "Remove the system spawn_enemies?");
        assert_eq!(one.cascade[0], "Also removed from 1 schedule: update.");
    }

    /// An EMPTY cascade still confirms, and still says what was checked --
    /// "nothing found" must not read as "nothing to lose".
    #[test]
    fn an_unreferenced_system_still_confirms_and_says_what_was_checked() {
        let confirm = confirm_for(
            &ManifestOp::remove(ManifestKind::System, "lonely", ""),
            &Cascade::default(),
        )
        .unwrap();
        assert_eq!(confirm.cascade[0], "No schedule's run list references it.");
        assert!(confirm.cascade.contains(&COMPILER_NOTE.to_string()));
    }

    /// The component blast radius names the worlds AND states its limit --
    /// the world scan is real, the code scan is the compiler's.
    #[test]
    fn a_component_confirm_names_its_worlds_and_admits_the_limit() {
        let confirm = confirm_for(
            &ManifestOp::remove(ManifestKind::Component, "HeroUnit", "gameplay"),
            &Cascade {
                worlds: vec!["worlds/arena.toml".into(), "worlds/nested/deep.toml".into()],
                ..Cascade::default()
            },
        )
        .unwrap();
        assert_eq!(confirm.message, "Remove the component gameplay/HeroUnit?");
        assert_eq!(
            confirm.cascade[0],
            "Still placed in 2 worlds: worlds/arena.toml, worlds/nested/deep.toml."
        );
        assert!(confirm.cascade.contains(&CODE_SCAN_NOTE.to_string()));
        assert!(confirm.cascade.contains(&COMPILER_NOTE.to_string()));

        let unplaced = confirm_for(
            &ManifestOp::remove(ManifestKind::Component, "Marker", ""),
            &Cascade::default(),
        )
        .unwrap();
        assert_eq!(
            unplaced.cascade[0],
            "No world under assets/worlds places it."
        );
        assert!(
            unplaced.cascade.contains(&CODE_SCAN_NOTE.to_string()),
            "the limit is stated whether or not the scan found anything"
        );
    }

    /// Removing a schedule and removing a field both confirm (nothing
    /// destructive is silent), with the compiler note and no invented
    /// cascade. Adding a field is the one op that does not confirm.
    #[test]
    fn the_remaining_ops_confirm_without_inventing_a_cascade() {
        let schedule = confirm_for(
            &ManifestOp::remove(ManifestKind::Schedule, "update", ""),
            &Cascade::default(),
        )
        .unwrap();
        assert_eq!(schedule.message, "Remove the schedule update?");
        assert_eq!(schedule.cascade, [COMPILER_NOTE]);

        let field = confirm_for(
            &ManifestOp::field_remove("HeroUnit", "gameplay", "hp"),
            &Cascade::default(),
        )
        .unwrap();
        assert_eq!(field.message, "Remove the field hp from gameplay/HeroUnit?");
        assert_eq!(field.cascade, [COMPILER_NOTE]);

        let add = ManifestOp::field_add("HeroUnit", "gameplay", "hp:int");
        assert!(!add.destructive());
        assert_eq!(confirm_for(&add, &Cascade::default()), None);
    }

    #[test]
    fn done_messages_name_the_thing_that_changed() {
        assert_eq!(
            ManifestOp::remove(ManifestKind::System, "spawn_enemies", "gameplay").done_message(),
            "Removed system gameplay/spawn_enemies"
        );
        assert_eq!(
            ManifestOp::remove(ManifestKind::Component, "Marker", "").done_message(),
            "Removed component Marker"
        );
        assert_eq!(
            ManifestOp::field_add("HeroUnit", "gameplay", "hp:int").done_message(),
            "Added field hp:int to HeroUnit"
        );
        assert_eq!(
            ManifestOp::field_remove("HeroUnit", "gameplay", "hp").done_message(),
            "Removed field hp from HeroUnit"
        );
    }
}
