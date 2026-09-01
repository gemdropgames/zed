//! Shared wire contract between a Zed session hosting the GGO emulator
//! panel and the `zedgg-emu-mcp` bridge binary.
//!
//! Two halves, both dependency-light so either side can link it:
//!
//! - [`protocol`]: the JSON-lines request/response types spoken over the
//!   per-session unix socket.
//! - [`registry`]: how live sessions advertise themselves (one JSON file +
//!   one socket per Zed process under the runtime dir) and how the bridge
//!   discovers and prunes them.

pub mod protocol;
pub mod registry;

use std::path::{Path, PathBuf};

/// `ggo-diag`'s consolidated log for a run: the file under `logs_dir`
/// (`~/.ggo/diag/logs`) whose name ends in `_<started_at>.log`. ggo-diag
/// stamps it `{branch}_{commit}_{started_at}.log` and writes the same
/// `started_at` onto the run row, which is the only link between the two
/// -- no database column records the path. `None` for a run ggo-diag
/// never saw (an emulator run) or whose log is gone.
pub fn diag_log_path(logs_dir: &Path, started_at: &str) -> Option<PathBuf> {
    if started_at.is_empty() {
        return None;
    }
    let suffix = format!("_{started_at}.log");
    let mut matches: Vec<PathBuf> = std::fs::read_dir(logs_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_diag_log_is_found_by_the_runs_started_at_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path();
        std::fs::write(logs.join("main_5370a5a_2026-09-01_13-29-09.log"), "").unwrap();
        std::fs::write(logs.join("main_7fe694e_2026-09-01_12-34-22.log"), "").unwrap();
        assert_eq!(
            diag_log_path(logs, "2026-09-01_13-29-09"),
            Some(logs.join("main_5370a5a_2026-09-01_13-29-09.log"))
        );
        assert_eq!(diag_log_path(logs, "2026-09-01_00-00-00"), None);
        assert_eq!(diag_log_path(logs, ""), None, "an empty stamp matches nothing");
        assert_eq!(diag_log_path(&logs.join("missing"), "2026-09-01_13-29-09"), None);
    }
}
