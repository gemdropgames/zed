//! The `emd` version lock: what the panel knows about the installed
//! binary, whether that lets a mutation run, and what it says when it
//! doesn't.
//!
//! Pure, like [`ops`] and [`forms`]: no gpui, so every banner phrasing and
//! every gating decision is a unit test rather than a rendered window. The
//! panel side is three fields and a poll loop
//! (`EmeraldPanel::start_lock_poll`).
//!
//! **Nothing here re-words the mismatch cases.** `ggo_worldlib::emerald`
//! already owns them: [`check_version`] returns a typed
//! [`EmdError::VersionMismatch`] whose `Display` phrases `CliOld`,
//! `CliNew` and both halves of `Missing` (`emd was not found` vs `emd
//! reported an unrecognized version "X"`, off the `actual: Option<String>`
//! it carries), and those phrasings are pinned by that crate's own tests.
//! This module decides WHICH of them applies and adds at most one sentence
//! of its own, [`emd_bin_hint`], naming the override this fork actually has
//! -- and only where "where emd is found" is the remedy ([`lock_hint`]).
//!
//! ## The one asymmetry, and what this module does about it
//!
//! Worldlib deliberately compares "the emd version" two different ways:
//!
//! - [`compare_lock`] (the PRE-flight gate) parses a `major.minor.patch`
//!   PREFIX, so `0.2.0-rc1` compares EQUAL to `0.2.0`.
//! - `verify_emd_result` (the POST-run check, already on this panel's
//!   mutation path in `EmeraldPanel::finish_run`) is exact string equality
//!   on the trailer's own `emd` field, so `0.2.0-rc1` is a drift.
//!
//! Taken literally that means a suffixed build passes the gate and then has
//! EVERY result it produces downgraded to "emd version changed
//! mid-session" -- a panel whose buttons are all enabled and whose every
//! run fails for a reason the banner never mentioned. Real releases carry
//! no suffix, so it is latent rather than live, but this is the module that
//! wires the two together and it must pick one.
//!
//! **It picks strict, matching the enforcement half.** [`lock_error`] runs
//! `check_version` first (so all four `LockStatus` phrasings come from
//! worldlib) and THEN rejects anything whose version string is not
//! `EXPECTED_EMD_VERSION` character-for-character. The extra case is not a
//! fifth phrasing: a build whose triple matches but whose string does not
//! is exactly `Missing`'s "emd reported an unrecognized version" sub-case,
//! so it is reported by constructing that same [`EmdError`]. The gate and
//! the enforcement now agree, and the disagreement shows up BEFORE the run
//! rather than in its result.
//!
//! [`ops`]: crate::ops
//! [`forms`]: crate::forms
//! [`compare_lock`]: ggo_worldlib::emerald::compare_lock

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ggo_worldlib::emerald::{
    EMD_JSON_PREFIX, EXPECTED_EMD_VERSION, EmdError, EmdRunOutcome, LockStatus, check_version,
};

/// How often the panel re-stats the `emd` binary once the lock poll is
/// running.
///
/// **Thirty seconds, six times ggo-ide's five**, deliberately: the poll is
/// not the safety net. `verify_emd_result` on the mutation path is, and it
/// is immediate and unmissable -- every run's own trailer is checked
/// against [`EXPECTED_EMD_VERSION`] before its result is believed. All this
/// interval decides is how fast the BANNER catches up with an `emd` that
/// was replaced while the editor sat open, which is a comfort, not a
/// correctness property. Zed also keeps this panel alive for the whole
/// session (ggo-ide's emerald PAGE only existed while it was on screen), so
/// a tighter interval buys nothing and costs a wakeup.
///
/// A tick is one `fs::metadata` on the background executor, and only a
/// CHANGED result spends an `emd version` child process (see
/// [`BinProbe`]).
pub const EMD_LOCK_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// What the panel shows while the first check is still in flight.
///
/// ggo-ide showed nothing here (`emdLock.ts`'s `'unchecked'` renders no
/// message) even though mutations were already disabled. That is a panel
/// with dead buttons and no explanation, so this fork says the one true
/// thing instead. It is visible for as long as one `emd version` takes.
pub const CHECKING_MESSAGE: &str = "Checking the emd version…";

/// `emd version`'s argv, before `ggo_common::ProcRequest::emd` appends
/// `--json`. The trailer is what is actually read
/// ([`check_from_outcome`]); `emd` prints it either way, but `--json` is
/// what makes it a contract rather than an accident.
pub fn version_args() -> Vec<String> {
    vec!["version".to_string()]
}

/// The fork-local addendum to worldlib's mismatch text: where THIS host
/// looks for `emd`.
///
/// Worldlib's `Missing` phrasing points at "your emd path override or
/// PATH" and deliberately names no host's UI (it used to end "set emd_path
/// in Settings", ggo-ide's page, which this fork does not have -- fixed in
/// worldlib rather than papered over here, so the banner no longer reads as
/// an instruction followed by its own retraction). Naming the override is
/// this host's job, and this is it: `ggo_common::EMD_BIN_ENV`.
pub fn emd_bin_hint() -> String {
    format!(
        "Point {} at an emd {EXPECTED_EMD_VERSION} binary, or put one on PATH.",
        ggo_common::EMD_BIN_ENV
    )
}

/// [`emd_bin_hint`], but only for the states it actually answers.
///
/// It was once appended to every non-`Unchecked` state, which made it a non
/// sequitur under `CliOld`/`CliNew`: those are not lookup failures, the
/// installed binary was found and identified, and worldlib's line for them
/// already names the remedy (update the CLI / update the IDE). Telling a
/// user with a working `emd 0.1.0` to put one on PATH answers a question
/// they did not ask.
///
/// So: the two states where the binary in hand is the wrong one or is not
/// there at all -- `Unreachable`, and a `Reached` that resolved to
/// `Missing` (no parseable version, including this module's strict
/// pre-release rejection).
pub fn lock_hint(check: &LockCheck) -> Option<String> {
    let names_the_binary = match check {
        LockCheck::Unchecked => false,
        LockCheck::Unreachable(_) => true,
        LockCheck::Reached(actual) => matches!(
            lock_error(Some(actual)),
            Some(EmdError::VersionMismatch {
                status: LockStatus::Missing,
                ..
            })
        ),
    };
    names_the_binary.then(emd_bin_hint)
}

/// The panel's record of the last `emd version` probe.
///
/// Distinct from [`LockStatus`], which only exists once a version string is
/// in hand: `Unchecked` is the pre-first-check state (mutations are already
/// gated, and [`lock_message`] says why), and `Unreachable` carries a raw
/// spawn/exec error, which `EmdError` has nowhere to put.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LockCheck {
    /// No check has landed yet.
    #[default]
    Unchecked,
    /// `emd version` ran and reported this string.
    Reached(String),
    /// `emd` could not be run at all; the payload is the raw error.
    Unreachable(String),
}

/// The mismatch, if any, between `actual` and [`EXPECTED_EMD_VERSION`].
///
/// `check_version` owns the four `LockStatus` cases and their wording. The
/// second half is this module's strictness fix -- see the module doc: a
/// version whose TRIPLE matches but whose STRING does not (`0.2.0-rc1`)
/// passes `compare_lock` and would then be rejected by every post-run
/// `verify_emd_result`, so it is refused here instead, reported through
/// worldlib's own `Missing`/"unrecognized version" phrasing.
pub fn lock_error(actual: Option<&str>) -> Option<EmdError> {
    if let Err(e) = check_version(actual) {
        return Some(e);
    }
    // `check_version` returning `Ok` implies `actual` is `Some` --
    // `compare_lock(_, None)` is `Missing`, never `Ok`.
    let actual = actual?;
    (actual != EXPECTED_EMD_VERSION).then(|| EmdError::VersionMismatch {
        expected: EXPECTED_EMD_VERSION.to_string(),
        actual: Some(actual.to_string()),
        status: LockStatus::Missing,
    })
}

/// Whether `emd` mutations may run.
///
/// Only a LANDED check reporting the exact expected version -- ggo-ide's
/// `emdMutationsEnabled` rule, kept verbatim including its two sharp edges:
///
/// - **`CliNew` gates exactly as hard as `CliOld` and `Missing`.** A newer
///   `emd` is not a safe superset: the argv builders in
///   `ggo_worldlib::emerald` and the JSON trailer shapes this panel reads
///   were both written against 0.2.0, and "newer" says nothing about
///   whether `--module`'s meaning or `reverted`'s presence survived. The
///   remedy differs (update the IDE, not the CLI) and the banner says so;
///   the gate does not.
/// - **`Unchecked` gates too** -- fail closed. An unknown CLI is not a
///   known-good one, and the window is one `emd version` wide.
pub fn mutations_enabled(check: &LockCheck) -> bool {
    matches!(check, LockCheck::Reached(actual) if lock_error(Some(actual)).is_none())
}

/// The banner's leading line, or `None` when the lock has nothing to say
/// (the installed `emd` is exactly the expected one).
///
/// `Unreachable` renders worldlib's `Missing`-with-no-version phrasing
/// ("emd was not found -- ...") rather than a sentence of its own; the raw
/// error it carries is the DETAIL line, [`lock_detail`], because a spawn
/// error is evidence, not a diagnosis.
pub fn lock_message(check: &LockCheck) -> Option<String> {
    match check {
        LockCheck::Unchecked => Some(CHECKING_MESSAGE.to_string()),
        LockCheck::Reached(actual) => lock_error(Some(actual)).map(|e| e.to_string()),
        LockCheck::Unreachable(_) => Some(
            check_version(None)
                .expect_err("check_version(None) is always a Missing mismatch")
                .to_string(),
        ),
    }
}

/// The banner's muted second line: the raw error behind an `Unreachable`,
/// and nothing for any other state.
pub fn lock_detail(check: &LockCheck) -> Option<String> {
    match check {
        LockCheck::Unreachable(err) => Some(err.clone()),
        _ => None,
    }
}

/// Read an `emd version` run into a [`LockCheck`].
///
/// The trailer is preferred, and its `version` field first: `emd version
/// --json` prints `{"emd":"0.2.0","ok":true,"version":"0.2.0"}` (verified
/// against the 0.2.0 binary), and `emd` is the same string every command's
/// trailer carries, so either answers the question. A successful run with
/// no trailer at all falls back to its first non-trailer output line --
/// what a very old `emd version` printed, and ggo-ide's own fallback. Only
/// a run that produced neither is `Unreachable`, which is also how a binary
/// that could not be spawned arrives here (`ggo_common::run_capture_async`
/// reports that as a non-ok capture whose output names the command).
pub fn check_from_outcome(outcome: &EmdRunOutcome) -> LockCheck {
    let trailer = |key: &str| {
        outcome
            .result
            .as_ref()
            .and_then(|r| r.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    if let Some(version) = trailer("version").or_else(|| trailer("emd")) {
        return LockCheck::Reached(version);
    }
    if outcome.ok
        && let Some(line) = outcome
            .output
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with(EMD_JSON_PREFIX.trim_end()))
    {
        return LockCheck::Reached(line.to_string());
    }
    let trimmed = outcome.output.trim();
    LockCheck::Unreachable(if trimmed.is_empty() {
        "`emd version` produced no output".to_string()
    } else {
        trimmed.to_string()
    })
}

// ------------------------------------------------------------- the poll

/// What the last stat of the `emd` binary saw.
///
/// Three states, not `Option<SystemTime>`, because "never looked" and
/// "looked, found nothing" must not compare equal: the poll re-runs `emd
/// version` only when this CHANGES, so collapsing them would either
/// re-spawn a doomed `emd version` every tick forever on a machine that has
/// none (ggo-ide's behaviour: its `lock_poll_task` re-checks
/// unconditionally when the path won't resolve), or skip the very first
/// check on a machine that does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BinProbe {
    /// Nothing has been stat'd yet -- the next tick always re-checks.
    #[default]
    Unprobed,
    /// The binary resolved to a file last modified at this instant.
    At(SystemTime),
    /// The binary could not be resolved to a file at all.
    Unresolved,
}

/// How the panel stats the `emd` binary -- an injection seam for the same
/// reason [`crate::runner::EmdRunner`] is one, plus one more: "the poll is
/// not on the render path" is only a real assertion if the number of stats
/// can be COUNTED, and a bare `fs::metadata` call cannot be.
pub type LockProbe = Arc<dyn Fn() -> BinProbe + Send + Sync>;

/// The production probe: resolve `emd` the way the runner would and stat
/// it.
pub fn system_probe() -> LockProbe {
    Arc::new(|| {
        probe_bin(
            &ggo_common::emd_bin(),
            std::env::var("PATH").ok().as_deref(),
        )
    })
}

/// Stat `bin`'s mtime, resolving a bare name against `path_env` first.
pub fn probe_bin(bin: &str, path_env: Option<&str>) -> BinProbe {
    match resolve_bin_path(bin, path_env)
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
    {
        Some(mtime) => BinProbe::At(mtime),
        None => BinProbe::Unresolved,
    }
}

/// Resolve `bin` to an absolute path for stat purposes: a name containing a
/// separator is used as-is (and must actually be a file); a bare name is
/// searched across `path_env` in `PATH`'s own format. Ported from ggo-ide's
/// `pages::emerald::resolve_bin_path`.
fn resolve_bin_path(bin: &str, path_env: Option<&str>) -> Option<PathBuf> {
    if bin.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(bin);
        return p.is_file().then_some(p);
    }
    std::env::split_paths(path_env?)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::emerald::{compare_lock, emd_run_outcome};

    fn version_lines(v: &str) -> Vec<String> {
        vec![format!(
            "emd-json: {{\"emd\":\"{v}\",\"ok\":true,\"version\":\"{v}\"}}"
        )]
    }

    /// Each `LockStatus` gets its OWN banner line, and each of those lines
    /// is worldlib's, not one written here -- the four are asserted against
    /// `EmdError`'s own `Display` for the same input rather than against
    /// string literals copied out of it, so a worldlib re-wording moves
    /// this panel with it instead of failing here.
    #[test]
    fn every_lock_status_renders_its_own_banner_line() {
        let cases = [
            ("0.1.9", LockStatus::CliOld),
            ("0.3.0", LockStatus::CliNew),
            ("not-a-version", LockStatus::Missing),
        ];
        let mut seen: Vec<String> = Vec::new();
        for (actual, status) in cases {
            assert_eq!(compare_lock(EXPECTED_EMD_VERSION, Some(actual)), status);
            let msg = lock_message(&LockCheck::Reached(actual.to_string()))
                .unwrap_or_else(|| panic!("{actual} must produce a banner"));
            assert_eq!(
                msg,
                check_version(Some(actual)).unwrap_err().to_string(),
                "{status:?} must be worldlib's phrasing verbatim"
            );
            assert!(
                !seen.contains(&msg),
                "{status:?} reuses another case's line"
            );
            seen.push(msg);
        }
        // `Missing` with no version at all is the fourth case, and it is
        // NOT the same sentence as `Missing` with an unparseable one.
        let missing = lock_message(&LockCheck::Unreachable("no such file".into())).unwrap();
        assert_eq!(missing, check_version(None).unwrap_err().to_string());
        assert!(missing.contains("emd was not found"));
        assert!(!seen.contains(&missing));
        // ...and the raw spawn error is kept, as the detail line.
        assert_eq!(
            lock_detail(&LockCheck::Unreachable("no such file".into())).as_deref(),
            Some("no such file")
        );
        assert_eq!(lock_detail(&LockCheck::Reached("0.1.9".into())), None);
    }

    /// The four wordings, spelled out once, so a change to any of them is
    /// visible in a diff here as well as in worldlib.
    #[test]
    fn the_four_banner_lines_say_which_way_the_drift_went() {
        let line = |v: &str| lock_message(&LockCheck::Reached(v.to_string())).unwrap();
        assert!(line("0.1.9").contains("older than expected"));
        assert!(line("0.1.9").contains("cargo install --path crates/cli"));
        assert!(line("0.3.0").contains("this IDE build is too old"));
        assert!(line("nope").contains("unrecognized version \"nope\""));
        assert!(
            lock_message(&LockCheck::Unreachable("boom".into()))
                .unwrap()
                .contains("emd was not found")
        );
    }

    #[test]
    fn a_matching_version_clears_the_banner_and_allows_mutations() {
        let ok = LockCheck::Reached(EXPECTED_EMD_VERSION.to_string());
        assert_eq!(lock_message(&ok), None);
        assert_eq!(lock_detail(&ok), None);
        assert!(mutations_enabled(&ok));
    }

    /// Every non-`Ok` state gates, `CliNew` included -- see
    /// [`mutations_enabled`]'s doc for why "newer" is not "compatible".
    #[test]
    fn only_an_exact_landed_match_enables_mutations() {
        for check in [
            LockCheck::Unchecked,
            LockCheck::Unreachable("no such file".into()),
            LockCheck::Reached("0.1.9".into()),
            LockCheck::Reached("0.3.0".into()),
            LockCheck::Reached("not-a-version".into()),
            LockCheck::Reached(String::new()),
        ] {
            assert!(!mutations_enabled(&check), "{check:?} must gate");
        }
        assert!(mutations_enabled(&LockCheck::Reached(
            EXPECTED_EMD_VERSION.to_string()
        )));
    }

    /// While the first check is in flight the panel is gated but NOT
    /// silent -- ggo-ide's one behaviour here that this fork does not copy.
    #[test]
    fn unchecked_gates_but_still_explains_itself() {
        assert!(!mutations_enabled(&LockCheck::Unchecked));
        assert_eq!(
            lock_message(&LockCheck::Unchecked).as_deref(),
            Some(CHECKING_MESSAGE)
        );
    }

    /// **The pre-release asymmetry**, pinned at the seam that owns it.
    /// `compare_lock` says a suffixed build is `Ok`; `verify_emd_result`
    /// would reject every result it produced. [`lock_error`] takes the
    /// strict side, so the refusal happens up front, and it does so in
    /// worldlib's own `Missing` phrasing rather than a fifth sentence.
    #[test]
    fn a_pre_release_build_is_gated_up_front_not_after_every_run() {
        for suffixed in ["0.2.0-rc1", "0.2.0-beta", "0.2.0+build7"] {
            assert_eq!(
                compare_lock(EXPECTED_EMD_VERSION, Some(suffixed)),
                LockStatus::Ok,
                "compare_lock is prefix-tolerant -- that is the trap"
            );
            assert!(check_version(Some(suffixed)).is_ok(), "so check_version is");
            assert!(
                !mutations_enabled(&LockCheck::Reached(suffixed.to_string())),
                "{suffixed} must not reach a run whose every result would be downgraded"
            );
            let msg = lock_message(&LockCheck::Reached(suffixed.to_string())).unwrap();
            assert!(
                msg.contains(&format!("unrecognized version \"{suffixed}\"")),
                "{msg}"
            );
        }
        // A fourth version segment is `parse_triple`'s other tolerance
        // (inherited from the TS port) and lands in the same place.
        assert_eq!(
            compare_lock(EXPECTED_EMD_VERSION, Some("0.2.0.1")),
            LockStatus::Ok
        );
        assert!(!mutations_enabled(&LockCheck::Reached("0.2.0.1".into())));
    }

    #[test]
    fn a_version_run_is_read_off_its_trailer() {
        assert_eq!(
            check_from_outcome(&emd_run_outcome(true, &version_lines("0.2.0"))),
            LockCheck::Reached("0.2.0".to_string())
        );
        // The `emd` field alone is enough -- every command's trailer has it.
        assert_eq!(
            check_from_outcome(&emd_run_outcome(
                true,
                &["emd-json: {\"emd\":\"0.3.1\",\"ok\":true}".to_string()]
            )),
            LockCheck::Reached("0.3.1".to_string())
        );
    }

    #[test]
    fn a_trailerless_version_run_falls_back_to_its_first_output_line() {
        assert_eq!(
            check_from_outcome(&emd_run_outcome(true, &["0.1.4".to_string()])),
            LockCheck::Reached("0.1.4".to_string())
        );
    }

    /// A binary that could not be spawned is `Unreachable` carrying the
    /// capture's own message, which is what the detail line shows.
    #[test]
    fn a_failed_version_run_with_nothing_to_read_is_unreachable() {
        let out = emd_run_outcome(
            false,
            &["running `emd version`: No such file or directory".to_string()],
        );
        assert_eq!(
            check_from_outcome(&out),
            LockCheck::Unreachable("running `emd version`: No such file or directory".to_string())
        );
        assert_eq!(
            check_from_outcome(&emd_run_outcome(false, &[])),
            LockCheck::Unreachable("`emd version` produced no output".to_string())
        );
    }

    /// "Never probed" must not equal "probed, found nothing" -- otherwise
    /// a machine with no `emd` re-spawns `emd version` on every tick.
    #[test]
    fn an_unprobed_binary_is_not_an_unresolved_one() {
        assert_ne!(BinProbe::Unprobed, BinProbe::Unresolved);
        assert_eq!(BinProbe::default(), BinProbe::Unprobed);
    }

    #[test]
    fn probe_bin_stats_a_path_and_searches_path_for_a_bare_name() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("emd");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();

        let by_path = probe_bin(&bin.to_string_lossy(), None);
        assert!(matches!(by_path, BinProbe::At(_)), "{by_path:?}");
        assert_eq!(
            probe_bin("emd", Some(&dir.path().to_string_lossy())),
            by_path,
            "the same file found two ways must probe the same"
        );

        assert_eq!(
            probe_bin("emd", Some("/nonexistent-ggo-dir")),
            BinProbe::Unresolved
        );
        assert_eq!(probe_bin("emd", None), BinProbe::Unresolved);
        assert_eq!(
            probe_bin(&dir.path().join("nope").to_string_lossy(), None),
            BinProbe::Unresolved
        );
    }

    #[test]
    fn the_bin_hint_names_this_forks_override_not_a_settings_page() {
        let hint = emd_bin_hint();
        assert!(hint.contains(ggo_common::EMD_BIN_ENV), "{hint}");
        assert!(hint.contains(EXPECTED_EMD_VERSION), "{hint}");
        assert!(!hint.contains("Settings"), "{hint}");
        // ...and worldlib no longer names one either, so the banner is one
        // instruction, not an instruction and its retraction.
        for actual in [None, Some("nope"), Some("0.1.9"), Some("0.3.0")] {
            let msg = check_version(actual).unwrap_err().to_string();
            assert!(!msg.contains("Settings"), "{msg}");
        }
    }

    /// The hint answers "where is emd", so it appears only where that is
    /// the question: a binary that could not be run, or one that ran and
    /// reported no usable version. A `CliOld`/`CliNew` drift found the
    /// binary fine and its own line already says what to update.
    #[test]
    fn the_bin_hint_is_attached_to_lookup_failures_only() {
        for check in [
            LockCheck::Unreachable("no such file".into()),
            LockCheck::Reached("not-a-version".into()),
            LockCheck::Reached(String::new()),
            LockCheck::Reached("0.2.0-rc1".into()),
        ] {
            assert_eq!(
                lock_hint(&check).as_deref(),
                Some(emd_bin_hint().as_str()),
                "{check:?} must carry the hint"
            );
        }
        for check in [
            LockCheck::Unchecked,
            LockCheck::Reached("0.1.9".into()),
            LockCheck::Reached("0.3.0".into()),
            LockCheck::Reached(EXPECTED_EMD_VERSION.to_string()),
        ] {
            assert_eq!(lock_hint(&check), None, "{check:?} must not carry the hint");
        }
    }

    #[test]
    fn version_args_are_just_the_subcommand() {
        assert_eq!(version_args(), vec!["version".to_string()]);
    }
}
