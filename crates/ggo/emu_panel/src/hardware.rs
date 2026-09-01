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
pub use ggo_emu_remote::protocol::FlashConfig;

use crate::menu::{DEFAULT_DIAG_BIN, DIAG_BIN_ENV, DIAG_REPO_ENV, DIAG_TTY_ENV, SERIAL_BY_ID_DIR};

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
    /// The board is on the USB bus but its serial driver is detached and
    /// the rescue could not bring it back -- replugging is what is left.
    PortStuck,
    /// No project open, so there is no game to pack.
    Project,
}

impl Missing {
    /// What the status row says, in the fork's "name the gap" style.
    pub fn label(self) -> String {
        match self {
            Missing::Repo => {
                format!("no GGO repo checkout -- set {DIAG_REPO_ENV}, or let ZedGG clone one")
            }
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
            Missing::PortStuck => format!(
                "the board is on USB but its serial driver is detached and could not \
                 be reattached -- unplug and replug the board, or set {DIAG_TTY_ENV}"
            ),
            Missing::Project => "no game project is open".to_string(),
        }
    }

    /// The wire code for [`Self::label`]'s prose.
    pub fn code(self) -> &'static str {
        match self {
            Missing::Repo => "repo",
            Missing::Diag => "diag",
            Missing::Emd => "emd",
            Missing::Port => "port",
            Missing::PortStuck => "port_stuck",
            Missing::Project => "project",
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
    /// `ports` is empty but the board IS on the USB bus with its serial
    /// driver detached, and the rescue did not bring a tty back. Changes
    /// what the empty scan is called: "replug the board", not "connect
    /// the board".
    pub stuck_board: bool,
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
    /// `repo`'s HEAD commit, when it can be read off the filesystem. This
    /// is the source the board's gateware and GemOS get built from.
    pub repo_commit: Option<String>,
    /// The commit the in-IDE emulator engine was compiled from, when the
    /// build embedded one.
    pub emu_commit: Option<String>,
    /// Whether [`Self::emu_commit`] exists in [`Self::repo`]'s object
    /// database, asked of git itself when the two commits diverge.
    /// Disambiguates the skew banner's remedy: a commit the repo has
    /// never seen cannot be pulled -- it was built from unpushed work --
    /// while a known commit means the repo has moved past the emulator.
    /// `None` when there is no skew, no git, or git could not be asked.
    pub emu_commit_in_repo: Option<bool>,
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
            missing.push(if self.stuck_board {
                Missing::PortStuck
            } else {
                Missing::Port
            });
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

    /// The flash source and the emulator engine are at different commits:
    /// `(flash_short, emu_short)`.
    ///
    /// The board renders whatever PPU the checkout in [`Self::repo`]
    /// builds, while the in-IDE emulator renders the one it was compiled
    /// from. Divergence is silent -- a board a thousand commits behind
    /// just draws an older frame -- so the page has to say it. Only ever
    /// `Some` when BOTH commits are known: an unreadable repo or a build
    /// without git is "unknown", and warning on unknown would fire on
    /// every machine that has neither.
    pub fn version_skew(&self) -> Option<(String, String)> {
        let (repo_commit, emu_commit) = (self.repo_commit.as_ref()?, self.emu_commit.as_ref()?);
        (repo_commit != emu_commit).then(|| (short_commit(repo_commit), short_commit(emu_commit)))
    }

    /// This probe as the agent socket reports it.
    pub fn remote_payload(&self) -> ggo_emu_remote::protocol::HwEnvPayload {
        use ggo_emu_remote::protocol::{HwEnvPayload, HwMissing};
        HwEnvPayload {
            ready: self.ready(),
            missing: self
                .missing()
                .into_iter()
                .map(|missing| HwMissing { code: missing.code().to_string(), label: missing.label() })
                .collect(),
            ports: self.ports.clone(),
            stuck_board: self.stuck_board,
            project: self.project.as_ref().map(|path| path.display().to_string()),
            repo: self.repo.as_ref().map(|path| path.display().to_string()),
            diag_bin: self.diag_bin.clone(),
            emd_bin: self.emd_bin.clone(),
            version_skew: self.version_skew(),
            emu_commit_in_repo: self.emu_commit_in_repo,
        }
    }

    /// Is [`Self::repo`] the clone this feature made, rather than a
    /// checkout the user maintains?
    pub fn repo_is_managed_clone(&self) -> bool {
        self.repo.as_ref() == Some(&self.clone_dest)
    }

    /// Bring the managed clone AND the installed `ggo-emu` binary to the
    /// GGO remote's head, as one shell script through the streaming
    /// runner.
    ///
    /// Only the clone under `~/.ggo` is ours to move: a dev checkout is
    /// the user's working copy, and pulling it out from under them could
    /// discard work or drop them onto a different branch mid-edit.
    ///
    /// The script tries `git pull --ff-only` first, and ANY failure --
    /// diverged history, corrupt objects, a rebased upstream -- falls
    /// back to deleting the clone and cloning fresh, which is safe
    /// precisely because the clone is managed: nothing in it is the
    /// user's to lose. `cargo install` then rebuilds `ggo-emu` from the
    /// synced source, skipped only when the sync moved nothing and a
    /// `ggo-emu` is already on PATH -- an install takes minutes and a
    /// no-op sync should not.
    ///
    /// One `sh -c` request rather than several `ProcRequest`s because the
    /// runner stops at the first failure, and "reclone" is only correct
    /// AFTER the pull has failed.
    pub fn sync_request(&self) -> Option<ProcRequest> {
        if !self.git || !self.repo_is_managed_clone() {
            return None;
        }
        let repo = shell_quote(&self.repo.clone()?);
        let url = GGO_REPO_URL;
        let script = format!(
            "set -e\n\
             pre=$(git -C {repo} rev-parse HEAD 2>/dev/null || echo none)\n\
             git -C {repo} pull --ff-only --progress || {{ rm -rf {repo}; git clone --progress {url} {repo}; }}\n\
             post=$(git -C {repo} rev-parse HEAD)\n\
             if [ \"$pre\" != \"$post\" ] || ! command -v ggo-emu >/dev/null; then\n\
                 cargo install --locked --path {repo}/tools/ggo-emu\n\
             else\n\
                 echo \"ggo-emu already matches $post\"\n\
             fi"
        );
        Some(ProcRequest::new(
            "sh",
            self.cwd_for_setup(),
            vec!["-c".to_string(), script],
        ))
    }
}

/// `path`, single-quoted for `sh -c`. Single quotes disable every shell
/// expansion, and an embedded quote is spliced as `'\''`.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Days a board-run transcript is kept before [`prune_run_logs`] deletes
/// it, overridable via [`RUN_LOG_TTL_DAYS_ENV`].
pub const DEFAULT_RUN_LOG_TTL_DAYS: u64 = 7;
pub const RUN_LOG_TTL_DAYS_ENV: &str = "ZED_GGO_RUN_LOG_TTL_DAYS";

/// The prefix every board-run transcript's filename carries. The prune
/// only ever touches files matching it: `~/.zed/logs` is not this
/// feature's directory to clean.
pub const RUN_LOG_PREFIX: &str = "ggo-run-";

/// Where board-run transcripts land: `~/.zed/logs`.
pub fn run_log_dir(home: &Path) -> PathBuf {
    home.join(".zed").join("logs")
}

/// `ggo-run-<stamp>-<what>.log`, with `what` reduced to filename-safe
/// characters.
pub fn run_log_name(what: &str, now: chrono::DateTime<chrono::Local>) -> String {
    let slug: String = what
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!(
        "{RUN_LOG_PREFIX}{}-{}.log",
        now.format("%Y%m%d-%H%M%S"),
        slug.trim_matches('-')
    )
}

/// Delete run transcripts in `dir` older than `ttl` (by mtime). Only
/// files named [`RUN_LOG_PREFIX`]`*.log` are candidates. Errors are
/// logged, not propagated: a transcript that cannot be pruned must never
/// stop the run that wanted to write a new one.
pub fn prune_run_logs(dir: &Path, ttl: Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(RUN_LOG_PREFIX) || !name.ends_with(".log") {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > ttl);
        if expired {
            if let Err(error) = std::fs::remove_file(entry.path()) {
                log::warn!("pruning old run log {name}: {error}");
            }
        }
    }
}

/// The configured transcript lifetime: [`RUN_LOG_TTL_DAYS_ENV`] in days,
/// else [`DEFAULT_RUN_LOG_TTL_DAYS`].
pub fn run_log_ttl() -> Duration {
    let days = std::env::var(RUN_LOG_TTL_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RUN_LOG_TTL_DAYS);
    Duration::from_secs(days * 24 * 60 * 60)
}

/// Open a transcript file for a run named `what`, pruning expired ones
/// first. `None` -- with a warning -- when the directory or file cannot
/// be made: a run must never be blocked by its own paper trail.
pub fn create_run_log(what: &str) -> Option<(PathBuf, std::fs::File)> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    let dir = run_log_dir(&home);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::warn!("creating {}: {error}", dir.display());
        return None;
    }
    prune_run_logs(&dir, run_log_ttl());
    let path = dir.join(run_log_name(what, chrono::Local::now()));
    match std::fs::File::create(&path) {
        Ok(file) => Some((path, file)),
        Err(error) => {
            log::warn!("creating run log {}: {error}", path.display());
            None
        }
    }
}

/// The first 10 characters of a commit hash -- long enough to be unique
/// in any repo this fork touches, short enough for a status line.
fn short_commit(commit: &str) -> String {
    commit.chars().take(10).collect()
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
                    (None, true, false) => {
                        Remedy::Install(format!("git clone into {}", self.clone_dest.display()))
                    }
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
                    None if self.stuck_board => {
                        Remedy::Manual(Missing::PortStuck.label())
                    }
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
/// never needs new gateware. A GGO repo update that changes the PPU/SoC
/// DOES need new gateware, which is what `rebuild_gateware` is for: it
/// drops `--skip-pnr` so the run place-and-routes and flashes a fresh
/// bitstream instead of the cached one. `--project` implies
/// `--provision`, so the card image is rewritten with the freshly packed
/// game every run.
///
/// `world` is the stem (`worlds/arena`) the packed cart should boot,
/// overriding the project's `default_world`. Without it the board boots
/// whatever world the manifest names -- which is never the one the IDE
/// was just editing, and reads on hardware as "my world is broken".
///
/// `--world` needs a `ggo-diag` built from a revision that HAS the flag:
/// an older binary on `PATH` exits 2 on the unknown argument rather than
/// ignoring it, and the transcript on the hardware page is where that
/// shows up. The remedy is the page's own install/update buttons.
pub fn flash_args(project: &Path, tty: &str, config: &FlashConfig) -> Vec<String> {
    let mut args = vec![
        "--project".to_string(),
        project.to_string_lossy().into_owned(),
        "--tty".to_string(),
        tty.to_string(),
    ];
    if let Some(world) = &config.world {
        args.push("--world".to_string());
        args.push(world.clone());
    }
    if !config.rebuild_gateware {
        args.push("--skip-pnr".to_string());
    }
    // Unset knobs are NOT passed: ggo-diag's own default rules, whatever
    // the mirrored constants say.
    if let Some(baud) = config.baud {
        args.push("--baud".to_string());
        args.push(baud.to_string());
    }
    if let Some(seconds) = config.collect_seconds {
        args.push("--collect-seconds".to_string());
        args.push(seconds.to_string());
    }
    if config.telemetry {
        args.push("--telemetry".to_string());
    }
    args
}

/// `config` with every default filled in the way `env` would fill it:
/// the serial port the scan found, ggo-diag's baud and capture window.
/// What the agent is told a flash it started actually runs with.
pub fn effective_config(env: &HardwareEnv, config: &FlashConfig) -> FlashConfig {
    FlashConfig {
        world: config.world.clone(),
        rebuild_gateware: config.rebuild_gateware,
        tty: config.tty.clone().or_else(|| env.ports.first().cloned()),
        baud: Some(config.baud.unwrap_or(ggo_emu_remote::protocol::DEFAULT_BAUD)),
        collect_seconds: Some(
            config
                .collect_seconds
                .unwrap_or(ggo_emu_remote::protocol::DEFAULT_COLLECT_SECONDS),
        ),
        telemetry: config.telemetry,
    }
}

/// The flash invocation, or the list of what is missing instead.
///
/// `cwd` is the GGO repo: that CLI finds the repo by walking up from its
/// working directory, and this fork's worktree is the user's GAME
/// project, not the repo.
pub fn flash_request(env: &HardwareEnv, config: &FlashConfig) -> Result<ProcRequest, String> {
    // A port named by the caller stands in for the scan's: the scan is
    // how the panel finds one, not a rule that only scanned ports count.
    // A STUCK board (`Missing::PortStuck`) is still a blocker: its driver
    // is detached, so no name would open.
    let missing: Vec<Missing> = env
        .missing()
        .into_iter()
        .filter(|missing| !(config.tty.is_some() && *missing == Missing::Port))
        .collect();
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
        config.tty.clone().or_else(|| env.ports.first().cloned()),
    ) else {
        return Err("flashing needs a board".to_string());
    };
    // The child gets every knob explicitly, so what `effective_config`
    // reports is what runs even if ggo-diag's own defaults move.
    Ok(ProcRequest::new(bin, repo, flash_args(&project, &tty, &effective_config(env, config))))
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
    /// `  [boot] <stage>` (optionally ` — <detail>`), detail kept: it
    /// carries the next stage's budget, the only "how long should this
    /// take" a boot verify has.
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
        return Some(Stage::Boot(rest.trim().to_string()));
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

/// The diag run id from `ggo-diag`'s `[db] run <id>: …` persistence line --
/// the TEXT key everything about the finished hardware run is looked up by,
/// including its perf data once `diag_db::clone_runs` has copied that into
/// `~/.ggo/ggo_ide.db` under `run.label`.
///
/// NOT the `[db] device run <n> (…)` line printed beside it: that `<n>` is
/// an INTEGER id in `diag.db`'s own perf tables, a different id space from
/// both this one and the local `run.id` the charts panel opens. The
/// `"[db] run "` prefix is what keeps the two apart, so it is matched
/// whole rather than by a looser "starts with `[db]`" test.
pub fn parse_db_run_id(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("[db] run ")?;
    let (id, _) = rest.split_once(':')?;
    Some(id.trim().to_string())
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

/// [`FLASH_PHASES`] plus the place-and-route phases a run without
/// `--skip-pnr` announces (titles from `ggo-diag`'s `Phase5::title`).
pub const FULL_FLASH_PHASES: [&str; 7] = [
    "Compile firmware",
    "Component PnR",
    "Full SoC PnR",
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
    /// The run `ggo-diag` recorded this flash under, from its `[db] run …`
    /// line (see [`parse_db_run_id`]). `None` for a run that recorded
    /// nothing -- a setup or `git pull` run, a flash that died before the
    /// Report phase, or a `ggo-diag` too old to print the line -- and that
    /// is what the page's post-PASS hop to the run's report keys off.
    pub diag_run_id: Option<String>,
    /// What is being run ("flashing worlds/chase_cam"), for the agent
    /// socket, which has no timeline header to read it off.
    pub what: Option<String>,
    /// The run's transcript on disk, once [`create_run_log`] made one.
    pub transcript: Option<PathBuf>,
    /// Why the run failed, in the child's own words. Set when the run
    /// is retired, never while it is live.
    pub failure: Option<String>,
    /// How many console lines predate this run: the console is shared
    /// across runs, and the agent's tail must not show the last one's.
    pub console_from: usize,
}

impl FlashProgress {
    /// A flash run: the expected pipeline, all pending.
    pub fn flash() -> Self {
        Self::steps(FLASH_PHASES.iter().map(|title| title.to_string()).collect())
    }

    /// A gateware-rebuilding flash run: the pipeline with both PnR
    /// phases, all pending.
    pub fn flash_full() -> Self {
        Self::steps(
            FULL_FLASH_PHASES
                .iter()
                .map(|title| title.to_string())
                .collect(),
        )
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
            ..Self::default()
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
        self.rows
            .iter()
            .find(|row| row.state == PhaseState::Running)
    }

    /// Where the run is, or -- once it has ended -- how far it got: the
    /// running row, else the last one that started. What the page shows
    /// as a highlighted line, and the only phase word a caller with no
    /// page (the agent socket) can be given.
    pub fn current_phase(&self) -> Option<&PhaseRow> {
        self.running().or_else(|| {
            self.rows
                .iter()
                .rev()
                .find(|row| row.state != PhaseState::Pending)
        })
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
        // Before the stage guard, because the persistence line is not a
        // stage: it moves no phase, it only names the run. First match
        // wins -- one run records itself once, and a second `[db] run`
        // line could only be a later run's, which is not this timeline's.
        if self.diag_run_id.is_none() {
            self.diag_run_id = parse_db_run_id(line);
        }
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

/// `repo`'s HEAD commit, read straight off the filesystem.
///
/// No `git rev-parse`: this is called from `probe`, which runs on the
/// foreground thread, and spawning a child there to answer a question
/// three files can answer is a stall for a hash. It also keeps the
/// function pure enough to test against a fixture tree.
///
/// Everything about a repo we cannot read is `None` rather than an error:
/// the one caller ([`HardwareEnv::version_skew`]) treats unknown as "do
/// not warn", so a shallow archive, a `.git` in a shape this does not
/// handle, or no repo at all all land in the same harmless place.
pub fn read_git_head(repo: &Path) -> Option<String> {
    let dot_git = repo.join(".git");
    // A worktree (and a submodule) has `.git` as a FILE holding
    // `gitdir: <path>`, where the path may be relative to the worktree.
    let git_dir = if dot_git.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = Path::new(pointer.trim().strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            repo.join(target)
        }
    } else {
        dot_git
    };
    // HEAD is per-worktree, so it always comes from the gitdir above.
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    // Detached: HEAD is the hash itself.
    let Some(reference) = head.strip_prefix("ref:") else {
        return is_commit_hash(head).then(|| head.to_string());
    };
    let reference = reference.trim();
    // `refs/heads/*` are NOT per-worktree: a linked worktree's gitdir
    // holds only its own HEAD and index, and `commondir` points at the
    // main `.git` where the branches actually live. Without this hop a
    // `git worktree` checkout resolves to nothing at all.
    let common = read_common_dir(&git_dir);
    // A loose ref file is the usual shape -- including for a freshly
    // cloned repo, whose checked-out branch is written loose while
    // `packed-refs` carries the remotes and tags. `git gc` is what later
    // packs heads away too, and a repo that has not committed since then
    // has its branch ONLY there.
    read_loose_ref(&git_dir, reference)
        .or_else(|| read_packed_ref(&git_dir, reference))
        .or_else(|| {
            let common = common?;
            read_loose_ref(&common, reference).or_else(|| read_packed_ref(&common, reference))
        })
}

/// The main `.git` a linked worktree's gitdir points back to, named by
/// its `commondir` file (usually the relative `../..`). `None` for an
/// ordinary repo, which has no such file and needs no hop.
fn read_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let pointer = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let target = Path::new(pointer.trim());
    if target.as_os_str().is_empty() {
        return None;
    }
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        git_dir.join(target)
    })
}

/// `.git/<reference>`, when it holds a hash.
fn read_loose_ref(git_dir: &Path, reference: &str) -> Option<String> {
    let hash = std::fs::read_to_string(git_dir.join(reference)).ok()?;
    let hash = hash.trim();
    is_commit_hash(hash).then(|| hash.to_string())
}

/// `reference`'s hash out of `.git/packed-refs`, whose lines are
/// `<hash> <ref>`.
fn read_packed_ref(git_dir: &Path, reference: &str) -> Option<String> {
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        // `#` is the header comment; `^<hash>` peels the PREVIOUS line's
        // annotated tag to its commit and names no ref of its own.
        if line.starts_with('#') || line.starts_with('^') {
            return None;
        }
        let (hash, name) = line.split_once(' ')?;
        (name.trim() == reference && is_commit_hash(hash)).then(|| hash.to_string())
    })
}

/// A hex object id: 40 characters for SHA-1, 64 for a SHA-256 repo.
fn is_commit_hash(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
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

// ------------------------------------------------- orphaned-board rescue
//
// The flash pipeline's `fujprog` talks to the FT231X over libusb, which
// detaches the `ftdi_sio` kernel driver to claim the interface -- and on
// exit nothing reattaches it. The board is still on the bus, but
// `/dev/ttyUSB0` (and the `/dev/serial/by-id` symlink) are gone until the
// user replugs, so the very next flash says "connect the board" at a board
// that is connected. The rescue finds that state on sysfs and reattaches
// the driver through usbfs's USBDEVFS_CONNECT ioctl -- the exact inverse
// of what libusb did, and it needs no root: the udev `uaccess` ACL that
// let fujprog claim the device covers the reattach too.

/// Where Linux lists USB devices and their interfaces as siblings
/// (`1-8` the device, `1-8:1.0` its interface).
pub const USB_SYSFS_DIR: &str = "/sys/bus/usb/devices";

/// The USB IDs the rescue recognizes as a board: the ULX3S's FT231X.
const BOARD_USB_IDS: [(&str, &str); 1] = [("0403", "6015")];

/// A board that is on the USB bus with no driver bound to its serial
/// interface -- what a libusb flash tool leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedBoard {
    pub busnum: u32,
    pub devnum: u32,
    pub interface_number: i32,
}

/// A board on the bus whose serial interface has no driver, or `None`.
/// Parametrized over the sysfs directory so it is testable against a
/// fixture tree; an unreadable directory (any non-Linux host) is simply
/// "no orphan".
pub fn find_orphaned_board_at(usb_dir: &Path) -> Option<OrphanedBoard> {
    let read = |dir: &Path, name: &str| -> Option<String> {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .map(|value| value.trim().to_string())
    };
    for entry in std::fs::read_dir(usb_dir).ok()?.flatten() {
        let device = entry.path();
        let (Some(vendor), Some(product)) = (read(&device, "idVendor"), read(&device, "idProduct"))
        else {
            continue;
        };
        if !BOARD_USB_IDS
            .iter()
            .any(|(v, p)| *v == vendor && *p == product)
        {
            continue;
        }
        let (Some(busnum), Some(devnum)) = (
            read(&device, "busnum").and_then(|v| v.parse().ok()),
            read(&device, "devnum").and_then(|v| v.parse().ok()),
        ) else {
            continue;
        };
        // Interfaces sit beside the device, named `<device>:<config>.<n>`.
        let prefix = format!("{}:", entry.file_name().to_string_lossy());
        for interface in std::fs::read_dir(usb_dir).ok()?.flatten() {
            let name = interface.file_name().to_string_lossy().into_owned();
            let Some(interface_number) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.split_once('.'))
                .and_then(|(_, number)| number.parse().ok())
            else {
                continue;
            };
            if !interface.path().join("driver").exists() {
                return Some(OrphanedBoard {
                    busnum,
                    devnum,
                    interface_number,
                });
            }
        }
    }
    None
}

/// Reattach the kernel serial driver to `board`'s interface, undoing a
/// flash tool's libusb detach. Sends USBDEVFS_CONNECT through the usbfs
/// device node, which is what `libusb_attach_kernel_driver` does.
#[cfg(target_os = "linux")]
pub fn reattach_kernel_driver(board: &OrphanedBoard) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct UsbdevfsIoctl {
        ifno: libc::c_int,
        ioctl_code: libc::c_int,
        data: *mut libc::c_void,
    }
    // _IO('U', 23): USBDEVFS_CONNECT, carried inside _IOWR('U', 18,
    // usbdevfs_ioctl) -- computed from the struct size rather than
    // hardcoded so 32-bit pointers get the request number they need.
    const USBDEVFS_CONNECT: libc::c_int = 0x5517;
    let request: libc::c_ulong = (3 << 30)
        | ((std::mem::size_of::<UsbdevfsIoctl>() as libc::c_ulong) << 16)
        | (0x55 << 8)
        | 18;
    let node = format!("/dev/bus/usb/{:03}/{:03}", board.busnum, board.devnum);
    let file = std::fs::OpenOptions::new().write(true).open(&node)?;
    let mut command = UsbdevfsIoctl {
        ifno: board.interface_number,
        ioctl_code: USBDEVFS_CONNECT,
        data: std::ptr::null_mut(),
    };
    if unsafe { libc::ioctl(file.as_raw_fd(), request, &mut command) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn reattach_kernel_driver(_board: &OrphanedBoard) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "kernel-driver reattach is Linux-only",
    ))
}

/// True while a board run is in flight, set by the panel around every
/// run and cleared when the run's handle drops. Global rather than
/// threaded through, because port scans happen from places with no path
/// to the panel (menu building, probes from render) and EVERY one of
/// them must honor it.
static BOARD_RUN_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_board_run_in_flight(in_flight: bool) {
    BOARD_RUN_IN_FLIGHT.store(in_flight, std::sync::atomic::Ordering::Relaxed);
}

/// The serial scan, with one rescue attempt when it comes up empty.
/// Returns the ports and whether a board is stuck on the bus: present
/// with its driver detached, and the rescue did not bring a tty back.
pub fn scan_ports_rescuing() -> (Vec<String>, bool) {
    let by_id = Path::new(SERIAL_BY_ID_DIR);
    let dev = Path::new(crate::menu::DEV_DIR);
    let scan = || crate::menu::scan_serial_ports_at(by_id, dev);
    let ports = scan();
    if !ports.is_empty() {
        return (ports, false);
    }
    let Some(board) = find_orphaned_board_at(Path::new(USB_SYSFS_DIR)) else {
        return (ports, false);
    };
    // A board with its driver detached DURING a run is not orphaned --
    // it is openFPGALoader holding the USB interface to program it, and
    // reattaching the kernel driver here rips the device out from under
    // the write (which is exactly how a 20-minute flash died at 100%).
    // Not stuck either: the run gives the port back when it ends.
    if BOARD_RUN_IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed) {
        log::info!(
            "board on USB {:03}/{:03} has no serial driver, but a run is in flight -- \
             leaving it alone",
            board.busnum,
            board.devnum
        );
        return (ports, false);
    }
    if let Err(error) = reattach_kernel_driver(&board) {
        log::warn!(
            "board on USB {:03}/{:03} has no serial driver, and reattaching failed: {error}",
            board.busnum,
            board.devnum
        );
        return (ports, true);
    }
    // The reattach kicks off the driver probe rather than completing it;
    // the tty node follows within milliseconds. Bounded, so a probe on
    // the foreground thread stalls a frame or two at worst, once.
    for _ in 0..5 {
        let ports = scan();
        if !ports.is_empty() {
            log::info!("reattached the board's serial driver: {}", ports[0]);
            return (ports, false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    (Vec::new(), true)
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
    let (ports, stuck_board) = match std::env::var(DIAG_TTY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(tty) => (vec![tty], false),
        None => scan_ports_rescuing(),
    };
    let repo_commit = repo.as_deref().and_then(read_git_head);
    let emu_commit = ggo_emu_core::BUILT_FROM_COMMIT.map(str::to_string);
    // Only asked when the banner will show: probe runs on the foreground
    // thread, and one short-lived `git cat-file` on the cached path is
    // the whole cost of a remedy that points the right way.
    let emu_commit_in_repo = match (&repo, &repo_commit, &emu_commit) {
        (Some(repo), Some(repo_commit), Some(emu_commit)) if repo_commit != emu_commit => {
            commit_in_repo(repo, emu_commit)
        }
        _ => None,
    };
    HardwareEnv {
        diag_bin,
        emd_bin,
        repo,
        emerald,
        ports,
        stuck_board,
        // The subject is an EMERALD project, not merely an open folder:
        // `ggo-diag --project` hands it to `emd pack-ggo`, which fails
        // deep inside itself on a folder that is not one.
        project: project.and_then(ggo_common::emerald_project_root),
        cargo: resolve_on_path("cargo", path_env).is_some(),
        git: resolve_on_path("git", path_env).is_some(),
        clone_dest,
        home: home.to_path_buf(),
        repo_commit,
        emu_commit,
        emu_commit_in_repo,
    }
}

/// Does `commit` exist in `repo`'s object database? `None` when git could
/// not be run at all; a clean "no" from git is `Some(false)`.
fn commit_in_repo(repo: &Path, commit: &str) -> Option<bool> {
    // `output()`, not `status()`: a missing object makes `cat-file -e`
    // print to stderr, which is noise on every probe of a skewed repo.
    // Blocking is `probe`'s own contract -- it already walks `/dev` and
    // sleeps in its port rescue -- and `cat-file -e` neither touches the
    // network nor takes locks.
    #[allow(clippy::disallowed_methods)]
    std::process::Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-e", commit])
        .output()
        .ok()
        .map(|output| output.status.success())
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
            stuck_board: false,
            project: Some(PathBuf::from("/game")),
            cargo: true,
            git: true,
            clone_dest: PathBuf::from("/home/u/.ggo/ggo"),
            home: PathBuf::from("/home/u"),
            repo_commit: None,
            emu_commit: None,
            emu_commit_in_repo: None,
        }
    }

    /// A repo fixture: `.git/HEAD` holding `head`, plus whatever loose
    /// refs and `packed-refs` body the test wants.
    fn git_fixture(
        root: &Path,
        head: &str,
        loose: &[(&str, &str)],
        packed: Option<&str>,
    ) -> PathBuf {
        let repo = root.join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), format!("{head}\n")).unwrap();
        for (name, hash) in loose {
            let path = git_dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{hash}\n")).unwrap();
        }
        if let Some(packed) = packed {
            std::fs::write(git_dir.join("packed-refs"), packed).unwrap();
        }
        repo
    }

    #[test]
    fn flash_args_pack_the_project_and_skip_place_and_route() {
        assert_eq!(
            flash_args(Path::new("/game"), "/dev/ttyUSB0", &FlashConfig::default()),
            vec!["--project", "/game", "--tty", "/dev/ttyUSB0", "--skip-pnr"],
        );
    }

    #[test]
    fn a_gateware_rebuild_does_not_skip_place_and_route() {
        let config = FlashConfig { rebuild_gateware: true, ..Default::default() };
        assert_eq!(
            flash_args(Path::new("/game"), "/dev/ttyUSB0", &config),
            vec!["--project", "/game", "--tty", "/dev/ttyUSB0"],
        );
    }

    /// Every knob reaches the child, and an unset one passes no flag at
    /// all -- ggo-diag's default is ggo-diag's to keep.
    #[test]
    fn every_set_knob_is_a_flag_and_every_unset_one_is_absent() {
        let config = FlashConfig {
            world: None,
            rebuild_gateware: false,
            tty: Some("/dev/ttyUSB3".into()),
            baud: Some(9600),
            collect_seconds: Some(30),
            telemetry: true,
        };
        assert_eq!(
            flash_args(Path::new("/game"), "/dev/ttyUSB3", &config),
            vec![
                "--project",
                "/game",
                "--tty",
                "/dev/ttyUSB3",
                "--skip-pnr",
                "--baud",
                "9600",
                "--collect-seconds",
                "30",
                "--telemetry"
            ],
        );
        let args = flash_args(Path::new("/game"), "/dev/ttyUSB0", &FlashConfig::default());
        for flag in ["--baud", "--collect-seconds", "--telemetry", "--world"] {
            assert!(!args.contains(&flag.to_string()), "{flag} in {args:?}");
        }
    }

    /// The effective configuration names the port the scan found and
    /// ggo-diag's defaults, so the agent learns what its flash runs with.
    #[test]
    fn the_effective_config_fills_the_scanned_port_and_the_diag_defaults() {
        let effective = effective_config(&ready_env(), &FlashConfig::default());
        assert_eq!(effective.tty.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(effective.baud, Some(115_200));
        assert_eq!(effective.collect_seconds, Some(120));
        let named = FlashConfig { tty: Some("/dev/ttyUSB7".into()), baud: Some(9600), ..Default::default() };
        let effective = effective_config(&ready_env(), &named);
        assert_eq!(effective.tty.as_deref(), Some("/dev/ttyUSB7"));
        assert_eq!(effective.baud, Some(9600));
    }

    /// A caller-named port is a port: the scan finding none is no longer
    /// a missing prerequisite.
    #[test]
    fn a_named_tty_stands_in_for_the_scan() {
        let mut env = ready_env();
        env.ports.clear();
        assert!(flash_request(&env, &FlashConfig::default()).is_err());
        let request = flash_request(&env, &FlashConfig { tty: Some("/dev/ttyUSB9".into()), ..Default::default() })
            .expect("a named port flashes");
        assert!(request.args.contains(&"/dev/ttyUSB9".to_string()), "{:?}", request.args);
    }

    /// The named world overrides the project's `default_world`, and sits
    /// between the tty and the place-and-route switch in both shapes of
    /// the run.
    #[test]
    fn a_named_world_is_what_the_board_boots() {
        let arena = FlashConfig { world: Some("worlds/arena".into()), ..Default::default() };
        assert_eq!(
            flash_args(Path::new("/game"), "/dev/ttyUSB0", &arena),
            vec![
                "--project",
                "/game",
                "--tty",
                "/dev/ttyUSB0",
                "--world",
                "worlds/arena",
                "--skip-pnr"
            ],
        );
        let arena_full = FlashConfig { rebuild_gateware: true, ..arena };
        assert_eq!(
            flash_args(Path::new("/game"), "/dev/ttyUSB0", &arena_full),
            vec![
                "--project",
                "/game",
                "--tty",
                "/dev/ttyUSB0",
                "--world",
                "worlds/arena"
            ],
        );
    }

    #[test]
    fn flash_request_runs_in_the_repo_not_the_project() {
        let request =
            flash_request(&ready_env(), &FlashConfig::default()).expect("a ready machine flashes");
        assert_eq!(request.bin, "ggo-diag");
        assert_eq!(
            request.cwd,
            PathBuf::from("/repo"),
            "ggo-diag walks up from its cwd to find the repo"
        );
        assert!(request.args.contains(&"/game".to_string()));
        assert!(
            !request.args.contains(&"--world".to_string()),
            "no world named means the project's default_world stands: {:?}",
            request.args
        );
    }

    /// The world reaches the child, not just [`flash_args`].
    #[test]
    fn flash_request_carries_the_world_through() {
        let arena = FlashConfig { world: Some("worlds/arena".into()), ..Default::default() };
        let request = flash_request(&ready_env(), &arena).expect("a ready machine flashes");
        assert_eq!(
            request.args,
            flash_args(Path::new("/game"), "/dev/ttyUSB0", &effective_config(&ready_env(), &arena))
        );
        // Explicit, not left to ggo-diag: the reported set is the run set.
        assert!(request.args.windows(2).any(|w| w == ["--baud", "115200"]), "{:?}", request.args);
        assert!(request.args.windows(2).any(|w| w == ["--collect-seconds", "120"]), "{:?}", request.args);
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
        let error =
            flash_request(&env, &FlashConfig::default()).expect_err("an empty machine cannot flash");
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
    fn the_remote_payload_names_every_missing_prerequisite_by_code() {
        let ready = ready_env().remote_payload();
        assert!(ready.ready && ready.missing.is_empty(), "{ready:?}");
        assert_eq!(ready.ports, vec!["/dev/ttyUSB0".to_string()]);

        let mut env = ready_env();
        env.ports.clear();
        env.stuck_board = true;
        env.emd_bin = None;
        let payload = env.remote_payload();
        assert!(!payload.ready);
        let codes: Vec<&str> = payload.missing.iter().map(|m| m.code.as_str()).collect();
        assert_eq!(codes, ["emd", "port_stuck"]);
        assert!(payload.missing[1].label.contains("replug") || !payload.missing[1].label.is_empty());
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
            parse_stage("  [boot] boot-rom alive — next: SD ready (10s budget)"),
            Some(Stage::Boot("boot-rom alive — next: SD ready (10s budget)".into())),
            "the detail is the stage budget, which the status row shows"
        );
        assert_eq!(
            parse_stage("diag step 3: PASS"),
            Some(Stage::DiagStep {
                index: "3".into(),
                status: "PASS".into()
            })
        );
        assert_eq!(
            parse_stage("RESULT: PASS"),
            Some(Stage::Result { pass: true })
        );
        assert_eq!(
            parse_stage("RESULT: FAIL"),
            Some(Stage::Result { pass: false })
        );
        // Everything else is console-only.
        assert_eq!(parse_stage(""), None);
        assert_eq!(parse_stage("warning: something"), None);
        assert_eq!(parse_stage("<-- component ppu: ok luts=123"), None);
    }

    /// The persistence line names the run everything about the finished
    /// flash is keyed by. The `[db] device run …` line beside it is a
    /// DIFFERENT id space (the perf `run` table's INTEGER id) and must not
    /// be mistaken for it.
    #[test]
    fn parse_db_run_id_reads_ggo_diags_db_line() {
        assert_eq!(
            parse_db_run_id(
                "[db] run 20260831T120000Z-abc123def0: 450 uart lines, 180 frames -> /home/x/.ggo/diag.db"
            ),
            Some("20260831T120000Z-abc123def0".to_string())
        );
        assert_eq!(
            parse_db_run_id(
                "[db] device run 7 (device:slop_battle): 180/180 FRAME packets -> diag.db"
            ),
            None
        );
        assert_eq!(parse_db_run_id("RESULT: PASS"), None);
    }

    /// The timeline carries the run id the transcript announced, so the
    /// retired run still knows which report to open. A line that names no
    /// run leaves it alone, and the first one wins.
    #[test]
    fn a_flash_remembers_the_run_ggo_diag_recorded_it_under() {
        let mut progress = FlashProgress::flash();
        assert_eq!(progress.diag_run_id, None);
        progress.apply("==> Report", Duration::from_secs(1));
        assert_eq!(progress.diag_run_id, None, "a phase names no run");
        progress.apply(
            "[db] run 20260831T120000Z-abc123def0: 450 uart lines, 180 frames -> /x/diag.db",
            Duration::from_secs(2),
        );
        assert_eq!(
            progress.diag_run_id.as_deref(),
            Some("20260831T120000Z-abc123def0")
        );
        progress.apply(
            "[db] run 20260901T000000Z-later00000: 1 uart lines, 0 frames -> /x/diag.db",
            Duration::from_secs(3),
        );
        assert_eq!(
            progress.diag_run_id.as_deref(),
            Some("20260831T120000Z-abc123def0"),
            "the first line is this run's; a second belongs to another"
        );
        assert!(
            FlashProgress::steps(Vec::new()).diag_run_id.is_none(),
            "a setup run records nothing"
        );
    }

    #[test]
    fn stage_labels_read_as_status_text() {
        assert_eq!(Stage::Phase("Flash board".into()).label(), "Flash board");
        assert_eq!(Stage::Component("ppu".into()).label(), "place & route: ppu");
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
    fn a_driverless_board_on_the_bus_is_the_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let usb = dir.path();
        let device = usb.join("1-8");
        std::fs::create_dir_all(&device).unwrap();
        for (name, value) in [
            ("idVendor", "0403"),
            ("idProduct", "6015"),
            ("busnum", "1"),
            ("devnum", "11"),
        ] {
            std::fs::write(device.join(name), format!("{value}\n")).unwrap();
        }
        let interface = usb.join("1-8:1.0");
        std::fs::create_dir_all(&interface).unwrap();
        assert_eq!(
            find_orphaned_board_at(usb),
            Some(OrphanedBoard {
                busnum: 1,
                devnum: 11,
                interface_number: 0
            }),
        );

        // Driver bound: a healthy board is not an orphan.
        std::fs::write(interface.join("driver"), b"").unwrap();
        assert_eq!(find_orphaned_board_at(usb), None);

        // Some other vendor's driverless device is not the board.
        let other = usb.join("1-9");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("idVendor"), "1b1c\n").unwrap();
        std::fs::write(other.join("idProduct"), "0c29\n").unwrap();
        std::fs::create_dir_all(usb.join("1-9:1.0")).unwrap();
        assert_eq!(find_orphaned_board_at(usb), None);

        // No sysfs at all (any non-Linux host): no orphan, no error.
        assert_eq!(find_orphaned_board_at(&usb.join("gone")), None);
    }

    /// The whole rescue against a real board: detach its serial driver
    /// first (`fujprog` does; so does usbfs USBDEVFS_DISCONNECT), then
    /// run `cargo test -p ggo_emu_panel --lib -- --ignored rescued`.
    #[test]
    #[ignore = "needs the ULX3S attached"]
    fn a_live_orphaned_board_is_rescued() {
        let (ports, stuck) = scan_ports_rescuing();
        assert!(!stuck, "board on the bus but the rescue failed");
        assert!(!ports.is_empty(), "no board attached, or no tty came back");
    }

    #[test]
    fn a_stuck_board_is_named_differently_from_a_missing_one() {
        let mut env = ready_env();
        env.ports.clear();
        env.stuck_board = true;
        assert_eq!(env.missing(), vec![Missing::PortStuck]);
        assert!(!Missing::PortStuck.installable());
        assert!(
            Missing::PortStuck.label().contains("replug"),
            "the remedy is a replug, not a connect: {}",
            Missing::PortStuck.label()
        );
        let requirements = env.requirements();
        let board_row = requirements
            .last()
            .expect("the board row is the last requirement");
        match &board_row.remedy {
            Remedy::Manual(what) => assert!(what.contains("replug"), "{what}"),
            other => panic!("expected a manual remedy, got {other:?}"),
        }
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
        assert_eq!(
            resolve_on_path("ggo-diag", None),
            None,
            "no PATH, no lookup"
        );
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
        assert!(matches!(env.requirements()[1].remedy, Remedy::Install(_)));
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
        assert_eq!(
            expand_home("~/.ggo/ggo", home),
            PathBuf::from("/home/u/.ggo/ggo")
        );
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

    // ------------------------------------------------- version skew

    const HEAD_HASH: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_HASH: &str = "fedcba9876543210fedcba9876543210fedcba98";

    /// A detached HEAD names its commit directly.
    #[test]
    fn a_detached_head_is_the_commit_itself() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_fixture(dir.path(), HEAD_HASH, &[], None);
        assert_eq!(read_git_head(&repo), Some(HEAD_HASH.to_string()));
    }

    /// The usual shape: HEAD points at a branch, the branch is a file.
    #[test]
    fn a_symbolic_head_resolves_through_the_loose_ref() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_fixture(
            dir.path(),
            "ref: refs/heads/main",
            &[("refs/heads/main", HEAD_HASH)],
            None,
        );
        assert_eq!(read_git_head(&repo), Some(HEAD_HASH.to_string()));
    }

    /// A freshly cloned (or `git gc`'d) repo keeps its refs packed, with
    /// a comment header and tag peel lines that name no branch.
    #[test]
    fn a_packed_ref_resolves_past_comments_and_peel_lines() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_fixture(
            dir.path(),
            "ref: refs/heads/main",
            &[],
            Some(&format!(
                "# pack-refs with: peeled fully-peeled sorted \n\
                 {OTHER_HASH} refs/tags/v1\n\
                 ^{HEAD_HASH}\n\
                 {HEAD_HASH} refs/heads/main\n"
            )),
        );
        assert_eq!(
            read_git_head(&repo),
            Some(HEAD_HASH.to_string()),
            "the peel line under the tag is not refs/heads/main"
        );
    }

    /// A loose ref beats the packed one: `packed-refs` can be stale for a
    /// branch that has moved since the last `git gc`.
    #[test]
    fn a_loose_ref_wins_over_a_stale_packed_one() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_fixture(
            dir.path(),
            "ref: refs/heads/main",
            &[("refs/heads/main", HEAD_HASH)],
            Some(&format!("{OTHER_HASH} refs/heads/main\n")),
        );
        assert_eq!(read_git_head(&repo), Some(HEAD_HASH.to_string()));
    }

    /// A `git worktree` checkout, in its real layout: `.git` is a FILE
    /// pointing at `<main>/.git/worktrees/<name>`, which holds this
    /// worktree's own HEAD and a `commondir` back to the main `.git` --
    /// where `refs/heads/*` actually live. Resolving the branch against
    /// the per-worktree gitdir alone finds nothing.
    #[test]
    fn a_worktree_resolves_its_branch_through_the_common_dir() {
        let dir = tempfile::tempdir().unwrap();
        let main_git = dir.path().join("main/.git");
        let worktree_git = main_git.join("worktrees/feature");
        std::fs::create_dir_all(main_git.join("refs/heads")).unwrap();
        std::fs::create_dir_all(&worktree_git).unwrap();
        // The branch exists ONLY in the common dir, as git puts it.
        std::fs::write(main_git.join("refs/heads/feature"), format!("{HEAD_HASH}\n")).unwrap();
        std::fs::write(worktree_git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(worktree_git.join("commondir"), "../..\n").unwrap();

        let worktree = dir.path().join("feature-tree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .unwrap();
        assert_eq!(read_git_head(&worktree), Some(HEAD_HASH.to_string()));

        // The common dir's packed-refs is the other place to look.
        std::fs::remove_file(main_git.join("refs/heads/feature")).unwrap();
        assert_eq!(read_git_head(&worktree), None, "nothing holds the branch");
        std::fs::write(
            main_git.join("packed-refs"),
            format!("{HEAD_HASH} refs/heads/feature\n"),
        )
        .unwrap();
        assert_eq!(read_git_head(&worktree), Some(HEAD_HASH.to_string()));

        // A `gitdir:` may be written relative to the worktree, and a
        // detached worktree needs no common dir at all.
        let detached = dir.path().join("detached-tree");
        let detached_git = main_git.join("worktrees/detached");
        std::fs::create_dir_all(&detached_git).unwrap();
        std::fs::write(detached_git.join("HEAD"), format!("{OTHER_HASH}\n")).unwrap();
        std::fs::create_dir_all(&detached).unwrap();
        std::fs::write(
            detached.join(".git"),
            "gitdir: ../main/.git/worktrees/detached\n",
        )
        .unwrap();
        assert_eq!(read_git_head(&detached), Some(OTHER_HASH.to_string()));
    }

    /// A SHA-256 repo's object ids are 64 hex characters, not 40.
    #[test]
    fn a_sha256_repo_reads_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let repo = git_fixture(dir.path(), &hash, &[], None);
        assert_eq!(read_git_head(&repo), Some(hash));
    }

    /// Nothing readable is "unknown", never an error: the caller's whole
    /// contract is that unknown does not warn.
    #[test]
    fn an_unreadable_repo_is_unknown_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_git_head(&dir.path().join("gone")), None);

        // A `.git` with no HEAD.
        let empty = git_fixture(dir.path(), HEAD_HASH, &[], None);
        std::fs::remove_file(empty.join(".git/HEAD")).unwrap();
        assert_eq!(read_git_head(&empty), None);

        // A symbolic HEAD whose ref is nowhere.
        let dangling = tempfile::tempdir().unwrap();
        let dangling = git_fixture(dangling.path(), "ref: refs/heads/main", &[], None);
        assert_eq!(read_git_head(&dangling), None);

        // Something that is not a hash where one belongs.
        let junk = tempfile::tempdir().unwrap();
        let junk = git_fixture(junk.path(), "not a hash at all", &[], None);
        assert_eq!(read_git_head(&junk), None);

        // A `.git` file that points nowhere useful.
        let broken = dir.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(".git"), "this is not a gitdir line\n").unwrap();
        assert_eq!(read_git_head(&broken), None);
    }

    /// The warning fires only when both sides are known and differ:
    /// warning on unknown would fire on every tarball build.
    #[test]
    fn skew_needs_two_known_and_different_commits() {
        let mut env = ready_env();
        env.repo_commit = Some(HEAD_HASH.to_string());
        env.emu_commit = Some(OTHER_HASH.to_string());
        assert_eq!(
            env.version_skew(),
            Some(("0123456789".to_string(), "fedcba9876".to_string())),
            "short enough for a status line, long enough to be unique"
        );

        env.emu_commit = Some(HEAD_HASH.to_string());
        assert_eq!(env.version_skew(), None, "the same commit is no skew");

        env.emu_commit = None;
        assert_eq!(env.version_skew(), None, "a build without git never warns");

        env.repo_commit = None;
        env.emu_commit = Some(OTHER_HASH.to_string());
        assert_eq!(
            env.version_skew(),
            None,
            "an unreadable checkout never warns"
        );
    }

    #[test]
    fn run_log_names_are_filename_safe_and_prefixed() {
        use chrono::TimeZone as _;
        let now = chrono::Local.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap();
        assert_eq!(
            run_log_name("flashing world: arena!", now),
            "ggo-run-20260901-100000-flashing-world--arena.log"
        );
    }

    /// Only OUR expired transcripts die: a fresh one and a stranger's
    /// file in the same directory survive every prune.
    #[test]
    fn prune_deletes_only_expired_run_logs() {
        let dir = tempfile::tempdir().unwrap();
        let old_age = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        let make = |name: &str, expired: bool| {
            let path = dir.path().join(name);
            let file = std::fs::File::create(&path).unwrap();
            if expired {
                file.set_modified(old_age).unwrap();
            }
            path
        };
        let expired = make("ggo-run-20260824-100000-flashing.log", true);
        let fresh = make("ggo-run-20260901-100000-flashing.log", false);
        let stranger = make("Zed.log", true);
        prune_run_logs(dir.path(), Duration::from_secs(7 * 24 * 60 * 60));
        assert!(!expired.exists(), "eight days beats a seven-day lifetime");
        assert!(fresh.exists(), "today's transcript stays");
        assert!(stranger.exists(), "~/.zed/logs is not ours to clean");
    }

    /// Only the clone ZedGG made can be synced, and the script carries
    /// every leg: fast-forward pull, reclone as the pull's fallback, and
    /// the `ggo-emu` install guarded so a no-op sync skips it.
    #[test]
    fn only_the_managed_clone_offers_a_sync() {
        let mut env = ready_env();
        env.repo = Some(env.clone_dest.clone());
        let request = env.sync_request().expect("the managed clone is ours to move");
        assert_eq!(request.bin, "sh");
        assert_eq!(request.cwd, env.home, "a reclone leg needs a cwd that survives it");
        let script = &request.args[1];
        assert!(script.contains("pull --ff-only"), "fast-forward first: {script}");
        assert!(
            script.contains("|| { rm -rf") && script.contains("git clone"),
            "reclone only as the pull's fallback: {script}"
        );
        assert!(
            script.contains("cargo install --locked --path")
                && script.contains("tools/ggo-emu"),
            "the installed ggo-emu follows the synced source: {script}"
        );
        assert!(
            script.contains("\"$pre\" != \"$post\""),
            "a sync that moved nothing must not spend minutes reinstalling: {script}"
        );
        assert!(
            script.contains(&format!("'{}'", env.clone_dest.display())),
            "the clone path is quoted for the shell: {script}"
        );

        // A checkout the user maintains is not ours to pull or delete.
        env.repo = Some(PathBuf::from("/repo"));
        assert!(env.sync_request().is_none());
        assert!(!env.repo_is_managed_clone());

        // No git, nothing to sync with.
        env.repo = Some(env.clone_dest.clone());
        env.git = false;
        assert!(env.sync_request().is_none());

        // And no repo at all is nothing to sync.
        let bare = HardwareEnv {
            git: true,
            ..Default::default()
        };
        assert!(
            bare.sync_request().is_none(),
            "an unset repo must not match an unset clone destination"
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
        assert!(
            progress
                .rows()
                .iter()
                .all(|row| row.state == PhaseState::Pending)
        );
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
        assert_eq!(
            rows[0].elapsed(secs(30)),
            secs(12),
            "a done phase stops counting"
        );
        assert_eq!(rows[1].state, PhaseState::Running);
        assert_eq!(
            rows[1].elapsed(secs(30)),
            secs(18),
            "the running one keeps counting"
        );
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
        progress.apply("  [boot] banner — next: launched (30s budget)", secs(2));
        assert_eq!(
            progress.running().and_then(|row| row.detail.clone()),
            Some("boot: banner — next: launched (30s budget)".to_string()),
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
            progress
                .rows()
                .iter()
                .all(|row| row.state == PhaseState::Done),
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
        assert_eq!(
            titles[2], "Provision SD card",
            "the known ones still follow"
        );
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
