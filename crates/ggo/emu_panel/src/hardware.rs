//! Flash the open game to a real board, and get this machine able to.
//!
//! `ggo-diag --project <dir> --tty <port> --skip-pnr` IS the flash
//! pipeline: it packs the project with `emd pack-ggo`, writes the
//! flash-backed sd-emu card image (GemOS + assets + the game), flashes the
//! cached bitstream with `fujprog`, boot-verifies over UART and records
//! the run in `~/.ggo/diag.db` -- the rows `ggo_charts_panel` reads. This
//! module owns the rules around that call and nothing else: what the
//! machine is missing, the argv, how to read the CLI's progress, and how
//! to install what is absent. Every function here is pure and unit-tested
//! without spawning anything; the panel is the part that spawns.
//!
//! **Not** a `.cart` write to a cartridge's QSPI NOR: `ggo-flash` is still
//! a stub (`exit(2)`), so the game reaches the board through the card
//! image. When that binary lands, it belongs here beside `flash_args`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ggo_common::ProcRequest;

use crate::menu::{
    DEFAULT_DIAG_BIN, DIAG_BIN_ENV, DIAG_REPO_ENV, DIAG_TTY_ENV, SERIAL_BY_ID_DIR,
};

/// Where a cloned GGO checkout lands when the user has none, under the
/// same `~/.ggo` the databases already live in.
pub const DEFAULT_CLONE_PARENT: &str = ".ggo";

/// `ssh://` form, NOT the scp-style `git@host:path`: `git clone` accepts
/// both, `cargo install --git` accepts only this one ("relative URL
/// without a base"), and one constant serving both callers has to be the
/// stricter spelling.
pub const GGO_REPO_URL: &str = "ssh://git@github.com/gemdropgames/ggo.git";
pub const EMERALD_REPO_URL: &str = "ssh://git@github.com/gemdropgames/emerald.git";

/// The binaries each repo installs -- `--git <url>` with no package spec
/// installs every binary in the workspace (or refuses as ambiguous).
pub const GGO_DIAG_CRATE: &str = "ggo-diag";
pub const EMD_CRATE: &str = "emerald-cli";

/// One unmet precondition for flashing. A value, not a sentence, so the
/// status line and [`setup_steps`] read from the same source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// No GGO repo checkout: the pipeline builds GemOS out of one.
    Repo,
    /// `ggo-diag` is not on `PATH`.
    Diag,
    /// `emd` is not on `PATH` (the pipeline shells out to it to pack).
    Emd,
    /// No serial device: nothing to flash or boot-verify against.
    Port,
    /// No project open, so there is no game to pack.
    Project,
}

impl Missing {
    /// What the status row says, in the fork's "name the gap" style.
    pub fn label(self) -> String {
        match self {
            Missing::Repo => format!(
                "no GGO repo checkout -- set {DIAG_REPO_ENV}, or let ZedGG clone one"
            ),
            Missing::Diag => format!(
                "`{DEFAULT_DIAG_BIN}` is not on PATH -- set {DIAG_BIN_ENV}, or let ZedGG install it"
            ),
            Missing::Emd => format!(
                "`{}` is not on PATH -- set {}, or let ZedGG install it",
                ggo_common::DEFAULT_EMD_BIN,
                ggo_common::EMD_BIN_ENV
            ),
            Missing::Port => format!(
                "no serial device (looked in {SERIAL_BY_ID_DIR}) -- connect the board, or set {DIAG_TTY_ENV}"
            ),
            Missing::Project => "no game project is open".to_string(),
        }
    }

    /// Can ZedGG fix this itself? A missing board cannot be installed.
    pub fn installable(self) -> bool {
        matches!(self, Missing::Repo | Missing::Diag | Missing::Emd)
    }
}

/// What this machine has, probed once. The panel re-probes after a setup
/// run so the button lights up without a restart.
#[derive(Debug, Clone, Default)]
pub struct HardwareEnv {
    /// The `ggo-diag` binary, when it resolves.
    pub diag_bin: Option<String>,
    /// The `emd` binary, when it resolves.
    pub emd_bin: Option<String>,
    /// The GGO repo checkout `ggo-diag` runs against.
    pub repo: Option<PathBuf>,
    /// An `emerald` checkout, if one sits beside the GGO repo -- an
    /// install source for `emd` that beats a network fetch.
    pub emerald: Option<PathBuf>,
    /// Candidate serial devices, in scan order.
    pub ports: Vec<String>,
    /// The open game project, which is what gets packed.
    pub project: Option<PathBuf>,
    /// Can we install anything at all?
    pub cargo: bool,
    pub git: bool,
    /// Where a clone would go.
    pub clone_dest: PathBuf,
    /// This user's home directory -- the one place a setup step can be
    /// spawned into without creating it first.
    pub home: PathBuf,
}

impl HardwareEnv {
    /// Every unmet precondition, in the order a reader should fix them.
    pub fn missing(&self) -> Vec<Missing> {
        let mut missing = Vec::new();
        if self.project.is_none() {
            missing.push(Missing::Project);
        }
        if self.repo.is_none() {
            missing.push(Missing::Repo);
        }
        if self.diag_bin.is_none() {
            missing.push(Missing::Diag);
        }
        if self.emd_bin.is_none() {
            missing.push(Missing::Emd);
        }
        if self.ports.is_empty() {
            missing.push(Missing::Port);
        }
        missing
    }

    pub fn ready(&self) -> bool {
        self.missing().is_empty()
    }

    /// Where a setup step runs. The home directory exists by
    /// construction; `~/.ggo` and the clone destination do not, and a
    /// child spawned into a missing directory dies before it can report
    /// anything useful.
    pub fn cwd_for_setup(&self) -> PathBuf {
        self.home.clone()
    }
}

/// What the setup page can do about one requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remedy {
    /// Already satisfied; the row shows where it was found.
    Satisfied,
    /// ZedGG can fix this: the page's Install button covers it.
    Install(String),
    /// Only the user can fix this -- the exact thing to do.
    Manual(String),
}

/// One row of the setup page. A view model, so the page renders and the
/// tests assert against the same values rather than parsed sentences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// Short name, e.g. `ggo-diag`.
    pub name: &'static str,
    /// Why the flash needs it.
    pub why: &'static str,
    /// Where it was found, when it was.
    pub found: Option<String>,
    pub remedy: Remedy,
}

impl Requirement {
    pub fn satisfied(&self) -> bool {
        self.found.is_some()
    }
}

impl HardwareEnv {
    /// Every prerequisite, satisfied or not, in the order the page lists
    /// them. Unsatisfied rows carry either the install ZedGG would run or
    /// the exact manual step -- a board and a serial permission cannot be
    /// installed, and saying nothing about them would leave the page a
    /// dead end.
    pub fn requirements(&self) -> Vec<Requirement> {
        vec![
            Requirement {
                name: "Game project",
                why: "the game that gets packed onto the card image",
                found: self
                    .project
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                remedy: match &self.project {
                    Some(_) => Remedy::Satisfied,
                    None => Remedy::Manual(format!(
                        "open a folder containing {}",
                        ggo_common::EMERALD_MANIFEST
                    )),
                },
            },
            Requirement {
                name: "GGO repo",
                why: "the pipeline builds GemOS and the gateware out of it",
                found: self.repo.as_ref().map(|p| p.to_string_lossy().into_owned()),
                remedy: match (&self.repo, self.git, self.clone_dest.exists()) {
                    (Some(_), _, _) => Remedy::Satisfied,
                    // Something is already at the clone destination and it
                    // is not a checkout (`probe` would have adopted it):
                    // say so, because cloning over it just fails.
                    (None, _, true) => Remedy::Manual(format!(
                        "{} already exists but is not a GGO checkout -- remove it, \
                         or set {DIAG_REPO_ENV} to a real one",
                        self.clone_dest.display()
                    )),
                    (None, true, false) => Remedy::Install(format!(
                        "git clone into {}",
                        self.clone_dest.display()
                    )),
                    (None, false, false) => Remedy::Manual(format!(
                        "install git, or set {DIAG_REPO_ENV} to an existing checkout"
                    )),
                },
            },
            Requirement {
                name: DEFAULT_DIAG_BIN,
                why: "runs the flash pipeline",
                found: self.diag_bin.clone(),
                remedy: match (&self.diag_bin, self.cargo) {
                    (Some(_), _) => Remedy::Satisfied,
                    (None, true) => Remedy::Install("cargo install ggo-diag".to_string()),
                    (None, false) => Remedy::Manual(format!(
                        "install Rust (cargo), or set {DIAG_BIN_ENV} to a built binary"
                    )),
                },
            },
            Requirement {
                name: ggo_common::DEFAULT_EMD_BIN,
                why: "packs the project into a .ggo",
                found: self.emd_bin.clone(),
                remedy: match (&self.emd_bin, self.cargo) {
                    (Some(_), _) => Remedy::Satisfied,
                    (None, true) => Remedy::Install("cargo install emd".to_string()),
                    (None, false) => Remedy::Manual(format!(
                        "install Rust (cargo), or set {} to a built binary",
                        ggo_common::EMD_BIN_ENV
                    )),
                },
            },
            Requirement {
                name: "Board",
                why: "the ULX3S to flash and boot-verify over UART",
                found: self.ports.first().cloned(),
                remedy: match self.ports.first() {
                    Some(_) => Remedy::Satisfied,
                    // Not installable, so the row has to carry the whole
                    // remedy: the two things that actually cause an empty
                    // scan are an unplugged board and a user who is not in
                    // the `dialout` group.
                    None => Remedy::Manual(format!(
                        "connect the board over USB; if it is connected, add \
                         yourself to the serial group (`sudo usermod -aG dialout \
                         $USER`, then log out and back in) or set {DIAG_TTY_ENV}"
                    )),
                },
            },
        ]
    }

}

/// `ggo-diag`'s argv for flashing `project` and booting it on `tty`.
///
/// `--skip-pnr` is not the CLI's default but IS the right default for a
/// one-click button: without it step 3 place-and-routes the whole SoC
/// (~20 minutes) before anything reaches the board, and a game change
/// never needs new gateware. `--project` implies `--provision`, so the
/// card image is rewritten with the freshly packed game every run.
pub fn flash_args(project: &Path, tty: &str) -> Vec<String> {
    vec![
        "--project".to_string(),
        project.to_string_lossy().into_owned(),
        "--tty".to_string(),
        tty.to_string(),
        "--skip-pnr".to_string(),
    ]
}

/// The flash invocation, or the list of what is missing instead.
///
/// `cwd` is the GGO repo: that CLI finds the repo by walking up from its
/// working directory, and this fork's worktree is the user's GAME
/// project, not the repo.
pub fn flash_request(env: &HardwareEnv) -> Result<ProcRequest, String> {
    let missing = env.missing();
    if !missing.is_empty() {
        return Err(format!(
            "flashing needs a board: {}",
            missing
                .iter()
                .map(|m| m.label())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let (Some(bin), Some(repo), Some(project), Some(tty)) = (
        env.diag_bin.clone(),
        env.repo.clone(),
        env.project.clone(),
        env.ports.first().cloned(),
    ) else {
        return Err("flashing needs a board".to_string());
    };
    Ok(ProcRequest::new(bin, repo, flash_args(&project, &tty)))
}

/// A stage of the pipeline, parsed from `ggo-diag`'s own output. The
/// grammar is that CLI's `diag/event.rs` printing, so this reads what it
/// actually emits rather than guessing at progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// `==> <title>` -- Compile firmware, Provision SD card, Flash board,
    /// Boot verify (UART), Report, ...
    Phase(String),
    /// `--> component <name>`: place-and-route of one component started.
    Component(String),
    /// `  [boot] <stage>` (optionally ` — <detail>`).
    Boot(String),
    /// `diag step <n>: running|PASS|FAIL|info`.
    DiagStep { index: String, status: String },
    /// `RESULT: PASS` / `RESULT: FAIL` -- the run's verdict.
    Result { pass: bool },
}

impl Stage {
    /// The status-row text for this stage.
    pub fn label(&self) -> String {
        match self {
            Stage::Phase(title) => title.clone(),
            Stage::Component(name) => format!("place & route: {name}"),
            Stage::Boot(stage) => format!("boot: {stage}"),
            Stage::DiagStep { index, status } => format!("diagnostic step {index}: {status}"),
            Stage::Result { pass: true } => "PASS".to_string(),
            Stage::Result { pass: false } => "FAIL".to_string(),
        }
    }
}

/// Why a run failed, in the child's own words: the last line that is
/// not one of `ggo-diag`'s own progress banners.
///
/// `ggo_common::failure_reason` takes the last non-blank line, which for
/// a streamed transcript is almost always `RESULT: FAIL` -- the verdict,
/// not the cause. The cause is a few lines above it.
pub fn failure_reason(capture: &ggo_common::ProcCapture) -> String {
    ggo_common::failure_line(capture, |line| parse_stage(line).is_some())
}

/// One output line as a stage, or `None` for lines that carry no
/// progress. An unknown line is not an error: it still reaches the
/// console, it just does not move the status row.
pub fn parse_stage(line: &str) -> Option<Stage> {
    let trimmed = line.trim_end();
    if let Some(title) = trimmed.strip_prefix("==> ") {
        return Some(Stage::Phase(title.trim().to_string()));
    }
    if let Some(name) = trimmed.strip_prefix("--> component ") {
        return Some(Stage::Component(name.trim().to_string()));
    }
    let body = trimmed.trim_start();
    if let Some(rest) = body.strip_prefix("[boot] ") {
        // `<stage>` or `<stage> — <detail>`; the stage alone is the label.
        let stage = rest.split(" — ").next().unwrap_or(rest);
        return Some(Stage::Boot(stage.trim().to_string()));
    }
    if let Some(rest) = body.strip_prefix("diag step ") {
        if let Some((index, status)) = rest.split_once(':') {
            return Some(Stage::DiagStep {
                index: index.trim().to_string(),
                status: status.trim().to_string(),
            });
        }
    }
    if let Some(verdict) = body.strip_prefix("RESULT: ") {
        return Some(Stage::Result {
            pass: verdict.trim() == "PASS",
        });
    }
    None
}

/// The phases a flash announces, in order. Pre-seeding them is what
/// lets the page answer "how much is left" before the run gets there;
/// the list is a hint, not a contract -- an unannounced phase is
/// inserted where it ran and a skipped one drops out.
pub const FLASH_PHASES: [&str; 5] = [
    "Compile firmware",
    "Provision SD card",
    "Flash board",
    "Boot verify (UART)",
    "Report",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseState {
    Pending,
    Running,
    Done,
    Failed,
}

/// One line of the flash timeline.
#[derive(Debug, Clone)]
pub struct PhaseRow {
    pub title: String,
    pub state: PhaseState,
    /// The newest sub-line: which component is placing, which boot stage
    /// is up. Replaced, not accumulated -- the transcript is the log's
    /// job, this is the "right now".
    pub detail: Option<String>,
    started: Duration,
    finished: Option<Duration>,
}

impl PhaseRow {
    /// How long this phase has taken, `now` being the run's elapsed
    /// time. A finished phase stops counting; a running one does not, so
    /// the caller's clock -- not this model -- is what ticks.
    pub fn elapsed(&self, now: Duration) -> Duration {
        match self.state {
            PhaseState::Pending => Duration::ZERO,
            _ => self.finished.unwrap_or(now).saturating_sub(self.started),
        }
    }
}

/// One diagnostic-cart step, latest status only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagStep {
    pub index: String,
    pub status: String,
}

/// A run's shape as it happens: which phase, for how long, what it is
/// doing, and how it ended.
///
/// Pure and clock-free -- every mutator takes the run's elapsed time
/// from its caller, which is what makes the whole thing unit-testable
/// without spawning or sleeping.
#[derive(Debug, Clone, Default)]
pub struct FlashProgress {
    rows: Vec<PhaseRow>,
    diag_steps: Vec<DiagStep>,
    verdict: Option<bool>,
}

impl FlashProgress {
    /// A flash run: the expected pipeline, all pending.
    pub fn flash() -> Self {
        Self::steps(FLASH_PHASES.iter().map(|title| title.to_string()).collect())
    }

    /// A run whose phases the caller names -- a setup run emits none of
    /// `ggo-diag`'s banners, so its steps come through [`Self::advance_to`].
    pub fn steps(titles: Vec<String>) -> Self {
        Self {
            rows: titles
                .into_iter()
                .map(|title| PhaseRow {
                    title,
                    state: PhaseState::Pending,
                    detail: None,
                    started: Duration::ZERO,
                    finished: None,
                })
                .collect(),
            diag_steps: Vec::new(),
            verdict: None,
        }
    }

    pub fn rows(&self) -> &[PhaseRow] {
        &self.rows
    }

    pub fn diag_steps(&self) -> &[DiagStep] {
        &self.diag_steps
    }

    pub fn verdict(&self) -> Option<bool> {
        self.verdict
    }

    pub fn running(&self) -> Option<&PhaseRow> {
        self.rows.iter().find(|row| row.state == PhaseState::Running)
    }

    fn running_mut(&mut self) -> Option<&mut PhaseRow> {
        self.rows
            .iter_mut()
            .find(|row| row.state == PhaseState::Running)
    }

    /// Start `title`, closing whatever was running.
    ///
    /// A pending row with this title is claimed where it sits, and the
    /// pending rows jumped over are dropped: `--skip-pnr` skips phases,
    /// and a skipped one left above the running phase reads as still to
    /// come. A title nobody expected is inserted at that same place.
    pub fn advance_to(&mut self, title: &str, at: Duration) {
        if let Some(running) = self.running_mut() {
            running.state = PhaseState::Done;
            running.finished = Some(at);
        }
        let cursor = self
            .rows
            .iter()
            .position(|row| row.state == PhaseState::Pending)
            .unwrap_or(self.rows.len());
        let claimed = self.rows[cursor..]
            .iter()
            .position(|row| row.title == title)
            .map(|offset| cursor + offset);
        match claimed {
            Some(index) => {
                self.rows.drain(cursor..index);
            }
            None => self.rows.insert(
                cursor,
                PhaseRow {
                    title: title.to_string(),
                    state: PhaseState::Pending,
                    detail: None,
                    started: Duration::ZERO,
                    finished: None,
                },
            ),
        }
        if let Some(row) = self.rows.get_mut(cursor) {
            row.state = PhaseState::Running;
            row.started = at;
            row.finished = None;
        }
    }

    /// Fold one output line in. Unrecognised lines move nothing -- they
    /// are the log's business.
    pub fn apply(&mut self, line: &str, at: Duration) {
        let Some(stage) = parse_stage(line) else {
            return;
        };
        match stage {
            Stage::Phase(title) => self.advance_to(&title, at),
            Stage::Component(_) | Stage::Boot(_) => {
                let detail = stage.label();
                if let Some(row) = self.running_mut() {
                    row.detail = Some(detail);
                }
            }
            Stage::DiagStep { index, status } => {
                match self.diag_steps.iter_mut().find(|step| step.index == index) {
                    Some(step) => step.status = status,
                    None => self.diag_steps.push(DiagStep { index, status }),
                }
            }
            Stage::Result { pass } => {
                self.verdict = Some(pass);
                if pass {
                    if let Some(row) = self.running_mut() {
                        row.state = PhaseState::Done;
                        row.finished = Some(at);
                    }
                    // Nothing else is coming: what is still pending was
                    // skipped, not queued.
                    self.rows.retain(|row| row.state != PhaseState::Pending);
                } else {
                    self.fail(at);
                }
            }
        }
    }

    /// The run died without saying so itself: a non-zero exit, or a
    /// `RESULT: FAIL`. The phase it was in is where it died.
    pub fn fail(&mut self, at: Duration) {
        self.verdict = Some(false);
        if let Some(row) = self.running_mut() {
            row.state = PhaseState::Failed;
            row.finished = Some(at);
        }
    }
}

/// One install step: a label for the console and the command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStep {
    pub label: String,
    pub request: ProcRequest,
}

/// What to run to make this machine able to flash, in order.
///
/// Only what is actually absent, and never the OSS CAD Suite: `ggo-diag`
/// downloads that itself on its first run (its `toolchain::ensure` calls
/// the repo's `scripts/setup.sh`), and that download streams through the
/// same console as everything else. A local checkout always beats a
/// network fetch as the install source -- the user's working copy is the
/// version they are actually developing against.
pub fn setup_steps(env: &HardwareEnv) -> Vec<SetupStep> {
    let mut steps = Vec::new();
    let repo = env.repo.clone().unwrap_or_else(|| env.clone_dest.clone());
    // Every step runs somewhere that is guaranteed to exist: a child
    // spawned with a `current_dir` that is not there fails as "No such
    // file or directory" before it can say anything useful, and on the
    // fresh machine this feature is FOR, neither `~/.ggo` nor the clone
    // destination exists yet.
    let cwd = env.cwd_for_setup();
    // `git clone` refuses a destination that already exists, and by this
    // point `probe` has already adopted it if it were a real checkout --
    // so an existing path here is something else, and overwriting it is
    // not ours to do.
    let dest_taken = env.clone_dest.exists();
    let cloning = env.repo.is_none() && env.git && !dest_taken;
    if cloning {
        steps.push(SetupStep {
            label: format!("clone the GGO repo into {}", repo.display()),
            request: ProcRequest::new(
                "git",
                cwd.clone(),
                vec![
                    "clone".to_string(),
                    GGO_REPO_URL.to_string(),
                    repo.to_string_lossy().into_owned(),
                ],
            ),
        });
    }
    if !env.cargo {
        return steps;
    }
    // The clone above lands before these run, so it counts as a local
    // checkout -- installing from git would re-fetch what we just cloned.
    let have_repo = env.repo.is_some() || cloning;
    if env.diag_bin.is_none() {
        steps.push(SetupStep {
            label: "install ggo-diag".to_string(),
            request: cargo_install(
                &repo,
                "tools/ggo-diag",
                GGO_REPO_URL,
                GGO_DIAG_CRATE,
                have_repo,
                &cwd,
            ),
        });
    }
    if env.emd_bin.is_none() {
        let (root, has_local) = match &env.emerald {
            Some(emerald) => (emerald.clone(), true),
            None => (repo, false),
        };
        steps.push(SetupStep {
            label: "install emd".to_string(),
            request: cargo_install(
                &root,
                "crates/cli",
                EMERALD_REPO_URL,
                EMD_CRATE,
                has_local,
                &cwd,
            ),
        });
    }
    steps
}

/// `cargo install` from a local checkout when there is one, else from
/// git. `--path` is `root/sub`; the fallback fetches `crate_name` out of
/// `url`. Runs in `cwd`, which the caller guarantees exists.
fn cargo_install(
    root: &Path,
    sub: &str,
    url: &str,
    crate_name: &str,
    local: bool,
    cwd: &Path,
) -> ProcRequest {
    let args = if local {
        vec![
            "install".to_string(),
            "--locked".to_string(),
            "--path".to_string(),
            root.join(sub).to_string_lossy().into_owned(),
        ]
    } else {
        vec![
            "install".to_string(),
            "--locked".to_string(),
            "--git".to_string(),
            url.to_string(),
            crate_name.to_string(),
        ]
    };
    ProcRequest::new("cargo", cwd.to_path_buf(), args)
}

/// Does `dir` hold a GGO checkout?
pub fn is_repo(dir: &Path) -> bool {
    dir.join(crate::menu::REPO_FINGERPRINT).is_file()
}

/// Expand a leading `~` against `home`. Env vars are not shell words, so
/// nothing else does this and a `~` reaches the filesystem literally.
pub fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    // Only a bare `~/`: `~someone/x` names ANOTHER user's home, which is
    // not this one and not ours to guess.
    match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    }
}

/// Resolve `bin` the way a spawn would: a path with a separator must
/// exist as given, a bare name is looked up in `path_env`. Parametrized
/// over PATH (rather than reading the process env) so it is testable
/// against a fixture tree -- `ggo_emerald_panel::lock`'s resolver makes
/// the same call for the same reason.
pub fn resolve_on_path(bin: &str, path_env: Option<&str>) -> Option<String> {
    if bin.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(bin).is_file().then(|| bin.to_string());
    }
    // `cargo` is `cargo.exe` on Windows; without the suffix every probe
    // there reports a missing toolchain that is in fact installed.
    let candidates: Vec<String> = if cfg!(windows) {
        vec![bin.to_string(), format!("{bin}.exe")]
    } else {
        vec![bin.to_string()]
    };
    std::env::split_paths(path_env?)
        .flat_map(|dir| candidates.iter().map(move |name| dir.join(name)))
        .any(|candidate| candidate.is_file())
        .then(|| bin.to_string())
}

/// Probe this machine. `project` is the open game project (the worktree
/// root), `home` is where a clone would land under. Every filesystem and
/// env read the flash flow makes happens here, once.
pub fn probe(project: Option<&Path>, path_env: Option<&str>, home: &Path) -> HardwareEnv {
    let diag = std::env::var(DIAG_BIN_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DIAG_BIN.to_string());
    let diag_bin = resolve_on_path(&diag, path_env);
    let emd_bin = resolve_on_path(&ggo_common::emd_bin(), path_env);
    let clone_dest = home.join(DEFAULT_CLONE_PARENT).join("ggo");
    let repo = std::env::var(DIAG_REPO_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        // A `~` typed into an env var is not expanded by anything: the
        // shell only does it for un-quoted words it parses itself.
        .map(|v| expand_home(&v, home))
        .filter(|repo| is_repo(repo))
        .or_else(|| project.and_then(crate::menu::find_repo_root))
        // A clone this feature made earlier is a checkout like any other;
        // without this the repo row stays missing and Install tries to
        // clone on top of it ("destination path already exists").
        .or_else(|| is_repo(&clone_dest).then(|| clone_dest.clone()));
    // An `emerald` checkout beside the GGO repo is the natural `emd`
    // source; ggo-diag's own repo layout puts them as siblings.
    let emerald = repo
        .as_ref()
        .and_then(|repo| repo.parent())
        .map(|parent| parent.join("emerald"))
        .filter(|emerald| emerald.join("crates/cli/Cargo.toml").is_file());
    let ports = match std::env::var(DIAG_TTY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(tty) => vec![tty],
        None => crate::menu::scan_serial_ports_at(
            Path::new(SERIAL_BY_ID_DIR),
            Path::new(crate::menu::DEV_DIR),
        ),
    };
    HardwareEnv {
        diag_bin,
        emd_bin,
        repo,
        emerald,
        ports,
        // The subject is an EMERALD project, not merely an open folder:
        // `ggo-diag --project` hands it to `emd pack-ggo`, which fails
        // deep inside itself on a folder that is not one.
        project: project.and_then(ggo_common::emerald_project_root),
        cargo: resolve_on_path("cargo", path_env).is_some(),
        git: resolve_on_path("git", path_env).is_some(),
        clone_dest,
        home: home.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ready_env() -> HardwareEnv {
        HardwareEnv {
            diag_bin: Some("ggo-diag".into()),
            emd_bin: Some("emd".into()),
            repo: Some(PathBuf::from("/repo")),
            emerald: None,
            ports: vec!["/dev/ttyUSB0".into()],
            project: Some(PathBuf::from("/game")),
            cargo: true,
            git: true,
            clone_dest: PathBuf::from("/home/u/.ggo/ggo"),
            home: PathBuf::from("/home/u"),
        }
    }

    #[test]
    fn flash_args_pack_the_project_and_skip_place_and_route() {
        assert_eq!(
            flash_args(Path::new("/game"), "/dev/ttyUSB0"),
            vec!["--project", "/game", "--tty", "/dev/ttyUSB0", "--skip-pnr"],
        );
    }

    #[test]
    fn flash_request_runs_in_the_repo_not_the_project() {
        let request = flash_request(&ready_env()).expect("a ready machine flashes");
        assert_eq!(request.bin, "ggo-diag");
        assert_eq!(
            request.cwd,
            PathBuf::from("/repo"),
            "ggo-diag walks up from its cwd to find the repo"
        );
        assert!(request.args.contains(&"/game".to_string()));
    }

    #[test]
    fn every_missing_prerequisite_is_named_and_nothing_is_spawned() {
        let mut env = HardwareEnv::default();
        assert_eq!(
            env.missing(),
            vec![
                Missing::Project,
                Missing::Repo,
                Missing::Diag,
                Missing::Emd,
                Missing::Port
            ]
        );
        let error = flash_request(&env).expect_err("an empty machine cannot flash");
        for missing in env.missing() {
            assert!(
                error.contains(&missing.label()),
                "the error names {missing:?}: {error}"
            );
        }

        // A board is the one gap ZedGG cannot close for the user.
        assert!(!Missing::Port.installable());
        assert!(Missing::Repo.installable() && Missing::Diag.installable());

        env = ready_env();
        env.ports.clear();
        assert_eq!(env.missing(), vec![Missing::Port]);
        assert!(!env.ready());
        assert!(ready_env().ready());
    }

    #[test]
    fn stages_parse_from_the_cli_grammar() {
        assert_eq!(
            parse_stage("==> Flash board"),
            Some(Stage::Phase("Flash board".into()))
        );
        assert_eq!(
            parse_stage("--> component ppu"),
            Some(Stage::Component("ppu".into()))
        );
        assert_eq!(
            parse_stage("  [boot] banner"),
            Some(Stage::Boot("banner".into()))
        );
        assert_eq!(
            parse_stage("  [boot] banner — GemOS 0.1"),
            Some(Stage::Boot("banner".into())),
            "the detail is console material, not status material"
        );
        assert_eq!(
            parse_stage("diag step 3: PASS"),
            Some(Stage::DiagStep {
                index: "3".into(),
                status: "PASS".into()
            })
        );
        assert_eq!(parse_stage("RESULT: PASS"), Some(Stage::Result { pass: true }));
        assert_eq!(
            parse_stage("RESULT: FAIL"),
            Some(Stage::Result { pass: false })
        );
        // Everything else is console-only.
        assert_eq!(parse_stage(""), None);
        assert_eq!(parse_stage("warning: something"), None);
        assert_eq!(parse_stage("<-- component ppu: ok luts=123"), None);
    }

    #[test]
    fn stage_labels_read_as_status_text() {
        assert_eq!(Stage::Phase("Flash board".into()).label(), "Flash board");
        assert_eq!(
            Stage::Component("ppu".into()).label(),
            "place & route: ppu"
        );
        assert_eq!(Stage::Boot("banner".into()).label(), "boot: banner");
        assert_eq!(Stage::Result { pass: true }.label(), "PASS");
    }

    #[test]
    fn setup_installs_only_what_is_absent_preferring_local_checkouts() {
        // A ready machine needs nothing.
        assert!(setup_steps(&ready_env()).is_empty());

        // Nothing present: clone first, then the two binaries, and the
        // installs come from git because there is no checkout to build.
        let bare = HardwareEnv {
            cargo: true,
            git: true,
            clone_dest: PathBuf::from("/home/u/.ggo/ggo"),
            ..Default::default()
        };
        let steps = setup_steps(&bare);
        assert_eq!(steps.len(), 3, "clone, ggo-diag, emd");
        assert_eq!(steps[0].request.bin, "git");
        assert_eq!(steps[0].request.args[0], "clone");
        assert!(
            steps[0].request.args.contains(&GGO_REPO_URL.to_string())
                && GGO_REPO_URL.starts_with("ssh://"),
            "cargo cannot parse scp-style URLs, so one spelling serves both"
        );
        assert!(steps[1].label.contains("ggo-diag"));
        assert!(
            steps[1]
                .request
                .args
                .contains(&"/home/u/.ggo/ggo/tools/ggo-diag".to_string()),
            "the clone above IS the checkout to build from: {:?}",
            steps[1].request.args
        );
        assert!(steps[2].label.contains("emd"));
        for step in &steps {
            assert_eq!(
                step.request.cwd, bare.home,
                "every step runs in a directory that exists"
            );
        }

        // With a checkout, ggo-diag builds from it and no clone runs.
        let mut local = bare.clone();
        local.repo = Some(PathBuf::from("/repo"));
        local.emerald = Some(PathBuf::from("/emerald"));
        let steps = setup_steps(&local);
        assert_eq!(steps.len(), 2, "nothing to clone");
        assert!(steps[0].request.args.contains(&"--path".to_string()));
        assert!(
            steps[0]
                .request
                .args
                .contains(&"/repo/tools/ggo-diag".to_string()),
            "{:?}",
            steps[0].request.args
        );
        assert!(
            steps[1]
                .request
                .args
                .contains(&"/emerald/crates/cli".to_string()),
            "a sibling emerald checkout is the emd source: {:?}",
            steps[1].request.args
        );

        // No cargo: the clone is still worth doing, the installs are not
        // attempted at all.
        let mut no_cargo = bare.clone();
        no_cargo.cargo = false;
        let steps = setup_steps(&no_cargo);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].request.bin, "git");

        // No git and no checkout: nothing to clone with, but a `--git`
        // cargo install still works.
        let mut no_git = bare;
        no_git.git = false;
        let steps = setup_steps(&no_git);
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s.request.bin == "cargo"));
        // Nothing to clone with and no checkout: fetch, and name the
        // crate -- a bare `--git <url>` installs every binary in the
        // workspace, or refuses as ambiguous.
        assert!(steps[0].request.args.contains(&"--git".to_string()));
        assert!(steps[0].request.args.contains(&GGO_DIAG_CRATE.to_string()));
        assert!(steps[1].request.args.contains(&EMD_CRATE.to_string()));
    }

    #[test]
    fn resolve_on_path_finds_bare_names_and_checks_explicit_ones() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("ggo-diag");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let path_env = dir.path().to_string_lossy().into_owned();

        assert_eq!(
            resolve_on_path("ggo-diag", Some(&path_env)),
            Some("ggo-diag".to_string()),
            "a bare name resolves against PATH"
        );
        assert_eq!(resolve_on_path("ggo-diag", None), None, "no PATH, no lookup");
        assert_eq!(resolve_on_path("nope", Some(&path_env)), None);

        let explicit = bin.to_string_lossy().into_owned();
        assert_eq!(
            resolve_on_path(&explicit, None),
            Some(explicit.clone()),
            "an explicit path needs no PATH, only the file"
        );
        assert_eq!(
            resolve_on_path(&dir.path().join("gone").to_string_lossy(), None),
            None,
            "an explicit path that is not there does not resolve"
        );
    }

    #[test]
    fn the_failure_reason_is_the_cause_not_the_verdict_banner() {
        let capture = ggo_common::ProcCapture {
            ok: false,
            lines: vec![
                "==> Flash board".to_string(),
                "fujprog: cannot open /dev/ttyUSB0".to_string(),
                "diag step 2: FAIL".to_string(),
                "RESULT: FAIL".to_string(),
            ],
        };
        assert_eq!(
            failure_reason(&capture),
            "fujprog: cannot open /dev/ttyUSB0",
            "the last NON-progress line is what went wrong"
        );
        // Nothing but banners: fall back rather than invent silence.
        let all_banners = ggo_common::ProcCapture {
            ok: false,
            lines: vec!["==> Flash board".to_string(), "RESULT: FAIL".to_string()],
        };
        assert_eq!(failure_reason(&all_banners), "RESULT: FAIL");
    }

    #[test]
    fn a_previous_clone_is_adopted_and_never_cloned_over() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".ggo/ggo");

        // Nothing there yet: clone.
        let mut env = HardwareEnv {
            cargo: true,
            git: true,
            clone_dest: dest.clone(),
            home: dir.path().to_path_buf(),
            ..Default::default()
        };
        assert!(matches!(
            env.requirements()[1].remedy,
            Remedy::Install(_)
        ));
        assert_eq!(setup_steps(&env)[0].request.args[0], "clone");

        // The destination exists but is not a checkout: cloning into it
        // would just fail, so say what to do instead.
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stray.txt"), b"x").unwrap();
        match &env.requirements()[1].remedy {
            Remedy::Manual(what) => assert!(
                what.contains("already exists"),
                "the row explains the collision: {what}"
            ),
            other => panic!("expected a manual remedy, got {other:?}"),
        }
        assert!(
            setup_steps(&env).iter().all(|s| s.request.bin != "git"),
            "never clone onto an existing path"
        );

        // A real checkout there IS the repo -- what `probe` adopts.
        std::fs::create_dir_all(dest.join("firmware/system")).unwrap();
        std::fs::write(dest.join(crate::menu::REPO_FINGERPRINT), b"[package]").unwrap();
        assert!(is_repo(&dest));
        env.repo = Some(dest);
        assert_eq!(env.requirements()[1].remedy, Remedy::Satisfied);
        assert!(
            setup_steps(&env).iter().all(|s| s.request.bin != "git"),
            "an adopted checkout needs no clone"
        );
    }

    #[test]
    fn a_tilde_in_the_env_var_is_expanded() {
        let home = Path::new("/home/u");
        assert_eq!(expand_home("~/.ggo/ggo", home), PathBuf::from("/home/u/.ggo/ggo"));
        assert_eq!(expand_home("~", home), PathBuf::from("/home/u"));
        assert_eq!(
            expand_home("/abs/path", home),
            PathBuf::from("/abs/path"),
            "an absolute path is left alone"
        );
        assert_eq!(
            expand_home("~user/x", home),
            PathBuf::from("~user/x"),
            "another user's home is not ours to guess"
        );
    }

    // ------------------------------------------------- flash progress

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A run starts with the whole pipeline visible and nothing done:
    /// the point of the page is answering "how much is left".
    #[test]
    fn a_flash_starts_with_every_phase_pending() {
        let progress = FlashProgress::flash();
        assert_eq!(
            progress
                .rows()
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            FLASH_PHASES.to_vec(),
        );
        assert!(progress.rows().iter().all(|row| row.state == PhaseState::Pending));
        assert_eq!(progress.verdict(), None);
    }

    /// An announced phase becomes the running one and closes the phase
    /// before it, which keeps its own elapsed time.
    #[test]
    fn an_announced_phase_closes_the_one_before_it() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Compile firmware", secs(0));
        progress.apply("==> Provision SD card", secs(12));

        let rows = progress.rows();
        assert_eq!(rows[0].state, PhaseState::Done);
        assert_eq!(rows[0].elapsed(secs(30)), secs(12), "a done phase stops counting");
        assert_eq!(rows[1].state, PhaseState::Running);
        assert_eq!(rows[1].elapsed(secs(30)), secs(18), "the running one keeps counting");
        assert_eq!(rows[2].state, PhaseState::Pending);
    }

    /// Component and boot lines say what the running phase is doing;
    /// they are not phases themselves.
    #[test]
    fn detail_lines_annotate_the_running_phase_without_adding_rows() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Flash board", secs(0));
        let before = progress.rows().len();
        progress.apply("--> component cpu", secs(1));
        assert_eq!(progress.rows().len(), before, "no row was added");
        assert_eq!(
            progress.running().and_then(|row| row.detail.clone()),
            Some("place & route: cpu".to_string()),
        );
        progress.apply("  [boot] banner — GemOS", secs(2));
        assert_eq!(
            progress.running().and_then(|row| row.detail.clone()),
            Some("boot: banner".to_string()),
            "the newest detail replaces the last",
        );
    }

    /// A diagnostic step is one chip that changes status, not a new one
    /// per line.
    #[test]
    fn diag_steps_are_upserted_by_index() {
        let mut progress = FlashProgress::flash();
        progress.apply("diag step 1: running", secs(0));
        progress.apply("diag step 1: PASS", secs(1));
        progress.apply("diag step 2: running", secs(2));
        assert_eq!(
            progress
                .diag_steps()
                .iter()
                .map(|step| (step.index.as_str(), step.status.as_str()))
                .collect::<Vec<_>>(),
            vec![("1", "PASS"), ("2", "running")],
        );
    }

    /// A FAIL verdict marks the phase it happened in, so the page points
    /// at where the run died rather than only saying that it did.
    #[test]
    fn a_fail_verdict_marks_the_running_phase() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Flash board", secs(0));
        progress.apply("RESULT: FAIL", secs(5));
        assert_eq!(progress.verdict(), Some(false));
        let failed = progress
            .rows()
            .iter()
            .find(|row| row.state == PhaseState::Failed)
            .expect("the phase that died");
        assert_eq!(failed.title, "Flash board");
        assert!(
            progress
                .rows()
                .iter()
                .any(|row| row.state == PhaseState::Pending),
            "the phases after it never ran",
        );
    }

    /// A PASS verdict leaves nothing running.
    #[test]
    fn a_pass_verdict_closes_the_last_phase() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Report", secs(0));
        progress.apply("RESULT: PASS", secs(3));
        assert_eq!(progress.verdict(), Some(true));
        assert!(progress.running().is_none());
        assert!(
            progress
                .rows()
                .iter()
                .any(|row| row.title == "Report" && row.state == PhaseState::Done),
        );
    }

    /// A finished run has no "still to come": whatever never ran was
    /// skipped, and a pending row under a PASS reads as unfinished work.
    #[test]
    fn a_pass_drops_the_phases_that_never_ran() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Flash board", secs(0));
        progress.apply("RESULT: PASS", secs(2));
        assert!(
            progress.rows().iter().all(|row| row.state == PhaseState::Done),
            "{:?}",
            progress.rows(),
        );
    }

    /// A non-zero exit with no verdict line still ends the run, in the
    /// phase it was in.
    #[test]
    fn a_dead_child_fails_the_running_phase() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Compile firmware", secs(0));
        progress.fail(secs(4));
        assert_eq!(progress.verdict(), Some(false));
        assert_eq!(progress.rows()[0].state, PhaseState::Failed);
        assert_eq!(progress.rows()[0].elapsed(secs(9)), secs(4));
    }

    /// `--skip-pnr` skips phases. A skipped one does not sit above the
    /// running phase pretending it is still coming.
    #[test]
    fn a_skipped_phase_drops_out_of_the_list() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Compile firmware", secs(0));
        progress.apply("==> Flash board", secs(9));
        let titles: Vec<&str> = progress.rows().iter().map(|r| r.title.as_str()).collect();
        assert!(
            !titles.contains(&"Provision SD card"),
            "the skipped phase is gone: {titles:?}",
        );
        assert_eq!(titles[0], "Compile firmware");
        assert_eq!(titles[1], "Flash board");
        assert!(titles.contains(&"Report"), "what is still to come stays");
    }

    /// A phase this fork has never heard of is still shown, where it
    /// happened -- the CLI's phase list is not ours to freeze.
    #[test]
    fn an_unknown_phase_is_inserted_where_it_ran() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Compile firmware", secs(0));
        progress.apply("==> Sacrifice a goat", secs(2));
        let titles: Vec<&str> = progress.rows().iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles[1], "Sacrifice a goat");
        assert_eq!(titles[2], "Provision SD card", "the known ones still follow");
        assert_eq!(progress.rows()[1].state, PhaseState::Running);
    }

    /// A setup run has no `==>` output at all, so the panel names each
    /// step through the same door the parser uses.
    #[test]
    fn a_named_step_advances_the_list_without_any_output() {
        let mut progress = FlashProgress::steps(vec![
            "clone the GGO repo".to_string(),
            "install ggo-diag".to_string(),
        ]);
        progress.advance_to("clone the GGO repo", secs(0));
        progress.advance_to("install ggo-diag", secs(6));
        assert_eq!(progress.rows()[0].state, PhaseState::Done);
        assert_eq!(progress.rows()[0].elapsed(secs(6)), secs(6));
        assert_eq!(progress.rows()[1].state, PhaseState::Running);
    }

}
