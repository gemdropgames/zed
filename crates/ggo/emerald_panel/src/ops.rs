//! The pure half of the manifest mutations: what a remove/field edit IS,
//! the `emd` argv it becomes, and -- the substance of the task -- what the
//! user is told before it runs.
//!
//! Same split as [`crate::forms`] and for the same reason: the argv comes
//! from [`ggo_worldlib::emerald`]'s builders verbatim
//! (`build_rm_args`/`build_field_add_args`/`build_field_rm_args`/
//! `build_schedule_set_args`), and the
//! confirm text is assembled here so "the schedules that break are named
//! in the prompt" is a claim provable without a window.
//!
//! Nothing here touches the filesystem. The cascade DATA is gathered by
//! [`crate::manifests::cascade_for`] (which needs the manifests and the
//! world files); this module only decides how it reads.

use ggo_worldlib::emerald::{
    ManifestKind, build_field_add_args, build_field_rm_args, build_rm_args,
    build_schedule_set_args, parse_cadenced_ref, system_ref,
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
/// confirm, a success message and a post-run refresh that differ. The
/// panel's run path takes one of these, so a new op is one variant here
/// plus one [`confirm_for`] arm -- which is exactly what F5.3/E3's
/// [`ManifestOp::ScheduleSet`] cost.
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
    /// `emd schedule set <name> --module <m> --systems <a,b,c>` -- the
    /// schedule run-list editor's one command.
    ///
    /// `systems` is the WHOLE new run list, because that is the only
    /// thing `emd schedule set` accepts: it replaces the list outright
    /// (`commands::schedule::set`), so a reorder and a removal issue the
    /// same shape of command and differ only in the list they carry.
    /// `edit` is this side's record of WHICH edit produced that list --
    /// the argv cannot say, and the confirm and the success message both
    /// need to.
    ScheduleSet {
        schedule: String,
        module: String,
        systems: Vec<String>,
        edit: ScheduleEdit,
    },
}

/// What a [`ManifestOp::ScheduleSet`] is doing to the run list.
///
/// Not derivable from the argv (a whole-list write looks the same however
/// it was produced) and not derivable from the old and new lists either
/// without re-deriving the edit, which is what
/// [`ggo_worldlib::emerald::OrderEdit`] already described. So the panel
/// carries the intent alongside the result, and it is what decides both
/// whether a confirm is raised and what the run reports afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleEdit {
    /// A system appended to the end of the run list.
    Add { system_ref: String },
    /// The entry at `index` dropped -- the one destructive schedule edit.
    /// `system_ref` is the entry AS IT STOOD, cadence suffix included.
    Remove { system_ref: String, index: usize },
    /// The entry at `from` moved to `to` (a post-removal index, per
    /// [`ggo_worldlib::emerald::OrderEdit::Move`]).
    Move {
        system_ref: String,
        from: usize,
        to: usize,
    },
    /// An entry's `@N` cadence changed. `system_ref` is the BASE ref, with
    /// no suffix; `cadence` is the new one (1 = every tick, no suffix).
    Cadence { system_ref: String, cadence: u32 },
}

impl ScheduleEdit {
    /// The run-list entry this edit is about, without its `@N` suffix --
    /// what the user calls the row.
    fn base(&self) -> String {
        match self {
            ScheduleEdit::Add { system_ref }
            | ScheduleEdit::Remove { system_ref, .. }
            | ScheduleEdit::Move { system_ref, .. }
            | ScheduleEdit::Cadence { system_ref, .. } => parse_cadenced_ref(system_ref).0,
        }
    }
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

    pub fn schedule_set(
        schedule: &str,
        module: &str,
        systems: Vec<String>,
        edit: ScheduleEdit,
    ) -> Self {
        ManifestOp::ScheduleSet {
            schedule: schedule.to_string(),
            module: module.to_string(),
            systems,
            edit,
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
            ManifestOp::ScheduleSet {
                schedule,
                module,
                systems,
                ..
            } => build_schedule_set_args(schedule, module, systems),
        }
    }

    /// Does this need a confirm before it runs? Everything destructive
    /// does; adding a field is the one op that only ever ADDS, so it goes
    /// straight through, and three of the four schedule edits are moves of
    /// something that stays in the list.
    ///
    /// **Dropping an entry from a run list IS destructive**, even though
    /// the system itself survives in the manifest: the run list is
    /// ordered, and `OrderEdit::Add` appends -- so putting the entry back
    /// puts it at the END, not where it was, and its `@N` cadence is gone
    /// too. That is a real loss, and [`confirm_for`] says so in those
    /// words rather than warning in the abstract.
    pub fn destructive(&self) -> bool {
        match self {
            ManifestOp::FieldAdd { .. } => false,
            ManifestOp::ScheduleSet { edit, .. } => {
                matches!(edit, ScheduleEdit::Remove { .. })
            }
            ManifestOp::Remove { .. } | ManifestOp::FieldRemove { .. } => true,
        }
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
            ManifestOp::ScheduleSet {
                schedule,
                module,
                edit,
                ..
            } => {
                let base = edit.base();
                let sched = qualified(module, schedule);
                match edit {
                    ScheduleEdit::Add { .. } => format!("Added {base} to schedule {sched}"),
                    ScheduleEdit::Remove { .. } => {
                        format!("Removed {base} from schedule {sched}")
                    }
                    ScheduleEdit::Move { to, .. } => {
                        format!("Moved {base} to position {} in schedule {sched}", to + 1)
                    }
                    ScheduleEdit::Cadence { cadence, .. } => {
                        format!("{base} now runs {} in schedule {sched}", every(*cadence))
                    }
                }
            }
        }
    }
}

/// `"every tick"` / `"every 4 ticks"` -- how a cadence reads in prose.
/// Cadence 1 is the no-suffix baseline
/// ([`ggo_worldlib::emerald::with_cadence`]), so it has no number to say.
pub fn every(cadence: u32) -> String {
    if cadence <= 1 {
        "every tick".to_string()
    } else {
        format!("every {cadence} ticks")
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
///
/// The run-list removal is the one confirm that does NOT carry
/// [`COMPILER_NOTE`], because it would be false: `emd schedule set`
/// rewrites the manifest and regenerates the schedule's builder file and
/// stops there -- no `cargo check`, no revert (emerald's
/// `commands::schedule::set`). It is also the fast op of the four, so the
/// "30s or more" half of that note would be wrong twice over.
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
        ManifestOp::ScheduleSet {
            schedule,
            module,
            edit:
                ScheduleEdit::Remove {
                    system_ref: entry,
                    index,
                },
            ..
        } => {
            let (base, cadence) = parse_cadenced_ref(entry);
            lines.push("The system itself stays in the manifest.".to_string());
            // The specific loss, named: this is the only op in the panel
            // whose damage is POSITIONAL, and "add it back" is a click
            // away, so an abstract warning would read as noise. Position
            // is 1-based here because that is how the rows are numbered
            // on screen.
            lines.push(format!(
                "Adding it back appends it to the end of the run list, not to position {}.",
                index + 1
            ));
            if cadence > 1 {
                lines.push(format!(
                    "Its cadence ({}) is not restored either.",
                    every(cadence)
                ));
            }
            return Some(Confirm {
                message: format!(
                    "Remove {base} from the schedule {}?",
                    qualified(module, schedule)
                ),
                cascade: lines,
                label: "Remove",
            });
        }
        // `destructive()` already returned early for these two.
        ManifestOp::FieldAdd { .. } | ManifestOp::ScheduleSet { .. } => return None,
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
        let order = vec!["tick_clock".to_string(), "gameplay/spawn@4".to_string()];
        assert_eq!(
            ManifestOp::schedule_set(
                "update",
                "",
                order.clone(),
                ScheduleEdit::Move {
                    system_ref: "gameplay/spawn@4".into(),
                    from: 0,
                    to: 1
                }
            )
            .args(),
            build_schedule_set_args("update", "", &order)
        );
        assert_eq!(
            ManifestOp::schedule_set(
                "update",
                "",
                order,
                ScheduleEdit::Add {
                    system_ref: "tick_clock".into()
                }
            )
            .args(),
            [
                "schedule",
                "set",
                "update",
                "--module",
                "",
                "--systems",
                "tick_clock,gameplay/spawn@4"
            ],
            "the whole list travels in one comma-joined --systems value"
        );
        assert_eq!(
            ManifestOp::schedule_set(
                "update",
                "",
                Vec::new(),
                ScheduleEdit::Remove {
                    system_ref: "tick_clock".into(),
                    index: 0
                }
            )
            .args(),
            ["schedule", "set", "update", "--module", ""],
            "emptying a run list omits --systems entirely (worldlib's own rule)"
        );
    }

    /// The run-list removal is the one schedule edit that confirms, and it
    /// names the SPECIFIC loss -- the position, and the cadence -- rather
    /// than warning in the abstract. The other three go straight through.
    #[test]
    fn only_dropping_a_run_list_entry_confirms_and_it_names_what_is_lost() {
        let removal = ManifestOp::schedule_set(
            "update",
            "gameplay",
            vec!["tick_clock".to_string()],
            ScheduleEdit::Remove {
                system_ref: "gameplay/spawn_enemies@4".into(),
                index: 2,
            },
        );
        assert!(removal.destructive());
        let confirm = confirm_for(&removal, &Cascade::default()).unwrap();
        assert_eq!(
            confirm.message,
            "Remove gameplay/spawn_enemies from the schedule gameplay/update?"
        );
        assert_eq!(
            confirm.cascade,
            [
                "The system itself stays in the manifest.",
                "Adding it back appends it to the end of the run list, not to position 3.",
                "Its cadence (every 4 ticks) is not restored either.",
            ]
        );
        assert!(
            !confirm.cascade.iter().any(|l| l == COMPILER_NOTE),
            "`emd schedule set` runs no compiler check, so it must not promise one"
        );

        // A cadence-1 entry has no cadence to lose, and says one fewer
        // thing rather than saying "every tick" pointlessly.
        let plain = ManifestOp::schedule_set(
            "update",
            "",
            Vec::new(),
            ScheduleEdit::Remove {
                system_ref: "tick_clock".into(),
                index: 0,
            },
        );
        let confirm = confirm_for(&plain, &Cascade::default()).unwrap();
        assert_eq!(
            confirm.message,
            "Remove tick_clock from the schedule update?"
        );
        assert_eq!(confirm.cascade.len(), 2, "{:?}", confirm.cascade);

        for edit in [
            ScheduleEdit::Add {
                system_ref: "tick_clock".into(),
            },
            ScheduleEdit::Move {
                system_ref: "tick_clock".into(),
                from: 1,
                to: 0,
            },
            ScheduleEdit::Cadence {
                system_ref: "tick_clock".into(),
                cadence: 4,
            },
        ] {
            let op = ManifestOp::schedule_set("update", "", vec!["tick_clock".into()], edit);
            assert!(!op.destructive(), "{op:?}");
            assert_eq!(confirm_for(&op, &Cascade::default()), None, "{op:?}");
        }
    }

    /// Every schedule edit reports what IT did -- the argv cannot say
    /// (all four are the same whole-list write).
    #[test]
    fn a_schedule_edit_reports_which_edit_it_was() {
        let done = |edit| {
            ManifestOp::schedule_set("update", "", vec!["tick_clock".into()], edit).done_message()
        };
        assert_eq!(
            done(ScheduleEdit::Add {
                system_ref: "gameplay/spawn".into()
            }),
            "Added gameplay/spawn to schedule update"
        );
        assert_eq!(
            done(ScheduleEdit::Remove {
                system_ref: "gameplay/spawn@4".into(),
                index: 1
            }),
            "Removed gameplay/spawn from schedule update",
            "the cadence suffix is not part of the system's name"
        );
        assert_eq!(
            done(ScheduleEdit::Move {
                system_ref: "gameplay/spawn".into(),
                from: 2,
                to: 0
            }),
            "Moved gameplay/spawn to position 1 in schedule update",
            "positions read 1-based, as the rows are numbered"
        );
        assert_eq!(
            done(ScheduleEdit::Cadence {
                system_ref: "gameplay/spawn".into(),
                cadence: 4
            }),
            "gameplay/spawn now runs every 4 ticks in schedule update"
        );
        assert_eq!(
            done(ScheduleEdit::Cadence {
                system_ref: "gameplay/spawn".into(),
                cadence: 1
            }),
            "gameplay/spawn now runs every tick in schedule update"
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
