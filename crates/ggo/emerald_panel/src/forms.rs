//! The pure half of the generate forms: what the user has typed, whether
//! it is submittable, and the `emd` argv it turns into.
//!
//! Every rule here is worldlib's, not this crate's --
//! [`valid_item_name`]/[`valid_field_spec`]/[`field_kind_spec`]/
//! [`to_pascal_case_preview`] and the `build_generate_*_args` builders all
//! came out of ggo-ide in F5.2/S1 precisely so the fork and ggo-ide agree
//! on what `emd` will accept. Nothing in this module re-implements a
//! validator or hand-assembles an argv that a builder already covers.
//!
//! It is a separate module from the panel so the "never shell out with a
//! name the CLI will reject" guarantee is testable without gpui: the panel
//! reads its editors into a [`GenDraft`], and the ONLY path to a spawn is
//! `GenDraft::error() == None` followed by `GenDraft::args()`.

use ggo_worldlib::emerald::{
    FieldEntry, build_generate_component_args, build_generate_schedule_args,
    build_generate_system_args, field_kind_spec, gen_module_args, to_pascal_case_preview,
    valid_field_spec, valid_item_name,
};

/// The field-kind vocabulary as a FORM offers it: worldlib's five plain
/// kinds plus the bare token `"asset"`, which stands in for the
/// parameterized sixth kind and is combined with the extension input by
/// [`field_kind_spec`]. Same split ggo-ide's `<select>` made, and the
/// reason `field_kind_spec` takes two arguments at all.
pub const FIELD_KINDS: [&str; 6] = ["int", "fixed", "bool", "str", "vec2", "asset"];

/// The member of [`FIELD_KINDS`] that needs the extension input.
pub const ASSET_KIND: &str = "asset";

/// What a new field row starts as.
pub const DEFAULT_FIELD_KIND: &str = "int";

/// Which `emd generate` subcommand a form drives.
///
/// All six exist as forms even though only some have their own
/// context-menu entry: the form carries a kind selector, so Resource and
/// Module are reachable by switching kind inside a form the menu opened
/// (see the panel's `contribute_emerald_menu` for why the menu itself
/// stays at three entries).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenKind {
    Component,
    System,
    Resource,
    Module,
    World,
    Schedule,
}

impl GenKind {
    /// Every kind, in the order the form's selector lists them --
    /// `emd generate --help`'s own order.
    pub const ALL: [GenKind; 6] = [
        GenKind::Component,
        GenKind::System,
        GenKind::Resource,
        GenKind::Module,
        GenKind::World,
        GenKind::Schedule,
    ];

    /// The `emd generate` subcommand.
    pub fn subcommand(self) -> &'static str {
        match self {
            GenKind::Component => "component",
            GenKind::System => "system",
            GenKind::Resource => "resource",
            GenKind::Module => "module",
            GenKind::World => "world",
            GenKind::Schedule => "schedule",
        }
    }

    /// Title-case noun for labels and messages.
    pub fn noun(self) -> &'static str {
        match self {
            GenKind::Component => "Component",
            GenKind::System => "System",
            GenKind::Resource => "Resource",
            GenKind::Module => "Module",
            GenKind::World => "World",
            GenKind::Schedule => "Schedule",
        }
    }

    /// A valid example name, shown in the placeholder and in the
    /// snake_case error.
    pub fn example(self) -> &'static str {
        match self {
            GenKind::Component => "hero_unit",
            GenKind::System => "spawn_enemies",
            GenKind::Resource => "game_config",
            GenKind::Module => "gameplay",
            GenKind::World => "arena",
            GenKind::Schedule => "tick",
        }
    }

    /// Does this subcommand take `--module`? `module` and `world` do not:
    /// a module IS the module, and a world is an asset file, not a
    /// module-scoped item (`emd generate world --help` says so, and it is
    /// why the world form has no module row).
    pub fn takes_module(self) -> bool {
        !matches!(self, GenKind::Module | GenKind::World)
    }

    /// Only `component` takes `--field`.
    pub fn takes_fields(self) -> bool {
        matches!(self, GenKind::Component)
    }

    /// Is the typed snake_case name PascalCase-converted before it is
    /// stored? True for `component`, whose `manifests/components.toml`
    /// entry records `HeroUnit` for a typed `hero_unit` -- which is why
    /// the form shows a "stored as" preview, exactly where ggo-ide's
    /// `new_component_form` showed one, and only there.
    pub fn pascal_cased(self) -> bool {
        matches!(self, GenKind::Component)
    }
}

/// One `--field name:kind` row, split the way a form collects it: the kind
/// selector's value and the asset extension are separate inputs, combined
/// by [`field_kind_spec`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDraft {
    pub name: String,
    pub kind: String,
    pub ext: String,
}

impl Default for FieldDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: DEFAULT_FIELD_KIND.to_string(),
            ext: String::new(),
        }
    }
}

impl FieldDraft {
    /// The `kind` half of the `name:kind` spec `emd` expects.
    pub fn spec(&self) -> String {
        field_kind_spec(&self.kind, &self.ext)
    }

    /// The worldlib entry this row becomes, for the argv builder.
    pub fn entry(&self) -> FieldEntry {
        FieldEntry {
            name: self.name.clone(),
            kind: self.spec(),
        }
    }
}

/// A whole form's worth of typed text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenDraft {
    pub kind: GenKind,
    pub name: String,
    /// Blank means the shared module -- `gen_module_args` omits `--module`
    /// entirely for it, which is how `emd` spells "not in a module".
    pub module: String,
    pub fields: Vec<FieldDraft>,
}

impl GenDraft {
    pub fn new(kind: GenKind) -> Self {
        Self {
            kind,
            name: String::new(),
            module: String::new(),
            fields: Vec::new(),
        }
    }

    /// Nothing typed yet -- the panel renders no error in this state, so a
    /// just-opened form does not greet the user with a complaint (ggo-ide
    /// gated its own message on `!new_name.is_empty()` for the same
    /// reason). Submit is still disabled: that comes from [`Self::error`].
    pub fn pristine(&self) -> bool {
        self.name.is_empty()
            && self.module.is_empty()
            && self.fields.iter().all(|f| *f == FieldDraft::default())
    }

    /// The name as `emd` will STORE it, for the "stored as" hint --
    /// `Some` only for a kind that is PascalCase-converted and only once
    /// the typed name is valid, so the hint never previews a name that
    /// cannot be submitted.
    pub fn stored_as(&self) -> Option<String> {
        if !self.kind.pascal_cased() || !valid_item_name(&self.name) {
            return None;
        }
        let preview = to_pascal_case_preview(&self.name);
        (!preview.is_empty()).then_some(preview)
    }

    /// The single inline error blocking submission, or `None` when the
    /// draft is submittable. Checked in the order the form is laid out, so
    /// the message always points at the topmost problem.
    ///
    /// This is the whole "never shell out with a name the CLI will reject"
    /// gate: [`Self::args`] is only ever called after this returns `None`.
    /// Each rule below has an `emd` counterpart that would otherwise fail
    /// the run with a worse message:
    /// `identifier '...' must start with [a-z_]` for the two name rules
    /// and `invalid field kind "..."` for the field rule (both verified
    /// against `emd 0.2.0`).
    pub fn error(&self) -> Option<String> {
        if !valid_item_name(&self.name) || (self.kind.pascal_cased() && self.stored_as().is_none())
        {
            return Some(format!(
                "{} names must be snake_case, e.g. {}.",
                self.kind.noun(),
                self.kind.example()
            ));
        }
        if self.kind.takes_module() && !self.module.is_empty() && !valid_item_name(&self.module) {
            return Some(
                "Module names must be snake_case, or blank for the shared module.".to_string(),
            );
        }
        if self.kind.takes_fields() {
            for (ix, field) in self.fields.iter().enumerate() {
                if !valid_field_spec(&field.name, &field.spec()) {
                    return Some(format!(
                        "Field {}: a snake_case name and a kind of {}.",
                        ix + 1,
                        "int, fixed, bool, str, vec2 or asset:<ext>"
                    ));
                }
            }
        }
        None
    }

    /// The full `emd` argv for this draft -- everything after the binary,
    /// except the `--json` flag [`crate::runner::EmdRequest`] appends.
    ///
    /// **Only valid to call when [`Self::error`] is `None`.**
    ///
    /// Component/system/schedule delegate to worldlib's builders verbatim.
    /// Resource has no builder of its own (S1 extracted only the three
    /// ggo-ide had tabs for) so it is assembled here -- but its
    /// `--module` half still goes through [`gen_module_args`], so the
    /// "blank module omits the flag" convention stays single-sourced.
    /// Module and world take neither a module nor fields.
    pub fn args(&self) -> Vec<String> {
        let fields: Vec<FieldEntry> = self.fields.iter().map(FieldDraft::entry).collect();
        match self.kind {
            GenKind::Component => build_generate_component_args(&self.name, &self.module, &fields),
            GenKind::System => build_generate_system_args(&self.name, &self.module),
            GenKind::Schedule => build_generate_schedule_args(&self.name, &self.module),
            GenKind::Resource => {
                let mut args = vec![
                    "generate".to_string(),
                    GenKind::Resource.subcommand().to_string(),
                    self.name.clone(),
                ];
                args.extend(gen_module_args(&self.module));
                args
            }
            GenKind::Module | GenKind::World => vec![
                "generate".to_string(),
                self.kind.subcommand().to_string(),
                self.name.clone(),
            ],
        }
    }
}

/// The `emd` argv for an inline "New World…" commit: `generate world
/// <name>`, plus `--dir <sub>` when the target sits below
/// `assets/worlds/`. Only valid to call after [`world_name_error`]
/// returned `None` for the typed input the pieces came from.
pub fn build_generate_world_args(name: &str, dir: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "generate".to_string(),
        "world".to_string(),
        name.to_string(),
    ];
    if let Some(dir) = dir {
        args.push("--dir".to_string());
        args.push(dir.to_string());
    }
    args
}

/// The inline "New World…" name gate: `/`-separated snake_case segments
/// (the last is the file's name, the rest become `--dir` levels), each
/// held to [`valid_item_name`] -- the exact per-segment rule `emd
/// generate world --dir` applies, so a name that passes here is never
/// rejected by the CLI.
pub fn world_name_error(typed: &str) -> Option<String> {
    if typed.split('/').any(|segment| !valid_item_name(segment)) {
        return Some(
            "World names must be snake_case segments, e.g. arena or dungeon/arena.".to_string(),
        );
    }
    None
}

/// A typed inline world name split into (`--dir` levels, file name) --
/// `"dungeon/arena"` is (`["dungeon"]`, `"arena"`). Only valid after
/// [`world_name_error`] returned `None`.
pub fn split_world_name(typed: &str) -> (Vec<&str>, &str) {
    let mut segments: Vec<&str> = typed.split('/').collect();
    let name = segments.pop().unwrap_or(typed);
    (segments, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(kind: GenKind, name: &str) -> GenDraft {
        GenDraft {
            name: name.to_string(),
            ..GenDraft::new(kind)
        }
    }

    /// The argv every form produces must be byte-for-byte what S1's
    /// builders produce -- this is the contract that keeps the fork and
    /// ggo-ide issuing the same commands.
    #[test]
    fn args_match_worldlibs_builders_for_every_kind() {
        let mut component = draft(GenKind::Component, "hero_unit");
        component.module = "gameplay".to_string();
        component.fields = vec![
            FieldDraft {
                name: "hp".into(),
                kind: "int".into(),
                ext: String::new(),
            },
            FieldDraft {
                name: "art".into(),
                kind: ASSET_KIND.into(),
                ext: "png".into(),
            },
        ];
        assert_eq!(
            component.args(),
            build_generate_component_args(
                "hero_unit",
                "gameplay",
                &[
                    FieldEntry {
                        name: "hp".into(),
                        kind: "int".into()
                    },
                    FieldEntry {
                        name: "art".into(),
                        kind: "asset:png".into()
                    },
                ]
            )
        );
        assert_eq!(
            component.args(),
            [
                "generate",
                "component",
                "hero_unit",
                "--module",
                "gameplay",
                "--field",
                "hp:int",
                "--field",
                "art:asset:png",
            ]
        );

        let system = draft(GenKind::System, "spawn_enemies");
        assert_eq!(
            system.args(),
            build_generate_system_args("spawn_enemies", "")
        );
        assert_eq!(system.args(), ["generate", "system", "spawn_enemies"]);

        let mut schedule = draft(GenKind::Schedule, "tick");
        schedule.module = "gameplay".to_string();
        assert_eq!(
            schedule.args(),
            build_generate_schedule_args("tick", "gameplay")
        );

        let mut resource = draft(GenKind::Resource, "game_config");
        assert_eq!(
            resource.args(),
            ["generate", "resource", "game_config"],
            "a blank module omits --module entirely (gen_module_args)"
        );
        resource.module = "gameplay".to_string();
        assert_eq!(
            resource.args(),
            [
                "generate",
                "resource",
                "game_config",
                "--module",
                "gameplay"
            ]
        );

        assert_eq!(
            draft(GenKind::Module, "gameplay").args(),
            ["generate", "module", "gameplay"]
        );
        assert_eq!(
            draft(GenKind::World, "arena").args(),
            ["generate", "world", "arena"]
        );
    }

    /// A module/world form never emits `--module`, even if a stale module
    /// string is sitting in the draft (the panel keeps one set of editors
    /// across a kind switch).
    #[test]
    fn module_and_world_never_pass_a_module_flag() {
        for kind in [GenKind::Module, GenKind::World] {
            let mut d = draft(kind, "thing");
            d.module = "gameplay".to_string();
            assert!(!d.args().contains(&"--module".to_string()));
            assert_eq!(d.error(), None, "a stale module must not block submission");
        }
    }

    /// Component fields are ignored by every other kind, so switching a
    /// half-filled component form to System can't smuggle `--field` in.
    #[test]
    fn only_component_emits_fields() {
        let mut d = draft(GenKind::System, "spawn_enemies");
        d.fields = vec![FieldDraft {
            name: "hp".into(),
            kind: "int".into(),
            ext: String::new(),
        }];
        assert_eq!(d.args(), ["generate", "system", "spawn_enemies"]);
    }

    #[test]
    fn a_name_that_emd_would_reject_is_an_inline_error() {
        for bad in ["", "Bad-Name", "HeroUnit", "9lives", "hero unit", "_"] {
            let d = draft(GenKind::Component, bad);
            assert!(
                d.error().is_some(),
                "{bad:?} must not be submittable as a component"
            );
        }
        // `_` is `valid_item_name`-true but PascalCases to nothing, so it
        // is rejected for a component (ggo-ide's own extra gate) and
        // accepted for the kinds that store the name verbatim.
        assert!(draft(GenKind::Component, "_").error().is_some());
        assert!(draft(GenKind::System, "_").error().is_none());
        assert!(draft(GenKind::Component, "hero_unit").error().is_none());
    }

    #[test]
    fn a_bad_module_is_an_inline_error_but_a_blank_one_is_the_shared_module() {
        let mut d = draft(GenKind::Component, "hero_unit");
        d.module = "Bad-Mod".to_string();
        assert!(d.error().is_some());
        d.module = "gameplay".to_string();
        assert_eq!(d.error(), None);
        d.module = String::new();
        assert_eq!(d.error(), None);
    }

    #[test]
    fn a_bad_field_is_an_inline_error_naming_its_row() {
        let mut d = draft(GenKind::Component, "hero_unit");
        d.fields = vec![
            FieldDraft {
                name: "hp".into(),
                kind: "int".into(),
                ext: String::new(),
            },
            FieldDraft {
                name: "art".into(),
                kind: ASSET_KIND.into(),
                ext: String::new(), // asset with no extension
            },
        ];
        assert!(d.error().unwrap().starts_with("Field 2:"));
        d.fields[1].ext = "png".into();
        assert_eq!(d.error(), None);
        d.fields[1].name = "Bad Name".into();
        assert!(d.error().unwrap().starts_with("Field 2:"));
    }

    /// A brand-new field row (name empty) blocks submission -- an empty
    /// `--field :int` is not something to hand to `emd`.
    #[test]
    fn an_empty_field_row_blocks_submission() {
        let mut d = draft(GenKind::Component, "hero_unit");
        d.fields = vec![FieldDraft::default()];
        assert!(d.error().is_some());
    }

    #[test]
    fn stored_as_previews_only_a_valid_pascal_cased_name() {
        assert_eq!(
            draft(GenKind::Component, "hero_unit")
                .stored_as()
                .as_deref(),
            Some("HeroUnit")
        );
        assert_eq!(draft(GenKind::Component, "Bad-Name").stored_as(), None);
        assert_eq!(draft(GenKind::Component, "_").stored_as(), None);
        assert_eq!(
            draft(GenKind::System, "spawn_enemies").stored_as(),
            None,
            "only components are PascalCase-converted"
        );
    }

    #[test]
    fn pristine_is_only_a_completely_untouched_draft() {
        let mut d = GenDraft::new(GenKind::Component);
        assert!(d.pristine());
        d.fields.push(FieldDraft::default());
        assert!(d.pristine(), "an empty added row is still untouched");
        d.fields[0].name = "hp".into();
        assert!(!d.pristine());
    }

    #[test]
    fn kind_shape_matches_emd_generates_own_subcommand_surface() {
        assert!(GenKind::ALL.iter().all(|k| !k.subcommand().is_empty()));
        assert!(GenKind::Component.takes_fields());
        assert!(GenKind::ALL.iter().filter(|k| k.takes_fields()).count() == 1);
        assert!(!GenKind::Module.takes_module());
        assert!(!GenKind::World.takes_module());
        assert!(GenKind::Component.takes_module());
        assert!(GenKind::Resource.takes_module());
        assert!(GenKind::Schedule.takes_module());
        assert!(GenKind::System.takes_module());
    }

    // ------------------------------------------- inline world name rules

    /// The inline argv matches the form's argv for a plain name, and only
    /// `--dir` distinguishes a subdir target.
    #[test]
    fn inline_world_args_match_the_form_and_add_only_dir() {
        assert_eq!(
            build_generate_world_args("arena", None),
            draft(GenKind::World, "arena").args()
        );
        assert_eq!(
            build_generate_world_args("arena", Some("dungeon/floors")),
            vec!["generate", "world", "arena", "--dir", "dungeon/floors"]
        );
    }

    #[test]
    fn world_name_error_accepts_snake_case_segments() {
        for ok in &["arena", "_x", "level_2", "dungeon/arena", "a/b/c_1"] {
            assert!(world_name_error(ok).is_none(), "{ok} should pass");
        }
    }

    /// Every rejected shape is one `emd generate world --dir` would also
    /// reject: empty segments (leading/trailing/double slash), traversal,
    /// non-snake_case, separators emd never sees.
    #[test]
    fn world_name_error_rejects_what_emd_would() {
        for bad in &[
            "",
            "Arena",
            "dungeon/Arena",
            "a b",
            "a.b",
            "/arena",
            "arena/",
            "a//b",
            "..",
            "../a",
            "a/../b",
            "a\\b",
        ] {
            assert!(world_name_error(bad).is_some(), "{bad:?} should fail");
        }
    }

    #[test]
    fn split_world_name_separates_dir_levels_from_the_file() {
        assert_eq!(split_world_name("arena"), (vec![], "arena"));
        assert_eq!(
            split_world_name("dungeon/arena"),
            (vec!["dungeon"], "arena")
        );
        assert_eq!(split_world_name("a/b/c"), (vec!["a", "b"], "c"));
    }
}
