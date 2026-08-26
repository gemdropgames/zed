//! The run-oriented context-menu entries (F5.2 task S4), and the pure
//! "what would we run" half behind them.
//!
//! Three entries, all of which end up in the emu panel because it is the
//! panel that owns running things:
//!
//! | entry | offered on | what it does |
//! |---|---|---|
//! | Emulate this world | a world `.toml` | save if dirty, `emd pack-ggo --world <stem>`, run the result |
//! | Re-run (perf) | a `.cart` | run it again; when the perf ingest lands, focus the charts panel |
//! | Run hardware diagnostics | any directory | `ggo-diag --launch` -- the built-in GGO Diagnostic Cart |
//!
//! Everything in this module is either a pure function (argv, predicates,
//! prerequisite checks -- all unit-tested without spawning anything) or a
//! contributor/handler that defers all panel work into
//! [`ggo_common::panel_entry_handler`]-shaped closures. Contributors run
//! while `ProjectPanel` is leased (see
//! `Workspace::context_menu_contributions`), so nothing here may touch a
//! panel; the `is_file` stats and string splits it does make are not panel
//! work and are legal, same as in `ggo_map_panel`/`ggo_emerald_panel`.

use std::path::{Path, PathBuf};

use gpui::{App, Context, Window};
use project::ProjectPath;
use workspace::Workspace;

use ggo_common::ProcRequest;

use crate::EmuPanel;

/// The cart extensions "Re-run (perf)" is offered on. `.cart` is the code
/// cart the panel's explorer route already claims; `.ggo` is the
/// single-file variant (code + assets + GGO2 index) that
/// [`world_pack_args`] produces and `ggo_emu_core::cart::Cart::parse`
/// reads with the same code path.
pub const RUNNABLE_EXTS: [&str; 2] = ["cart", "ggo"];

/// The "Emulate this world" entry's label.
///
/// **"(cart)" is load-bearing, not decoration.** ggo-ide's Emulate boots
/// the world through a full system image -- GemOS, the OS->cart handoff,
/// the syscall surface, FAT card asset streaming -- and this one runs the
/// world as a bare cartridge (see [`world_pack_args`]). The two answer
/// different questions, and the perf runs they produce land in the SAME
/// `runs` table with no mode column, so the name is the only place a user
/// is told which one they pressed. `MIGRATION.md`'s "Emulate this world is
/// cart mode" entry has the full list of what differs.
pub const EMULATE_LABEL: &str = "Emulate this world (cart)";

// Moved to `ggo_common` so the world panel's popout Emulate can build the
// same argv without a dependency cycle; re-exported to keep this module
// the home of "what would we run" for the emu panel.
pub use ggo_common::{PACK_OUT_DIR, failure_reason, pack_out_name, world_pack_args};

/// Env var naming a non-default `ggo-diag` binary -- ggo-ide's Device page
/// reads the same one (`pages::device::DIAG_BIN_ENV`).
pub const DIAG_BIN_ENV: &str = "GGO_DIAG_BIN";

/// Bare-name fallback for the `ggo-diag` binary, resolved against `PATH`.
pub const DEFAULT_DIAG_BIN: &str = "ggo-diag";

/// Env var naming the GGO repo checkout `ggo-diag` runs against. `ggo-diag`
/// discovers the repo by walking up from its cwd, and this fork's worktree
/// is the user's GAME project, not the GGO repo -- so unlike ggo-ide
/// (which lives inside the repo and can use `CARGO_MANIFEST_DIR`) there is
/// nothing to auto-detect from, and this is the only way to point at one.
pub const DIAG_REPO_ENV: &str = "GGO_REPO";

/// Env var pinning the serial device, bypassing [`scan_serial_ports_at`] --
/// the fork's substitute for ggo-ide's Device-page port picker.
pub const DIAG_TTY_ENV: &str = "GGO_DIAG_TTY";

/// The file whose presence marks a GGO repo checkout: `firmware/system` is
/// the OS crate every diagnostic build needs. `looks_like_repo_root` in
/// ggo-ide's `backend/emubuild` uses exactly this fingerprint.
pub(crate) const REPO_FINGERPRINT: &str = "firmware/system/Cargo.toml";

// ------------------------------------------------------------- predicates

/// Is `rel` a file this panel can run? Extension-only, case-insensitive --
/// the same test `intercept_cart_open` makes, widened to the `.ggo`
/// variant a world build produces.
pub fn is_runnable_cart(rel: &str) -> bool {
    Path::new(rel)
        .extension()
        .is_some_and(|ext| RUNNABLE_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

// ------------------------------------------------------- world -> cartridge

/// `abs` as a `/`-separated path relative to `root`, or `abs` itself when
/// it lies outside `root`.
///
/// Both answers are usable by the panel: it resolves a selection with
/// `project_root.join(selected)`, and `Path::join` with an absolute path
/// yields that absolute path. The relative form is preferred purely
/// because it is what the transport row shows the user.
pub fn cart_selection(root: &Path, abs: &Path) -> String {
    match abs.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => abs.to_string_lossy().into_owned(),
    }
}

// ----------------------------------------------------- hardware diagnostics

/// Stable-symlink directory scanned first -- immune to `ttyUSB`
/// renumbering across reboots/replugs, which has cost real debugging
/// sessions (ggo-ide's `backend::diag::scan_serial_ports_at`, ported
/// verbatim below).
pub const SERIAL_BY_ID_DIR: &str = "/dev/serial/by-id";
/// Fallback raw-device directory, scanned only when [`SERIAL_BY_ID_DIR`]
/// has nothing.
pub const DEV_DIR: &str = "/dev";
/// Serial-device name prefixes, in the order a scan reports them.
///
/// The first two are Linux (`ttyUSB` for the ULX3S's FT231X, `ttyACM` for
/// CDC-ACM boards) and are what ggo-ide scanned for -- it ran on a Linux
/// box. Zed is a first-class macOS app, where the same board enumerates as
/// `/dev/cu.usbserial-*` (or `tty.usbmodem*` for CDC), so a Mac user WITH a
/// board plugged in would otherwise be told there is no board.
///
/// Both macOS spellings are listed because both exist and either can be
/// opened; `cu.` (call-out, no DCD wait) is the right one to talk to a dev
/// board through, and the scan's `sort` puts `cu.` before `tty.` so it is
/// the one [`diag_request`] picks.
const TTY_PREFIXES: [&str; 4] = ["ttyUSB", "ttyACM", "cu.usb", "tty.usb"];

/// Scan for candidate serial devices: every entry under `by_id_dir` if it
/// has any, else every entry under `dev_dir` whose name starts with
/// `ttyUSB`/`ttyACM`. Sorted, deduped. Parametrized over both directories
/// (rather than hardcoding the two constants above) so this is testable
/// against a fixture tree instead of the real, host-dependent `/dev`. A
/// missing/unreadable directory reads as "no ports there" rather than an
/// error -- a board that is not plugged in is a normal state, not a
/// failure.
pub fn scan_serial_ports_at(by_id_dir: &Path, dev_dir: &Path) -> Vec<String> {
    let mut ports = Vec::new();
    if let Ok(entries) = std::fs::read_dir(by_id_dir) {
        for e in entries.flatten() {
            ports.push(e.path().to_string_lossy().into_owned());
        }
    }
    if ports.is_empty()
        && let Ok(entries) = std::fs::read_dir(dev_dir)
    {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if TTY_PREFIXES.iter().any(|p| name.starts_with(p)) {
                ports.push(dev_dir.join(name.as_ref()).to_string_lossy().into_owned());
            }
        }
    }
    ports.sort();
    ports.dedup();
    ports
}

/// Walk up from `start` (inclusive) for a GGO repo checkout -- the nearest
/// ancestor holding [`REPO_FINGERPRINT`].
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(REPO_FINGERPRINT).is_file())
        .map(Path::to_path_buf)
}

/// Everything [`diag_request`] needs, gathered from the environment and
/// the filesystem by the caller so the decision itself stays pure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagEnv {
    /// The `ggo-diag` binary, already resolved.
    pub bin: String,
    /// The GGO repo checkout, if one was found.
    pub repo: Option<PathBuf>,
    /// Candidate serial devices, in scan order.
    pub ports: Vec<String>,
}

/// The `ggo-diag` invocation for the built-in GGO Diagnostic Cart, or the
/// message to show the user instead.
///
/// **This is the "make the failure legible" requirement.** Hardware
/// diagnostics genuinely need a board: `ggo-diag`'s pipeline compiles
/// GemOS out of a GGO repo checkout, flashes an attached ULX3S over a
/// serial device, and reads its UART back. A developer machine with no
/// board attached -- the normal case -- can satisfy neither precondition,
/// and the wrong answer would be a menu entry that appears to do nothing.
/// So both preconditions are checked up front and every missing one is
/// NAMED, with the env var that supplies it, in a single message the panel
/// puts on its status row.
///
/// `--launch` with no directory is the built-in diagnostic cart
/// (`ggo-diag --help`: "with no DIR: build + run the GGO Diagnostic Cart
/// (firmware/carts/diag)"), which is why no project is needed.
/// `--skip-pnr` is not the CLI's default but IS the right default for a
/// one-click menu entry: without it step 3 place-and-routes the full SoC,
/// ~20 minutes, before anything reaches the board.
pub fn diag_request(env: &DiagEnv) -> Result<ProcRequest, String> {
    let mut missing: Vec<String> = Vec::new();
    if env.repo.is_none() {
        missing.push(format!(
            "no GGO repo checkout (nothing with {REPO_FINGERPRINT} above the \
             open folder) -- set {DIAG_REPO_ENV}"
        ));
    }
    if env.ports.is_empty() {
        missing.push(format!(
            "no serial device (looked in {SERIAL_BY_ID_DIR}, then {DEV_DIR} for \
             {}) -- connect the board, or set {DIAG_TTY_ENV}",
            TTY_PREFIXES
                .iter()
                .map(|p| format!("{p}*"))
                .collect::<Vec<_>>()
                .join("/")
        ));
    }
    if !missing.is_empty() {
        return Err(format!(
            "hardware diagnostics need a board: {}",
            missing.join("; ")
        ));
    }
    Ok(ProcRequest::new(
        env.bin.clone(),
        env.repo.clone().expect("checked above"),
        diag_args(&env.ports[0]),
    ))
}

/// `ggo-diag`'s argv for the built-in diagnostic cart on `tty` -- ggo-ide's
/// `pages::device::argv::build_args` for a `launch_diag_cart` run, minus
/// the form fields this entry has no form to collect (`--baud` and
/// `--collect-seconds` both fall back to the CLI's own defaults).
pub fn diag_args(tty: &str) -> Vec<String> {
    vec![
        "--tty".to_string(),
        tty.to_string(),
        "--skip-pnr".to_string(),
        "--launch".to_string(),
    ]
}

/// [`DiagEnv`] read off this machine: the binary override, the repo above
/// `near` (usually the open folder), and either the pinned TTY or a scan.
pub fn diag_env(near: Option<&Path>) -> DiagEnv {
    let bin = std::env::var(DIAG_BIN_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DIAG_BIN.to_string());
    let repo = std::env::var(DIAG_REPO_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .filter(|repo| repo.join(REPO_FINGERPRINT).is_file())
        .or_else(|| near.and_then(find_repo_root));
    let ports = match std::env::var(DIAG_TTY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(tty) => vec![tty],
        None => scan_serial_ports_at(Path::new(SERIAL_BY_ID_DIR), Path::new(DEV_DIR)),
    };
    DiagEnv { bin, repo, ports }
}

// -------------------------------------------------------- the context menu

/// `workspace::ContextMenuContributor` for the run actions.
///
/// Gated on [`ggo_common::rel_in_primary_worktree`] first, like every other
/// GGO contributor: a path in a secondary worktree, or in a non-local
/// project (SSH remote / collab guest), gets nothing, because the panel
/// runs carts off the primary worktree's `abs_path` with `std::fs` and
/// spawns children on this machine.
///
/// "Run hardware diagnostics" is gated to directories (`is_dir`), not to a
/// project: it needs no `emerald.toml` above the click, only a GGO repo
/// checkout and a board, both checked inside the handler (see
/// [`diag_env`]). That "no project needed" is the spec's "anywhere" --
/// it is not offered on every individual path, which would put a second
/// always-present line on every file's menu; a right-click on a directory
/// (including the worktree root) is the fork's stand-in surface, since the
/// panel's own toolbar is the cart transport, not a project-wide action
/// bar.
pub fn contribute_run_menu(
    workspace: &mut Workspace,
    path: &ProjectPath,
    is_dir: bool,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Vec<ui::ContextMenuItem> {
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return Vec::new();
    };
    let mut items: Vec<ui::ContextMenuItem> = Vec::new();
    if !is_dir {
        if is_runnable_cart(&rel) {
            items.push(
                ui::ContextMenuEntry::new("Re-run (perf)")
                    .icon(ui::IconName::RotateCw)
                    .handler(rerun_handler(cx.weak_entity(), rel))
                    .into(),
            );
        } else if ggo_world_panel::world_stem(&rel).is_some() {
            items.push(
                ui::ContextMenuEntry::new(EMULATE_LABEL)
                    .icon(ui::IconName::PlayFilled)
                    .handler(emulate_world_handler(cx.weak_entity(), rel))
                    .into(),
            );
        }
    }
    if is_dir {
        items.push(
            ui::ContextMenuEntry::new("Run hardware diagnostics")
                .icon(ui::IconName::Debug)
                .handler(diagnostics_handler(cx.weak_entity()))
                .into(),
        );
    }
    items
}

/// Reveal + focus the emu panel and run `action` on it -- the body every
/// entry handler here ends with.
///
/// NOT `ggo_common::panel_entry_handler`, which reaches the panel without
/// focusing it: all three of these entries START something the user then
/// wants to watch, so the dock has to come forward. That means going
/// through `Workspace::focus_panel`, which needs a `Context<Workspace>`,
/// which means the panel method runs INSIDE the workspace's own update --
/// so every one of them defers anything that reads the workspace back
/// (i.e. `EmuPanel::refresh_root`) onto a spawned task, exactly as
/// `EmuPanel::open_rel_path` already does for the interceptor.
///
/// Safe to call a panel from here at all, unlike from the contributor:
/// contributors run while `ProjectPanel` is leased, handlers run after the
/// lease is released.
fn emu_panel_handler(
    workspace: gpui::WeakEntity<Workspace>,
    action: impl Fn(&mut EmuPanel, &mut Window, &mut Context<EmuPanel>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        let Some(workspace) = workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            crate::open_emu_item(workspace, window, cx, |panel: &mut EmuPanel, window, cx| {
                action(panel, window, cx)
            });
        });
    }
}

/// The message shown when the world could not be written, and the build
/// is therefore refused.
pub const SAVE_FAILED_MESSAGE: &str =
    "could not save this world — refusing to build, a run would boot the stale file on disk";

/// "Emulate this world". Saves the world panel's document first, OUTSIDE
/// the workspace update below -- the world panel is a different entity and
/// reaching it needs only a read, so it must happen before the emu panel's
/// update takes the workspace, not inside it.
///
/// **A failed save cancels the build.** `emd pack-ggo` stages worlds from
/// DISK, so building after a write that did not land would silently boot
/// the previous version of the world -- the exact bug ggo-ide names in
/// `pages/world/mod.rs`'s `emulate_after_save` ("Dropped on a failed save:
/// never boot a stale world"). The refusal is reported on the emu panel's
/// status row rather than swallowed, so it is as visible as the build it
/// replaced.
pub(crate) fn emulate_world_handler(
    workspace: gpui::WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let route = emu_panel_handler(workspace.clone(), {
        let rel = rel.clone();
        move |panel, window, cx| panel.emulate_world(&rel, window, cx)
    });
    let blocked = emu_panel_handler(workspace.clone(), |panel, _window, cx| {
        panel.report_blocked(SAVE_FAILED_MESSAGE.to_string(), cx)
    });
    move |window, cx| {
        let saved = match workspace.upgrade() {
            // No world panel docked means no unsaved copy of this world
            // anywhere, so the file on disk IS the current one.
            Some(workspace) => workspace
                .read(cx)
                .panel::<ggo_world_panel::WorldPanel>(cx)
                .is_none_or(|world_panel| {
                    world_panel.update(cx, |world_panel, cx| {
                        world_panel.save_if_open_and_dirty(&rel, cx)
                    })
                }),
            None => return,
        };
        if saved {
            route(window, cx)
        } else {
            blocked(window, cx)
        }
    }
}

/// "Re-run (perf)".
pub(crate) fn rerun_handler(
    workspace: gpui::WeakEntity<Workspace>,
    rel: String,
) -> impl Fn(&mut Window, &mut App) + 'static {
    emu_panel_handler(workspace, move |panel, window, cx| {
        panel.rerun(&rel, window, cx)
    })
}

/// "Run hardware diagnostics".
pub(crate) fn diagnostics_handler(
    workspace: gpui::WeakEntity<Workspace>,
) -> impl Fn(&mut Window, &mut App) + 'static {
    emu_panel_handler(workspace, |panel, window, cx| {
        panel.run_hardware_diagnostics(window, cx)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_common::ProcCapture;

    #[test]
    fn the_cart_predicate_matches_only_runnable_extensions() {
        assert!(is_runnable_cart("carts/green.cart"));
        assert!(is_runnable_cart("build/GAME.GGO"), "case-insensitive");
        assert!(is_runnable_cart("target/ggo-emulate/worlds-main.ggo"));
        assert!(!is_runnable_cart("assets/worlds/main.toml"));
        assert!(!is_runnable_cart("notes.txt"));
        assert!(!is_runnable_cart("cart"), "no extension at all");
    }

    /// The boot world reaches `emd` as a FLAG, which is what makes it
    /// assertable here (and in the panel's own tests) without spawning
    /// anything -- see [`world_pack_args`]'s doc.
    #[test]
    fn the_pack_argv_names_the_out_path_and_the_boot_world() {
        assert_eq!(
            world_pack_args(
                Path::new("/proj/target/ggo-emulate/worlds-main.ggo"),
                "worlds/main"
            ),
            [
                "pack-ggo",
                "--out",
                "/proj/target/ggo-emulate/worlds-main.ggo",
                "--world",
                "worlds/main",
            ]
        );
    }

    /// `ProcRequest::emd` is what adds `--json`, so the whole request --
    /// binary, cwd and argv -- is what a panel actually hands the runner.
    #[test]
    fn the_pack_request_runs_in_the_project_root_with_json() {
        let request = ProcRequest::emd(
            "/proj",
            world_pack_args(Path::new("/proj/out.ggo"), "worlds/main"),
        );
        assert_eq!(request.cwd, Path::new("/proj"));
        assert_eq!(request.args.last().unwrap(), "--json");
        assert!(request.command_line().contains("--world worlds/main"));
    }

    #[test]
    fn the_pack_output_name_flattens_a_nested_stem() {
        assert_eq!(pack_out_name("worlds/main"), "worlds-main.ggo");
        assert_eq!(pack_out_name("worlds/boss/main"), "worlds-boss-main.ggo");
    }

    /// A built cart inside the worktree is shown (and stored) relative to
    /// it; one outside falls back to the absolute path, which the panel's
    /// `project_root.join(selected)` still resolves correctly.
    #[test]
    fn the_cart_selection_is_worktree_relative_when_it_can_be() {
        assert_eq!(
            cart_selection(Path::new("/proj"), Path::new("/proj/target/x.ggo")),
            "target/x.ggo"
        );
        assert_eq!(
            cart_selection(Path::new("/proj"), Path::new("/elsewhere/x.ggo")),
            "/elsewhere/x.ggo"
        );
    }

    #[test]
    fn the_failure_reason_is_the_last_non_blank_line() {
        assert_eq!(
            failure_reason(&ProcCapture {
                ok: false,
                lines: vec!["building".into(), "error: no such world".into(), "".into()],
            }),
            "error: no such world"
        );
        assert_eq!(
            failure_reason(&ProcCapture {
                ok: false,
                lines: Vec::new()
            }),
            "no output"
        );
    }

    // ------------------------------------------------ hardware diagnostics

    /// The by-id directory wins outright when it has anything, because
    /// those symlinks survive a replug and `ttyUSB0` does not.
    #[test]
    fn the_serial_scan_prefers_by_id_and_falls_back_to_dev() {
        let dir = tempfile::tempdir().unwrap();
        let by_id = dir.path().join("by-id");
        let dev = dir.path().join("dev");
        std::fs::create_dir_all(&by_id).unwrap();
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("ttyUSB0"), b"").unwrap();
        std::fs::write(dev.join("random"), b"").unwrap();

        assert_eq!(
            scan_serial_ports_at(&by_id, &dev),
            [dev.join("ttyUSB0").to_string_lossy().into_owned()],
            "an empty by-id falls through to ttyUSB*/ttyACM* only"
        );

        std::fs::write(by_id.join("usb-FTDI_board"), b"").unwrap();
        assert_eq!(
            scan_serial_ports_at(&by_id, &dev),
            [by_id.join("usb-FTDI_board").to_string_lossy().into_owned()],
            "by-id wins outright once it has an entry"
        );

        let missing = dir.path().join("nope");
        assert!(scan_serial_ports_at(&missing, &missing).is_empty());
    }

    #[test]
    fn the_diag_argv_launches_the_builtin_cart_without_a_full_pnr() {
        assert_eq!(
            diag_args("/dev/ttyUSB0"),
            ["--tty", "/dev/ttyUSB0", "--skip-pnr", "--launch"],
            "bare --launch IS the built-in diagnostic cart"
        );
    }

    #[test]
    fn a_complete_diag_env_produces_a_request_in_the_repo() {
        let request = diag_request(&DiagEnv {
            bin: "ggo-diag".into(),
            repo: Some(PathBuf::from("/ggo")),
            ports: vec!["/dev/ttyUSB0".into(), "/dev/ttyUSB1".into()],
        })
        .expect("both prerequisites present");
        assert_eq!(request.bin, "ggo-diag");
        assert_eq!(
            request.cwd,
            Path::new("/ggo"),
            "ggo-diag detects the repo from its cwd"
        );
        assert_eq!(request.args, diag_args("/dev/ttyUSB0"));
    }

    /// **The no-hardware case, which is the normal one.** Every missing
    /// prerequisite must be NAMED, with the env var that supplies it --
    /// the entry must never be a silent no-op.
    #[test]
    fn a_missing_board_produces_a_message_naming_everything_that_is_missing() {
        let err = diag_request(&DiagEnv {
            bin: DEFAULT_DIAG_BIN.into(),
            repo: None,
            ports: Vec::new(),
        })
        .unwrap_err();
        assert!(err.contains(REPO_FINGERPRINT), "{err}");
        assert!(err.contains(DIAG_REPO_ENV), "{err}");
        assert!(err.contains(SERIAL_BY_ID_DIR), "{err}");
        assert!(err.contains(DIAG_TTY_ENV), "{err}");

        // ...and one missing half names only that half.
        let no_board = diag_request(&DiagEnv {
            bin: DEFAULT_DIAG_BIN.into(),
            repo: Some(PathBuf::from("/ggo")),
            ports: Vec::new(),
        })
        .unwrap_err();
        assert!(!no_board.contains(DIAG_REPO_ENV), "{no_board}");
        assert!(no_board.contains(DIAG_TTY_ENV), "{no_board}");
    }

    #[test]
    fn the_repo_walk_finds_a_checkout_above_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("ggo");
        std::fs::create_dir_all(repo.join("firmware/system")).unwrap();
        std::fs::create_dir_all(repo.join("games/demo/assets")).unwrap();
        assert_eq!(find_repo_root(&repo.join("games/demo/assets")), None);
        std::fs::write(repo.join(REPO_FINGERPRINT), "").unwrap();
        assert_eq!(
            find_repo_root(&repo.join("games/demo/assets")).as_deref(),
            Some(repo.as_path())
        );
        assert_eq!(find_repo_root(dir.path()), None);
    }
}
