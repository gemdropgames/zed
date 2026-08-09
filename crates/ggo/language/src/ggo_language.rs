//! The "GGO World" language: native (non-extension) Zed registration of the
//! TOML dialect that `ggo-worldlib` reads and writes -- `[[entity]]`,
//! `[[instance]]` and `[[background]]` scene files under a project's
//! `worlds/` tree.
//!
//! # Why this is native and not an extension
//!
//! Upstream extracted TOML into an extension in `d074586fbf`, so the fork has
//! no built-in TOML language and no vendored TOML grammar. Going through the
//! extension API would mean a WASM grammar downloaded at runtime and a
//! separately versioned artifact; this crate instead does exactly what
//! `crates/languages/src/lib.rs` does for a built-in, with the same two
//! registry calls in the same order:
//!
//! - `LanguageRegistry::register_native_grammars` (`crates/languages/src/lib.rs:59`)
//!   makes a compiled-in `tree_sitter::Language` available under a name;
//! - `LanguageRegistry::register_language` (`crates/languages/src/lib.rs:359`,
//!   inside `register_language`) records the config/matcher eagerly and defers
//!   the `LoadedLanguage` (config + queries) to a closure the registry calls on
//!   first use.
//!
//! The one deliberate divergence from that file is where the config and query
//! text come from. Upstream reads them out of `crates/grammars`, whose
//! `rust-embed` `GrammarDir` is rooted at `crates/grammars/src/` -- putting
//! this language's files there would mean adding upstream files, which the
//! fork's marked-lines-only rule forbids. So the three files live next to this
//! module and are `include_str!`'d; `load_config` still parses `config.toml`
//! into a `LanguageConfig` exactly as `grammars::load_config` does.
//!
//! # File matching
//!
//! There is no `path_suffixes` in `config.toml` -- see the comment there.
//! Which files are GGO worlds is a per-project decision, made by the
//! `file_types` key in the repo's committed `.zed/settings.json`.

use std::borrow::Cow;
use std::sync::Arc;

use language::{LanguageConfig, LanguageQueries, LanguageRegistry, LoadedLanguage};

/// The name the language is registered under. `.zed/settings.json`'s
/// `file_types` key must spell it exactly this way -- `find_for_file` looks the
/// glob set up by language name.
pub const LANGUAGE_NAME: &str = "GGO World";

/// The glob a GGO project's committed `.zed/settings.json` maps to
/// [`LANGUAGE_NAME`] under its `file_types` key. Declared here so the fork can
/// test the exact string the repo ships rather than a paraphrase of it.
///
/// The leading `**/` is load-bearing: `AvailableLanguages::find_for_file` tests
/// the globs against `File::full_path`, which is prefixed with the worktree's
/// root directory name, so `worlds/**/*.toml` would never match. There is a
/// test pinning that.
pub const PROJECT_FILE_TYPE_GLOB: &str = "**/worlds/**/*.toml";

/// The name the grammar is registered under, and the value of `grammar` in
/// `config.toml`. Namespaced so it cannot collide with a TOML grammar an
/// extension registers.
const GRAMMAR_NAME: &str = "ggo-world";

const CONFIG_TOML: &str = include_str!("ggo_world/config.toml");
const HIGHLIGHTS_SCM: &str = include_str!("ggo_world/highlights.scm");
const OUTLINE_SCM: &str = include_str!("ggo_world/outline.scm");

/// Registers the grammar and the language with the shared registry.
///
/// Call once at startup, after `languages::init` has registered the built-ins
/// (order is not actually load-bearing -- the grammar name is ours alone -- but
/// keeping the two adjacent keeps the startup sequence readable).
pub fn init(languages: &LanguageRegistry) {
    languages.register_native_grammars([(GRAMMAR_NAME, tree_sitter_toml_ng::LANGUAGE)]);

    let config = load_config();
    let loaded_config = config.clone();
    languages.register_language(
        config.name.clone(),
        config.grammar.clone(),
        config.matcher.clone(),
        config.hidden,
        None,
        Arc::new(move || {
            Ok(LoadedLanguage {
                config: loaded_config.clone(),
                queries: queries(),
                context_provider: None,
                toolchain_provider: None,
                manifest_name: None,
            })
        }),
    );
}

/// Parses the embedded `config.toml`, mirroring `grammars::load_config`
/// (`crates/grammars/src/grammars.rs`), panic and all: the input is a
/// compile-time constant, so a failure here is a build-time authoring bug that
/// no run-time recovery could improve on.
fn load_config() -> LanguageConfig {
    let config: LanguageConfig =
        toml::from_str(CONFIG_TOML).expect("failed to parse the embedded GGO World config.toml");
    assert_eq!(
        config.grammar.as_deref(),
        Some(GRAMMAR_NAME),
        "config.toml's `grammar` must name the grammar `init` registers"
    );
    config
}

/// The query set, mirroring `grammars::load_queries` for a language directory
/// holding exactly `highlights.scm` and `outline.scm`.
fn queries() -> LanguageQueries {
    LanguageQueries {
        highlights: Some(Cow::Borrowed(HIGHLIGHTS_SCM)),
        outline: Some(Cow::Borrowed(OUTLINE_SCM)),
        ..LanguageQueries::default()
    }
}

#[cfg(test)]
mod tests;
