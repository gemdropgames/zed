//! Running `emd` as a child process, and the seam that lets tests do it
//! without one.
//!
//! Everything that decides WHAT to run lives in `forms` (and, below that,
//! in `ggo_worldlib::emerald`'s argv builders). This module only decides
//! HOW: which binary, which working directory, and how a finished process
//! becomes the [`EmdRunOutcome`] worldlib's own result helpers understand.
//!
//! Mirrors the INVOCATION ggo-ide's `backend/emerald.rs` makes -- same
//! binary resolution convention, same `--json` trailer contract, same
//! "the transcript is stdout AND stderr, not just one half" rule -- and
//! deliberately none of its plumbing: no `Db`-backed settings, no
//! single-active-slot `EmdRunner` manager, no streaming line callback.
//! A `generate` is one short one-shot call whose only output that matters
//! is its trailer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smol::process::Command;

use ggo_worldlib::emerald::{EmdRunOutcome, emd_run_outcome};

/// Env var naming a non-default `emd` binary.
///
/// Same convention as `.zed/tasks.json`'s `GGO_*` variables (which spawn
/// `emd` by bare name against `PATH`, with every per-user value read from
/// the environment at spawn time) and the same *variable* ggo-ide's
/// `pages::emerald` reads (`EMD_BIN_ENV`). Deliberately NOT ggo-ide's
/// `emd_path` DB setting: that one exists because ggo-ide has a settings
/// page writing `~/.ggo/ggo_ide.db`, and this fork has no such surface --
/// an env var is the whole story here, and it is documented as such.
pub const EMD_BIN_ENV: &str = "GGO_EMD";

/// Bare-name fallback for the `emd` binary, resolved against `PATH` --
/// what `.zed/tasks.json` and ggo-ide both default to.
pub const DEFAULT_EMD_BIN: &str = "emd";

/// `emd`'s machine-readable-output flag. Implies `--quiet`, and is what
/// makes the `emd-json:` trailer
/// ([`ggo_worldlib::emerald::EMD_JSON_PREFIX`]) appear at all, so every
/// request this module builds carries it.
pub const JSON_FLAG: &str = "--json";

/// The `emd` binary to spawn: [`EMD_BIN_ENV`] when set and non-blank, else
/// [`DEFAULT_EMD_BIN`].
pub fn emd_bin() -> String {
    resolve_emd_bin(std::env::var(EMD_BIN_ENV).ok())
}

/// [`emd_bin`]'s rule, with the environment read passed in -- so it can be
/// tested without mutating a process-global the rest of this crate's tests
/// share. Blank is treated as unset (same filter ggo-ide's
/// `resolve_emd_bin` applies to its stored setting), so an
/// accidentally-empty export doesn't turn into a spawn of `""`.
fn resolve_emd_bin(configured: Option<String>) -> String {
    configured
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EMD_BIN.to_string())
}

/// One `emd` invocation, fully resolved: which binary, in which project
/// directory, with which argv.
///
/// Built on the UI thread (so the env read and the project-root walk
/// happen where the panel can still report a problem) and moved into the
/// background task, which is why it owns everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmdRequest {
    pub bin: String,
    /// `emd`'s working directory -- the emerald project root (the nearest
    /// ancestor holding `emerald.toml`). `emd` discovers the project from
    /// its cwd, so this is what decides which project is written to.
    pub project_dir: PathBuf,
    pub args: Vec<String>,
}

impl EmdRequest {
    /// A request for `args` (as built by `ggo_worldlib::emerald`'s argv
    /// builders) in `project_dir`, with [`JSON_FLAG`] appended.
    ///
    /// The flag is appended HERE rather than in the argv builders because
    /// it is a property of how this host consumes the run (it wants the
    /// trailer), not of the command being run -- the builders are shared
    /// with ggo-ide, whose streaming console wants the same flag for the
    /// same reason but adds it at its own call sites.
    pub fn new(project_dir: impl Into<PathBuf>, args: Vec<String>) -> Self {
        let mut args = args;
        if !args.iter().any(|a| a == JSON_FLAG) {
            args.push(JSON_FLAG.to_string());
        }
        Self {
            bin: emd_bin(),
            project_dir: project_dir.into(),
            args,
        }
    }

    /// The invocation as a human would type it -- what the panel shows
    /// while a run is in flight, and what a spawn failure is reported
    /// against.
    pub fn command_line(&self) -> String {
        let mut line = self.bin.clone();
        for arg in &self.args {
            line.push(' ');
            // The one arg that can legitimately be empty is `--module ""`
            // (`mutation_module_args`' "the shared bucket, precisely"),
            // which would otherwise render as a trailing space.
            if arg.is_empty() {
                line.push_str("\"\"");
            } else {
                line.push_str(arg);
            }
        }
        line
    }
}

/// The injection seam: anything that can turn an [`EmdRequest`] into an
/// [`EmdRunOutcome`].
///
/// A boxed `Fn` rather than a trait because every implementation is a
/// single function -- the real one below, and a recording fake in tests --
/// and `Arc` because the panel holds one and each spawned run needs its
/// own handle. `Send + Sync` because the call happens on
/// `cx.background_spawn`'s thread, never on the UI thread.
pub type EmdRunner = Arc<dyn Fn(EmdRequest) -> EmdRunOutcome + Send + Sync>;

/// The production runner: really spawn `emd`.
pub fn system_runner() -> EmdRunner {
    Arc::new(|request| run_emd(&request))
}

/// Spawn `emd` and wait for it. **Blocking** -- callers run this on
/// `cx.background_spawn`, never on the UI thread.
///
/// The captured transcript is stdout followed by stderr, and that order is
/// load-bearing: `emd` prints its `emd-json:` trailer to **stdout on
/// success and stderr on failure** (verified against `emd 0.2.0`), and
/// [`ggo_worldlib::emerald::parse_emd_trailer`] scans for the LAST trailer
/// line, so stderr-last is what makes a failure's trailer -- the one
/// carrying `error` -- the one that wins.
///
/// A spawn failure (no `emd` on `PATH`, a bad [`EMD_BIN_ENV`]) is reported
/// as a non-ok outcome naming the command, not as a panic or a silent
/// no-op: "emd isn't installed" is the single most likely first-run
/// failure and it has to reach the panel as text.
///
/// No timeout, deliberately: `emd generate` writes files and updates a
/// manifest, with no compile step (unlike `emd component field rm`, whose
/// `cargo check` is what forced ggo-ide's generous `EMD_TIMEOUT`). A
/// mutation that CAN compile arrives with F5.3's manifest ops; the timeout
/// belongs with it.
///
/// The child is `smol::process::Command`, not `std::process::Command`:
/// this checkout's `clippy.toml` disallows the latter's `output`/`spawn`/
/// `status` outright ("can block the current thread for an unknown
/// duration"). `smol::block_on` around it keeps this function's signature
/// synchronous -- which is what lets [`EmdRunner`] stay a plain `Fn` with
/// a one-line fake -- and blocking is correct here precisely because the
/// only caller is inside `cx.background_spawn`.
pub fn run_emd(request: &EmdRequest) -> EmdRunOutcome {
    let output = smol::block_on(
        Command::new(&request.bin)
            .args(&request.args)
            .current_dir(&request.project_dir)
            .output(),
    );
    let output = match output {
        Ok(output) => output,
        Err(e) => {
            return EmdRunOutcome {
                ok: false,
                output: format!("running `{}`: {e}", request.command_line()),
                result: None,
            };
        }
    };
    let mut lines = capture_lines(&output.stdout);
    lines.extend(capture_lines(&output.stderr));
    emd_run_outcome(output.status.success(), &lines)
}

/// Captured bytes as lines, lossily decoded (a child's output is not
/// guaranteed UTF-8 and a mojibake transcript beats no transcript).
fn capture_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

/// The emerald project root for `start`: the nearest ancestor directory
/// (INCLUSIVE of `start` itself) holding `emerald.toml`, mirroring
/// emerald's own `Project::discover`.
///
/// This is `emd`'s working directory, and it is also the directory
/// `manifests/` lives under -- the same walk `ggo_map_panel`/
/// `ggo_sprite_panel`/`ggo_import_panel` make to find `<project>/assets`,
/// stopping one level earlier.
pub fn emerald_project_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(EMERALD_MANIFEST).is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// The file that marks an emerald project root.
pub const EMERALD_MANIFEST: &str = "emerald.toml";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emd_bin_defaults_to_the_bare_name_and_treats_blank_as_unset() {
        assert_eq!(resolve_emd_bin(None), DEFAULT_EMD_BIN);
        assert_eq!(resolve_emd_bin(Some(String::new())), DEFAULT_EMD_BIN);
        assert_eq!(resolve_emd_bin(Some("   ".into())), DEFAULT_EMD_BIN);
        assert_eq!(resolve_emd_bin(Some("/opt/emd".into())), "/opt/emd");
    }

    #[test]
    fn request_appends_json_once() {
        let req = EmdRequest::new(
            "/proj",
            vec!["generate".into(), "module".into(), "x".into()],
        );
        assert_eq!(req.args, ["generate", "module", "x", "--json"]);
        let already = EmdRequest::new(
            "/proj",
            vec![
                "generate".into(),
                "module".into(),
                "x".into(),
                "--json".into(),
            ],
        );
        assert_eq!(already.args, ["generate", "module", "x", "--json"]);
    }

    #[test]
    fn command_line_renders_an_empty_arg_as_quotes() {
        let req = EmdRequest {
            bin: "emd".into(),
            project_dir: PathBuf::from("/proj"),
            args: vec![
                "component".into(),
                "rm".into(),
                "--module".into(),
                String::new(),
            ],
        };
        assert_eq!(req.command_line(), "emd component rm --module \"\"");
    }

    /// A binary that cannot be spawned must come back as a NON-OK outcome
    /// naming the command -- not a panic, and not something the panel
    /// could mistake for success.
    #[test]
    fn a_missing_binary_is_a_non_ok_outcome_naming_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let request = EmdRequest {
            bin: "ggo-emd-that-does-not-exist".into(),
            project_dir: dir.path().to_path_buf(),
            args: vec![
                "generate".into(),
                "module".into(),
                "x".into(),
                "--json".into(),
            ],
        };
        let outcome = run_emd(&request);
        assert!(!outcome.ok);
        assert!(outcome.result.is_none());
        assert!(
            outcome.output.contains("ggo-emd-that-does-not-exist"),
            "{}",
            outcome.output
        );
    }

    /// End-to-end against a **real** `emd`: scaffold a project with
    /// `emd new`, then drive [`run_emd`] exactly as the panel does and
    /// assert both the success and the failure trailer round-trip.
    ///
    /// This is the one test that exercises the invocation SHAPE -- the
    /// working directory, the appended `--json`, the stdout/stderr
    /// capture -- against the actual CLI rather than against `sh`. It
    /// **skips** (rather than fails) when `emd new` doesn't succeed, so a
    /// checkout without the emerald toolchain stays green; every other
    /// test in this crate is hermetic by construction.
    #[test]
    fn run_emd_drives_a_real_emd_generate() {
        let dir = tempfile::tempdir().unwrap();
        let scaffold = EmdRequest {
            bin: emd_bin(),
            project_dir: dir.path().to_path_buf(),
            args: vec!["new".to_string(), "demo".to_string()],
        };
        if !run_emd(&scaffold).ok {
            return; // no `emd` on PATH -- nothing to integrate against
        }
        let project = dir.path().join("demo");

        let outcome = run_emd(&EmdRequest::new(
            &project,
            vec![
                "generate".to_string(),
                "component".to_string(),
                "hero_unit".to_string(),
                "--field".to_string(),
                "hp:int".to_string(),
            ],
        ));
        assert!(outcome.ok, "{}", outcome.output);
        let result = outcome.result.expect("a real run prints a trailer");
        assert_eq!(result["ok"], serde_json::json!(true));
        assert!(
            std::fs::read_to_string(project.join("manifests/components.toml"))
                .unwrap()
                .contains("HeroUnit"),
            "the manifest the world panel reads must have gained the component"
        );

        // The same command again fails -- and its trailer, which `emd`
        // prints to STDERR, is what has to reach the panel.
        let again = run_emd(&EmdRequest::new(
            &project,
            vec![
                "generate".to_string(),
                "component".to_string(),
                "hero_unit".to_string(),
            ],
        ));
        assert!(!again.ok);
        assert!(
            ggo_worldlib::emerald::emd_error_message(&again).contains("already exists"),
            "{}",
            again.output
        );
    }

    /// The transcript's stdout-then-stderr order is what makes a FAILING
    /// run's trailer (which `emd` prints to stderr) the one
    /// `parse_emd_trailer` finds. Exercised through a real child process,
    /// with `sh` standing in for `emd`.
    #[test]
    fn stderr_is_captured_after_stdout_so_a_failure_trailer_wins() {
        let dir = tempfile::tempdir().unwrap();
        let request = EmdRequest {
            bin: "sh".into(),
            project_dir: dir.path().to_path_buf(),
            args: vec![
                "-c".into(),
                "echo 'starting'; \
                 echo 'emd-json: {\"ok\":false,\"error\":\"nope\"}' >&2; \
                 exit 1"
                    .into(),
            ],
        };
        let outcome = run_emd(&request);
        assert!(!outcome.ok);
        assert_eq!(
            outcome.result.as_ref().unwrap()["error"],
            serde_json::json!("nope")
        );
        assert!(outcome.output.starts_with("starting"));
    }

    #[test]
    fn emerald_project_root_walks_up_inclusively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(EMERALD_MANIFEST), "").unwrap();
        std::fs::create_dir_all(root.join("assets/tiles")).unwrap();
        assert_eq!(emerald_project_root(root).as_deref(), Some(root));
        assert_eq!(
            emerald_project_root(&root.join("assets/tiles")).as_deref(),
            Some(root)
        );

        let outside = tempfile::tempdir().unwrap();
        assert_eq!(emerald_project_root(outside.path()), None);
    }
}
