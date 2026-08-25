use std::sync::Arc;

use gpui::{App, AppContext as _, BorrowAppContext as _, TestAppContext};
use language::{Buffer, File, LanguageAwareStyling, LanguageRegistry, TestFile};
use settings::{AllLanguageSettingsContent, SettingsStore};
use syntax_theme::SyntaxTheme;
use util::rel_path::rel_path;

use super::*;

/// A GGO world scene carrying one of every block kind, lifted verbatim from
/// `ggo-worldlib`'s own round-trip fixture
/// (`tools/ggo-worldlib/src/world_file.rs`, `read_world` tests) so the outline
/// is exercised against the shape the writer actually emits -- including the
/// plain `[layers]` table and the stray comment that the normalized write
/// drops.
const WORLD_FIXTURE: &str = r#"
# a stray comment -- must not survive the normalized write
[layers]
bg = "maps/ground"

[[entity]]
Transform = { pos = [3, 4], z = 2 }
Enemy = { hp = 5, speed = 1.5 }

[[entity]]
Transform = { pos = [10, -2], z = 0 }

[[instance]]
world = "worlds/nested/arena"
pos = [64, 128]

[[background]]
layer = 1
map = "maps/mid.map"
"#;

fn registry(cx: &mut App) -> Arc<LanguageRegistry> {
    let languages = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
    init(&languages);
    languages
}

/// Mirrors `language::buffer_tests::init_settings`: a test settings store with
/// the `file_types` map a project's `.zed/settings.json` would contribute.
fn init_settings(cx: &mut App, f: impl FnOnce(&mut AllLanguageSettingsContent)) {
    let settings_store = SettingsStore::test(cx);
    cx.set_global(settings_store);
    cx.update_global::<SettingsStore, _>(|settings, cx| {
        settings.update_user_settings(cx, |content| f(&mut content.project.all_languages));
    });
}

fn test_file(path: &str) -> Arc<dyn File> {
    Arc::new(TestFile {
        path: Arc::from(rel_path(path)),
        // `File::full_path` is `<root_name>/<rel path>`, and that is what
        // `language_for_file` matches the `file_types` globs against. The
        // worktree root name being on the front is the whole reason the glob
        // needs its leading `**/`.
        root_name: "ggo".into(),
        local_root: None,
    })
}

// ---------------------------------------------------------------- the config

#[test]
fn the_embedded_config_describes_the_ggo_world_language() {
    let config = load_config();
    assert_eq!(config.name.as_ref(), LANGUAGE_NAME);
    assert_eq!(config.grammar.as_deref(), Some(GRAMMAR_NAME));
    assert!(!config.hidden, "the language must be user-selectable");
    assert_eq!(
        config
            .line_comments
            .iter()
            .map(|c| &**c)
            .collect::<Vec<_>>(),
        vec!["# "],
    );
}

#[test]
fn the_config_claims_no_path_suffixes_of_its_own() {
    // If this ever gains `path_suffixes = ["toml"]`, every TOML file in every
    // project becomes a GGO world. Scoping is `.zed/settings.json`'s job.
    assert!(
        load_config().matcher.path_suffixes.is_empty(),
        "matching is delegated to the project's `file_types` setting",
    );
}

#[test]
fn the_query_set_is_highlights_plus_outline() {
    let queries = queries();
    assert!(queries.highlights.is_some());
    assert!(queries.outline.is_some());
    assert!(queries.brackets.is_none());
    assert!(queries.indents.is_none());
    assert!(queries.injections.is_none());
    assert!(queries.overrides.is_none());
    assert!(queries.runnables.is_none());
    assert!(queries.text_objects.is_none());
}

// ---------------------------------------------------------- the registration

#[gpui::test]
fn init_registers_the_grammar_and_the_language(cx: &mut App) {
    let languages = registry(cx);

    assert!(
        languages
            .grammar_names()
            .iter()
            .any(|name| &**name == GRAMMAR_NAME),
        "grammar names: {:?}",
        languages.grammar_names(),
    );
    assert!(
        languages
            .language_names()
            .iter()
            .any(|name| name.as_ref() == LANGUAGE_NAME),
        "language names: {:?}",
        languages.language_names(),
    );
}

#[gpui::test]
fn init_is_idempotent(cx: &mut App) {
    // `register_language` early-returns when the name is already present, so a
    // double `init` must not produce two "GGO World" entries in the language
    // selector.
    let languages = registry(cx);
    init(&languages);

    let count = languages
        .language_names()
        .iter()
        .filter(|name| name.as_ref() == LANGUAGE_NAME)
        .count();
    assert_eq!(count, 1);
}

#[gpui::test]
async fn the_language_loads_by_name_with_its_grammar_and_queries(cx: &mut TestAppContext) {
    let languages = cx.update(registry);

    // This resolving at all is the real assertion: the registry's loader runs
    // `Language::new(config, grammar).with_queries(queries)`, so a bad grammar
    // name, an unparseable `config.toml`, or a query that does not compile
    // against the TOML grammar all surface here as an `Err`.
    let language = languages
        .language_for_name(LANGUAGE_NAME)
        .await
        .expect("GGO World failed to load");

    assert_eq!(language.name().as_ref(), LANGUAGE_NAME);
    assert!(
        language.grammar().is_some(),
        "the native grammar did not attach",
    );
}

#[gpui::test]
async fn an_unregistered_name_still_does_not_resolve(cx: &mut TestAppContext) {
    // Guards the assertion above against being vacuously true.
    let languages = cx.update(registry);
    assert!(languages.language_for_name("GGO Worlds").await.is_err());
}

// -------------------------------------------------------------- the outline

/// The outline as Zed's outline panel would render it: `(text, depth)` per row,
/// mirroring `language::buffer_tests::test_outline`.
async fn outline_rows(cx: &mut TestAppContext, text: &str) -> Vec<(String, usize)> {
    let languages = cx.update(registry);
    let language = languages.language_for_name(LANGUAGE_NAME).await.unwrap();

    let buffer = cx.new(|cx| {
        let buffer = Buffer::local(text, cx);
        buffer.with_language(language, cx)
    });
    let snapshot = buffer.update(cx, |buffer, _| buffer.snapshot());
    let outline = snapshot.outline(None);

    outline
        .items
        .iter()
        .map(|item| (item.text.to_string(), item.depth))
        .collect()
}

#[gpui::test]
async fn the_outline_surfaces_every_array_of_table_header(cx: &mut TestAppContext) {
    let rows = outline_rows(cx, WORLD_FIXTURE).await;

    let headers = rows
        .iter()
        .filter(|(text, _)| text.starts_with("[["))
        .map(|(text, depth)| (text.as_str(), *depth))
        .collect::<Vec<_>>();

    assert_eq!(
        headers,
        vec![
            ("[[entity]]", 0),
            ("[[entity]]", 0),
            ("[[instance]]", 0),
            ("[[background]]", 0),
        ],
        "full outline was {rows:?}",
    );
}

#[gpui::test]
async fn the_outline_nests_a_block_s_keys_under_its_header(cx: &mut TestAppContext) {
    let rows = outline_rows(cx, WORLD_FIXTURE).await;
    let rows = rows
        .iter()
        .map(|(text, depth)| (text.as_str(), *depth))
        .collect::<Vec<_>>();

    assert_eq!(
        rows,
        vec![
            ("[layers]", 0),
            ("bg", 1),
            // An entity's rows are its component names -- the thing worth
            // navigating to. `Transform = { pos = [3, 4], z = 2 }` is ONE row,
            // not three: inline-table members are `pair` nodes too, and the
            // outline query excludes them by requiring the parent to be a
            // header. Without that, every component field would show up here.
            ("[[entity]]", 0),
            ("Transform", 1),
            ("Enemy", 1),
            ("[[entity]]", 0),
            ("Transform", 1),
            ("[[instance]]", 0),
            ("world", 1),
            ("pos", 1),
            ("[[background]]", 0),
            ("layer", 1),
            ("map", 1),
        ],
    );
}

#[gpui::test]
async fn the_outline_handles_dotted_and_quoted_header_keys(cx: &mut TestAppContext) {
    let rows = outline_rows(
        cx,
        concat!(
            "[[world.entity]]\n",
            "Transform = { pos = [0, 0] }\n",
            "\n",
            "[\"odd key\"]\n",
            "n = 1\n",
        ),
    )
    .await;

    assert_eq!(
        rows.iter()
            .map(|(text, depth)| (text.as_str(), *depth))
            .collect::<Vec<_>>(),
        vec![
            ("[[world.entity]]", 0),
            ("Transform", 1),
            ("[\"odd key\"]", 0),
            ("n", 1),
        ],
    );
}

#[gpui::test]
async fn a_world_file_with_no_blocks_has_an_empty_outline(cx: &mut TestAppContext) {
    assert!(outline_rows(cx, "# nothing here yet\n").await.is_empty());
}

// ------------------------------------------------------------- highlighting

#[gpui::test]
async fn the_vendored_highlights_capture_a_world_files_keys_and_values(cx: &mut TestAppContext) {
    // The load test already proves the query COMPILES against this grammar (a
    // query naming a node type the grammar does not have is a hard error). What
    // it cannot prove is that the node types it names ever actually OCCUR --
    // highlights.scm was vendored from upstream's pre-extraction TOML language
    // dir (`d074586fbf~1`), which targeted a different TOML grammar of the same
    // lineage. This pins that the two really do agree.
    let languages = cx.update(registry);
    let language = languages.language_for_name(LANGUAGE_NAME).await.unwrap();

    // Highlight ids are indices into the active syntax theme, so without one
    // every chunk comes back unhighlighted (`HighlightMap::default()` is empty
    // and `get` returns `None` for every capture). Same `SyntaxTheme::new` +
    // `Language::set_theme` setup `language::buffer_tests` uses around
    // `crates/language/src/buffer_tests.rs:3418`.
    let theme = SyntaxTheme::new_test([
        ("property", gpui::rgba(0x00000001).into()),
        ("comment", gpui::rgba(0x00000002).into()),
        ("string", gpui::rgba(0x00000003).into()),
        ("number", gpui::rgba(0x00000004).into()),
        ("constant", gpui::rgba(0x00000005).into()),
        ("operator", gpui::rgba(0x00000006).into()),
        ("punctuation.bracket", gpui::rgba(0x00000007).into()),
        ("punctuation.delimiter", gpui::rgba(0x00000008).into()),
        ("string.special", gpui::rgba(0x00000009).into()),
    ]);
    language.set_theme(&theme);

    let buffer = cx.new(|cx| {
        let buffer = Buffer::local(WORLD_FIXTURE, cx);
        buffer.with_language(language, cx)
    });
    let snapshot = buffer.update(cx, |buffer, _| buffer.snapshot());

    let highlighted = snapshot
        .chunks(
            0..snapshot.len(),
            LanguageAwareStyling {
                tree_sitter: true,
                diagnostics: false,
            },
        )
        .filter_map(|chunk| {
            let id = chunk.syntax_highlight_id?;
            Some((
                chunk.text.to_string(),
                theme.get_capture_name(id)?.to_string(),
            ))
        })
        .collect::<Vec<_>>();

    for (text, capture) in [
        (
            "# a stray comment -- must not survive the normalized write",
            "comment",
        ),
        ("layers", "property"),         // (bare_key)
        ("\"maps/ground\"", "string"),  // (string)
        ("=", "operator"),              // "=" @operator
        ("[[", "punctuation.bracket"),  // the array-of-table header token
        ("3", "number"),                // (integer)
        ("1.5", "number"),              // (float)
        (",", "punctuation.delimiter"), // "," @punctuation.delimiter
    ] {
        assert!(
            highlighted.iter().any(|(t, c)| t == text && c == capture),
            "expected {text:?} highlighted as {capture:?}; got {highlighted:?}",
        );
    }
}

// -------------------------------------------------------- the project glob

#[gpui::test]
fn a_worlds_toml_matches_through_the_projects_file_types(cx: &mut App) {
    init_settings(cx, |settings| {
        settings.file_types.get_or_insert_default().0.insert(
            LANGUAGE_NAME.into(),
            vec![PROJECT_FILE_TYPE_GLOB.into()].into(),
        );
    });
    let languages = registry(cx);
    let name = |file| {
        languages
            .language_for_file(&file, None, cx)
            .and_then(|id| languages.language_name_for_id(id))
    };

    for path in [
        "worlds/arena.toml",
        "worlds/nested/arena.toml",
        "assets/worlds/deep/nested/arena.toml",
    ] {
        assert_eq!(
            name(test_file(path)).as_ref().map(|n| n.as_ref()),
            Some(LANGUAGE_NAME),
            "{path} should be a GGO world",
        );
    }

    for path in ["Cargo.toml", "assets/sprites/hero.toml", "worlds/README.md"] {
        assert_eq!(name(test_file(path)), None, "{path} should not match");
    }
}

/// With the glob shipped in the fork's default settings, a world file
/// activates the language in ANY project -- no per-repo
/// `.zed/settings.json` -- while ordinary TOML stays TOML.
#[gpui::test]
fn the_default_settings_activate_the_language_without_a_project_setting(cx: &mut App) {
    init_settings(cx, |_| {});
    let languages = registry(cx);
    let name = |file| {
        languages
            .language_for_file(&file, None, cx)
            .and_then(|id| languages.language_name_for_id(id))
    };

    for path in ["worlds/arena.toml", "assets/worlds/nested/boss.toml"] {
        assert_eq!(
            name(test_file(path)).map(|n| n.to_string()),
            Some(LANGUAGE_NAME.to_string()),
            "{path} should be a GGO World"
        );
    }
    for path in ["Cargo.toml", "assets/sprites/hero.toml", "worlds/README.md"] {
        assert_eq!(name(test_file(path)), None, "{path} should not match");
    }
}

/// The shipped glob must start with `**/`: `find_for_file` matches
/// against `File::full_path`, which is prefixed with the worktree's root
/// name ("ggo/worlds/arena.toml"), so an unanchored `worlds/**/*.toml`
/// never matches. Pinned on the constant since the default settings now
/// carry it everywhere.
#[test]
fn the_project_glob_is_anchored_with_a_leading_globstar() {
    assert!(PROJECT_FILE_TYPE_GLOB.starts_with("**/"), "{PROJECT_FILE_TYPE_GLOB}");
}

/// The fork ships the project glob in its DEFAULT settings, so the
/// language activates in every project without a per-repo
/// `.zed/settings.json` (the config-placement gap MIGRATION.md §11 named).
/// An upstream merge that drops the `// GGO` line in
/// `assets/settings/default.json` fails here.
#[test]
fn the_default_settings_ship_the_project_glob() {
    let defaults = settings::default_settings();
    let value: serde_json::Value =
        settings_json::parse_json_with_comments(&defaults).expect("default.json parses");
    let globs = value["file_types"][LANGUAGE_NAME]
        .as_array()
        .expect("GGO World has a default file_types entry");
    assert_eq!(globs, &vec![serde_json::json!(PROJECT_FILE_TYPE_GLOB)]);
}
