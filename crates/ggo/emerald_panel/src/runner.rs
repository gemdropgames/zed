//! Running `emd` as a child process, and the seam that lets tests do it
//! without one.
//!
//! Everything that decides WHAT to run lives in `forms` (and, below that,
//! in `ggo_worldlib::emerald`'s argv builders). This module only decides
//! HOW: which binary, which working directory, and how a finished process
//! becomes the [`EmdRunOutcome`] worldlib's own result helpers understand.
//!
//! The spawn itself is **not here**: binary resolution, the `--json`
//! append, the working directory and the stdout-then-stderr capture all
//! live in [`ggo_common`] ([`ProcRequest`], `run_capture`), because the emu
//! panel shells out to the very same `emd` (and to `ggo-diag`) and a second
//! copy of that mechanism is exactly the duplication the fork's
//! single-source rule forbids. What stays here is the one thing that is
//! emerald-specific: mapping a raw capture onto worldlib's trailer-aware
//! [`EmdRunOutcome`], and the injectable [`EmdRunner`] seam shaped around
//! it.
//!
//! Mirrors the INVOCATION ggo-ide's `backend/emerald.rs` makes -- same
//! binary resolution convention, same `--json` trailer contract, same
//! "the transcript is stdout AND stderr, not just one half" rule -- and
//! deliberately none of its plumbing: no `Db`-backed settings, no
//! single-active-slot `EmdRunner` manager, no streaming line callback.
//! A `generate` is one short one-shot call whose only output that matters
//! is its trailer.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ggo_worldlib::emerald::{EmdRunOutcome, emd_run_outcome};

/// One resolved `emd` invocation. `ggo_common`'s general child-process
/// request, re-exported under the name this crate has always used for it;
/// build one with [`EmdRequest::emd`], which resolves the binary and
/// appends `--json`.
pub use ggo_common::ProcRequest as EmdRequest;
pub use ggo_common::{EMERALD_MANIFEST, emerald_project_root};

/// How long one `emd` invocation may run before the panel kills it.
///
/// **Ten minutes, ggo-ide's own `EMD_TIMEOUT` to the second**
/// (`backend/emerald.rs`), and for its stated reason: the mutation
/// commands this panel now issues -- `emd rm`, `component field rm` --
/// shell out to `cargo` themselves, and a cold `target/` dir makes the
/// difference between "slow" and "hung" a matter of minutes, not seconds.
/// A budget tight enough to be a useful progress signal would kill honest
/// work; this one only ever fires on something genuinely stuck (a
/// `cargo` waiting on a lock another process holds, say), which is exactly
/// when the panel must come back rather than sit on "Running…" forever.
///
/// F5.2 shipped no timeout at all because `emd generate` measures ~2 ms.
/// That was defensible for generate and is not defensible here.
pub const EMD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One in-flight `emd` run. Boxed because [`EmdRunner`] is a trait object;
/// `Send` because it is polled on `cx.background_spawn`'s thread.
pub type EmdRun = Pin<Box<dyn Future<Output = EmdRunOutcome> + Send>>;

/// The injection seam: anything that can turn an [`EmdRequest`] into an
/// [`EmdRunOutcome`].
///
/// A boxed `Fn` rather than a trait because every implementation is a
/// single function -- the real one below, and a recording fake in tests --
/// and `Arc` because the panel holds one and each spawned run needs its
/// own handle. `Send + Sync` because the call happens on
/// `cx.background_spawn`'s thread, never on the UI thread.
///
/// It hands back a FUTURE rather than a finished outcome (F5.3) so the run
/// can be given a deadline: the panel races this future against
/// `cx.background_executor().timer(EMD_TIMEOUT)` and drops it on expiry,
/// and `ggo_common::run_capture_async`'s `kill_on_drop` child turns that
/// drop into a dead `cargo check` rather than an orphan that keeps holding
/// the project's `target/` lock. A blocking `Fn` could not be interrupted
/// at all -- `smol::block_on` is not an await point, so cancelling the task
/// around it would return control to the panel while the child ran on.
///
/// Deliberately narrower than `ggo_common::ProcRunner`, which it wraps: a
/// panel that only ever runs `emd` wants the parsed trailer, not a raw
/// transcript, and pushing that mapping into every call site would be the
/// same duplication one level up.
pub type EmdRunner = Arc<dyn Fn(EmdRequest) -> EmdRun + Send + Sync>;

/// The production runner: really spawn `emd`.
pub fn system_runner() -> EmdRunner {
    Arc::new(|request| Box::pin(run_emd(request)))
}

/// Spawn `emd` and turn the capture into an outcome. Dropping the returned
/// future kills the child (`ggo_common::run_capture_async`).
///
/// The capture's stdout-then-stderr line order is what makes a FAILING
/// run's trailer (which `emd` prints to stderr) the one
/// [`ggo_worldlib::emerald::parse_emd_trailer`] finds; see
/// `ggo_common::run_capture_async` for the rest of the contract, including
/// how a binary that cannot be spawned is reported.
pub async fn run_emd(request: EmdRequest) -> EmdRunOutcome {
    let capture = ggo_common::run_capture_async(&request).await;
    emd_run_outcome(capture.ok, &capture.lines)
}

/// The outcome the panel applies when [`EMD_TIMEOUT`] expires and the child
/// is killed.
///
/// A synthesized non-ok [`EmdRunOutcome`] rather than a fourth run state:
/// a killed run IS a failed run, it just has no trailer of its own to
/// explain itself, and going through the same path means it inherits the
/// transcript rendering, the generation guard and the "the form stays
/// open" behaviour for free. `emd_error_message` falls back to `output`
/// when there is no trailer, so this text is what the user reads.
pub fn timed_out(request: &EmdRequest) -> EmdRunOutcome {
    EmdRunOutcome {
        ok: false,
        output: format!(
            "timed out after {} minutes and was killed: {}",
            EMD_TIMEOUT.as_secs() / 60,
            request.command_line()
        ),
        result: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_common::{EMD_BIN_ENV, emd_bin};

    /// Opt-out for the one non-hermetic test below: `GGO_ALLOW_NO_EMD=1`
    /// turns "no emerald toolchain" from a failure into a skip. Not a
    /// bare `is_ok()` -- an accidental empty export must not silence it.
    const ALLOW_NO_EMD_ENV: &str = "GGO_ALLOW_NO_EMD";

    fn allow_no_emd() -> bool {
        std::env::var(ALLOW_NO_EMD_ENV).is_ok_and(|v| v.trim() == "1")
    }

    /// A binary that cannot be spawned must come back as a NON-OK outcome
    /// naming the command -- not a panic, and not something the panel
    /// could mistake for success.
    #[test]
    fn a_missing_binary_is_a_non_ok_outcome_naming_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let request = EmdRequest::new(
            "ggo-emd-that-does-not-exist",
            dir.path(),
            vec![
                "generate".into(),
                "module".into(),
                "x".into(),
                "--json".into(),
            ],
        );
        let outcome = smol::block_on(run_emd(request));
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
    /// capture -- against the actual CLI rather than against `sh`; every
    /// other test in this crate is hermetic by construction.
    ///
    /// **It FAILS, rather than skipping, when `emd new` doesn't succeed**,
    /// unless [`ALLOW_NO_EMD_ENV`] is explicitly set. It used to `return`
    /// (and, briefly, `eprintln!` first) -- but libtest CAPTURES stdout and
    /// stderr for passing tests, so `GGO_EMD=/nonexistent/emd cargo test`
    /// printed nothing anywhere and reported `ok`. A silently-green
    /// integration test is precisely the failure mode the skip was
    /// supposed to make visible, so the default is now the loud one and
    /// opting out is a deliberate act.
    #[test]
    fn run_emd_drives_a_real_emd_generate() {
        let dir = tempfile::tempdir().unwrap();
        let scaffold = EmdRequest::new(
            emd_bin(),
            dir.path(),
            vec!["new".to_string(), "demo".to_string()],
        );
        if !smol::block_on(run_emd(scaffold)).ok {
            assert!(
                allow_no_emd(),
                "`{} new demo` did not succeed -- no emerald toolchain on PATH \
                 (point {EMD_BIN_ENV} at one, or set {ALLOW_NO_EMD_ENV}=1 to \
                 accept a checkout that cannot run this integration test)",
                emd_bin()
            );
            eprintln!("skip: no emerald toolchain, and {ALLOW_NO_EMD_ENV}=1 allows it");
            return;
        }
        let project = dir.path().join("demo");

        let outcome = smol::block_on(run_emd(EmdRequest::emd(
            &project,
            vec![
                "generate".to_string(),
                "component".to_string(),
                "hero_unit".to_string(),
                "--field".to_string(),
                "hp:int".to_string(),
            ],
        )));
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
        let again = smol::block_on(run_emd(EmdRequest::emd(
            &project,
            vec![
                "generate".to_string(),
                "component".to_string(),
                "hero_unit".to_string(),
            ],
        )));
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
        let request = EmdRequest::new(
            "sh",
            dir.path(),
            vec![
                "-c".into(),
                "echo 'starting'; \
                 echo 'emd-json: {\"ok\":false,\"error\":\"nope\"}' >&2; \
                 exit 1"
                    .into(),
            ],
        );
        let outcome = smol::block_on(run_emd(request));
        assert!(!outcome.ok);
        assert_eq!(
            outcome.result.as_ref().unwrap()["error"],
            serde_json::json!("nope")
        );
        assert!(outcome.output.starts_with("starting"));
    }
}
