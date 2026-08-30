//! Session discovery: each Zed process that hosts an emu-panel remote
//! listener advertises itself as `<dir>/<pid>.json` next to its
//! `<dir>/<pid>.sock`, where `<dir>` is `$XDG_RUNTIME_DIR/zedgg-emu`
//! (fallback `/tmp/zedgg-emu-<uid>`). The bridge lists the dir, prunes
//! entries whose pid is gone, and targets one socket.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One advertised Zed process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub pid: u32,
    /// Absolute path of this process's unix socket.
    pub socket: PathBuf,
    /// Absolute project-root path of every workspace currently open.
    pub workspaces: Vec<String>,
}

/// The advertisement dir (created if missing).
pub fn dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(base) => PathBuf::from(base).join("zedgg-emu"),
        // uid keeps parallel users out of each other's way on shared /tmp.
        None => PathBuf::from(format!("/tmp/zedgg-emu-{}", uid())),
    }
}

fn uid() -> u32 {
    // /proc/self status is linux-only, but so are our unix sockets here.
    std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid()
        })
        .unwrap_or(0)
}

/// Paths for this process's advertisement.
pub fn session_paths(dir: &Path, pid: u32) -> (PathBuf, PathBuf) {
    (dir.join(format!("{pid}.json")), dir.join(format!("{pid}.sock")))
}

/// Write (or rewrite) this process's advertisement file.
pub fn publish(dir: &Path, info: &SessionInfo) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // The socket grants emulator control: keep the dir owner-only, like
    // $XDG_RUNTIME_DIR itself (matters for the /tmp fallback).
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let (json_path, _) = session_paths(dir, info.pid);
    let tmp = json_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(info).expect("SessionInfo serializes"))?;
    std::fs::rename(tmp, json_path)
}

/// Remove this process's advertisement (best effort — a crash leaves the
/// files behind, which is exactly what pruning is for).
pub fn withdraw(dir: &Path, pid: u32) {
    let (json_path, sock_path) = session_paths(dir, pid);
    std::fs::remove_file(json_path).ok();
    std::fs::remove_file(sock_path).ok();
}

/// Live sessions under `dir`: parseable advertisements whose pid still
/// exists. Dead entries are deleted on the way through.
pub fn list(dir: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed: Option<SessionInfo> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        match parsed {
            Some(info) if pid_alive(info.pid) => out.push(info),
            // Unparseable or dead: prune the pair so the list stays clean.
            Some(info) => {
                withdraw(dir, info.pid);
            }
            None => {
                std::fs::remove_file(&path).ok();
            }
        }
    }
    out.sort_by_key(|s| s.pid);
    out
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, dir: &Path) -> SessionInfo {
        SessionInfo {
            pid,
            socket: session_paths(dir, pid).1,
            workspaces: vec!["/home/x/proj".to_string()],
        }
    }

    #[test]
    fn publish_then_list_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        publish(dir.path(), &info(me, dir.path())).unwrap();
        assert_eq!(list(dir.path()), vec![info(me, dir.path())]);
    }

    #[test]
    fn dead_pids_are_pruned_from_list_and_disk() {
        let dir = tempfile::tempdir().unwrap();
        // pid 0 never exists as /proc/0 on linux; 4194304+ is above the
        // default pid_max, so this cannot race a real process.
        let dead = 4_194_999;
        publish(dir.path(), &info(dead, dir.path())).unwrap();
        assert!(list(dir.path()).is_empty());
        assert!(!session_paths(dir.path(), dead).0.exists(), "dead advertisement must be deleted");
    }

    #[test]
    fn unparseable_advertisements_are_deleted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("999.json"), b"garbage").unwrap();
        let me = std::process::id();
        publish(dir.path(), &info(me, dir.path())).unwrap();
        assert_eq!(list(dir.path()).len(), 1);
        assert!(!dir.path().join("999.json").exists());
    }

    #[test]
    fn withdraw_removes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        publish(dir.path(), &info(me, dir.path())).unwrap();
        std::fs::write(session_paths(dir.path(), me).1, b"").unwrap();
        withdraw(dir.path(), me);
        assert!(list(dir.path()).is_empty());
        assert!(!session_paths(dir.path(), me).1.exists());
    }

    #[test]
    fn missing_dir_lists_empty() {
        assert!(list(Path::new("/nonexistent/zedgg-emu-test")).is_empty());
    }
}
