//! Agent remote-control host: a per-process unix socket (advertised via
//! `ggo_emu_remote::registry`) through which the `zedgg-emu-mcp` bridge
//! drives this process's emu panels — boot a cart, latch pad input,
//! pause/step frame-precisely, screenshot the latest frame, read uart.
//!
//! Shape: a background accept loop reads JSON-line requests and forwards
//! `(Request, reply)` pairs over a channel to one foreground dispatch
//! task, which resolves the target workspace's panel and answers. All
//! panel access happens on the foreground executor — the socket side
//! never touches gpui state.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine as _;
use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Global, WeakEntity};
use workspace::Workspace;
use ggo_emu_remote::protocol::{Cmd, Request, Response, parse_request, response_line};
use ggo_emu_remote::registry::{self, SessionInfo};

use crate::EmuPanel;
use crate::input::{SELECT_BIT, button_bit};

/// Foreground registry of live panels (keyed by absolute project root)
/// and of every live workspace (keyed by entity id — its root is resolved
/// live, since worktrees attach after construction). Workspaces let a
/// remote boot OPEN the emu panel itself instead of requiring a human to
/// have clicked a cart first.
#[derive(Default)]
pub struct RemotePanels {
    panels: HashMap<String, (WeakEntity<EmuPanel>, Option<AnyWindowHandle>)>,
    workspaces: HashMap<u64, (WeakEntity<Workspace>, Option<AnyWindowHandle>)>,
}

impl Global for RemotePanels {}

/// `EmuPanel::refresh_root` calls this once it knows its project root, so
/// the panel becomes addressable and the on-disk advertisement lists the
/// workspace.
pub fn register_panel(
    root: PathBuf,
    panel: WeakEntity<EmuPanel>,
    window: Option<AnyWindowHandle>,
    cx: &mut App,
) {
    if cx.try_global::<RemotePanels>().is_none() {
        cx.set_global(RemotePanels::default());
    }
    cx.global_mut::<RemotePanels>()
        .panels
        .insert(root.to_string_lossy().into_owned(), (panel, window));
    publish_advertisement(cx);
}

fn publish_advertisement(cx: &App) {
    let workspaces: Vec<String> = cx
        .try_global::<RemotePanels>()
        .map(|g| g.panels.keys().cloned().collect())
        .unwrap_or_default();
    publish_advertisement_roots(workspaces);
}

fn publish_advertisement_roots(mut workspaces: Vec<String>) {
    workspaces.sort();
    workspaces.dedup();
    let dir = registry::dir();
    let pid = std::process::id();
    let (_, socket) = registry::session_paths(&dir, pid);
    registry::publish(&dir, &SessionInfo { pid, socket, workspaces }).ok();
}

/// Start the socket host. Called once from `ggo_emu_panel::init`.
pub fn init(cx: &mut App) {
    cx.set_global(RemotePanels::default());
    publish_advertisement(cx);

    // Track every workspace from birth, so a remote boot can open the
    // emu panel itself (same registration idiom as the dock panels).
    cx.observe_new(|_: &mut Workspace, window, cx| {
        let handle = window.map(|w| w.window_handle());
        let weak = cx.weak_entity();
        let id = cx.entity_id().as_u64();
        cx.defer(move |cx| {
            if cx.try_global::<RemotePanels>().is_none() {
                cx.set_global(RemotePanels::default());
            }
            cx.global_mut::<RemotePanels>().workspaces.insert(id, (weak, handle));
        });
    })
    .detach();

    let dir = registry::dir();
    let (_, sock_path) = registry::session_paths(&dir, std::process::id());
    std::fs::remove_file(&sock_path).ok(); // stale from a recycled pid

    let (tx, rx) = smol::channel::unbounded::<(Request, smol::channel::Sender<Response>)>();

    // Foreground dispatch: the only place panel entities are touched.
    cx.spawn(async move |cx| {
        while let Ok((req, reply)) = rx.recv().await {
            let resp = dispatch(req, cx).await;
            reply.send(resp).await.ok();
        }
    })
    .detach();

    // Socket side, entirely on the background executor.
    let executor = cx.background_executor().clone();
    let accept_executor = executor.clone();
    cx.background_spawn(async move {
        let listener = match smol::net::unix::UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("zedgg-emu remote: bind {} failed: {e}", sock_path.display());
                return;
            }
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            accept_executor
                .spawn(async move {
                    serve_connection(stream, tx).await;
                })
                .detach();
        }
    })
    .detach();
}

async fn serve_connection(
    stream: smol::net::unix::UnixStream,
    tx: smol::channel::Sender<(Request, smol::channel::Sender<Response>)>,
) {
    use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut writer = stream.clone();
    let mut lines = BufReader::new(stream).lines();
    while let Some(Ok(line)) = smol::stream::StreamExt::next(&mut lines).await {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match parse_request(&line) {
            Ok(req) => {
                let (reply_tx, reply_rx) = smol::channel::bounded(1);
                if tx.send((req, reply_tx)).await.is_err() {
                    return; // host shut down
                }
                match reply_rx.recv().await {
                    Ok(resp) => resp,
                    Err(_) => return,
                }
            }
            Err(e) => Response::err(0, e),
        };
        let mut out = response_line(&resp);
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Resolve which registered workspace a command targets: an explicit path
/// must match exactly; omitted is fine only when there is exactly one.
pub fn resolve_workspace(keys: &[String], requested: Option<&str>) -> Result<String, String> {
    match requested {
        Some(w) => keys
            .iter()
            .find(|k| k.as_str() == w)
            .cloned()
            .ok_or_else(|| format!("no workspace {w:?}; open: {keys:?}")),
        None => match keys {
            [only] => Ok(only.clone()),
            [] => Err("no emu panel registered in this zed session".to_string()),
            _ => Err(format!("multiple workspaces open, pass one of: {keys:?}")),
        },
    }
}

/// Button names (the pad's key names plus `"select"`) -> 18-bit mask.
pub fn buttons_to_mask(buttons: &[String]) -> Result<u32, String> {
    let mut mask = 0u32;
    for b in buttons {
        let bit = match b.as_str() {
            "select" => SELECT_BIT,
            other => button_bit(other)
                .ok_or_else(|| format!("unknown button {other:?} (z x a s up down left right q w e r t y u i enter select)"))?,
        };
        mask |= bit;
    }
    Ok(mask)
}

/// Live (root, workspace, window) rows for every tracked workspace whose
/// project has a visible worktree, pruning dropped entities.
fn live_workspaces(cx: &mut App) -> Vec<(String, Entity<Workspace>, Option<AnyWindowHandle>)> {
    let tracked: Vec<(u64, WeakEntity<Workspace>, Option<AnyWindowHandle>)> = cx
        .try_global::<RemotePanels>()
        .map(|g| g.workspaces.iter().map(|(id, (w, h))| (*id, w.clone(), *h)).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut dead = Vec::new();
    for (id, weak, handle) in tracked {
        let Some(workspace) = weak.upgrade() else {
            dead.push(id);
            continue;
        };
        let root = workspace.read(cx).project().read(cx).visible_worktrees(cx).next().map(
            |worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned(),
        );
        if let Some(root) = root {
            out.push((root, workspace, handle));
        }
    }
    if !dead.is_empty() {
        cx.global_mut::<RemotePanels>().workspaces.retain(|id, _| !dead.contains(id));
    }
    out
}

fn workspace_of(cmd: &Cmd) -> Option<&str> {
    match cmd {
        Cmd::Status => None,
        Cmd::Boot { workspace, .. }
        | Cmd::Input { workspace, .. }
        | Cmd::Pause { workspace }
        | Cmd::Resume { workspace }
        | Cmd::Step { workspace, .. }
        | Cmd::Screenshot { workspace }
        | Cmd::Uart { workspace, .. }
        | Cmd::Stop { workspace } => workspace.as_deref(),
    }
}

async fn dispatch(req: Request, cx: &mut AsyncApp) -> Response {
    let id = req.id;
    match dispatch_inner(req.cmd, cx).await {
        Ok(data) => Response::ok(id, data),
        Err(e) => Response::err(id, e),
    }
}

async fn dispatch_inner(cmd: Cmd, cx: &mut AsyncApp) -> Result<serde_json::Value, String> {
    // Snapshot panels and live workspaces on the foreground.
    struct Target {
        root: String,
        panel: Option<WeakEntity<EmuPanel>>,
        workspace: Option<Entity<Workspace>>,
        window: Option<AnyWindowHandle>,
    }
    let targets: Vec<Target> = cx.update(|cx| {
        let panels: Vec<(String, WeakEntity<EmuPanel>, Option<AnyWindowHandle>)> = cx
            .try_global::<RemotePanels>()
            .map(|g| {
                g.panels
                    .iter()
                    .map(|(k, (p, w))| (k.clone(), p.clone(), *w))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut targets: Vec<Target> = panels
            .into_iter()
            .map(|(root, panel, window)| Target {
                root,
                panel: Some(panel),
                workspace: None,
                window,
            })
            .collect();
        for (root, workspace, window) in live_workspaces(cx) {
            match targets.iter_mut().find(|t| t.root == root) {
                Some(t) => t.workspace = Some(workspace),
                None => targets.push(Target { root, panel: None, workspace: Some(workspace), window }),
            }
        }
        // Keep the on-disk advertisement in step with live worktrees.
        publish_advertisement_roots(targets.iter().map(|t| t.root.clone()).collect());
        targets
    });

    if let Cmd::Status = cmd {
        let mut rows = Vec::new();
        for t in &targets {
            let status = t
                .panel
                .as_ref()
                .and_then(|p| p.update(cx, |p, _| p.remote_status(t.root.clone())).ok());
            rows.push(status.unwrap_or(ggo_emu_remote::protocol::WorkspaceStatus {
                workspace: t.root.clone(),
                cart: None,
                running: false,
                paused: false,
                frame: 0,
            }));
        }
        return Ok(serde_json::json!({ "pid": std::process::id(), "workspaces": rows }));
    }

    let keys: Vec<String> = targets.iter().map(|t| t.root.clone()).collect();
    let target_root = resolve_workspace(&keys, workspace_of(&cmd))?;
    let target = targets
        .into_iter()
        .find(|t| t.root == target_root)
        .expect("resolve_workspace returned a member of keys");
    let window = target.window;
    let panel = target.panel;

    match cmd {
        Cmd::Status => unreachable!("handled above"),
        Cmd::Boot { cart, .. } => {
            let window = window.ok_or("workspace has no window (headless test?)")?;
            let root = std::path::PathBuf::from(&target.root);
            // No panel yet? Open it — the whole point of a remote boot.
            let result: Result<(), String> = match (panel, target.workspace) {
                (Some(panel), _) => window
                    .update(cx, |_, window, app| {
                        panel.update(app, |p, cx| p.remote_boot(cart, root, window, cx))
                    })
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?,
                (None, Some(workspace)) => window
                    .update(cx, |_, window, app| {
                        workspace.update(app, |workspace, cx| {
                            let mut outcome = Err("emu panel did not open".to_string());
                            crate::open_emu_item(workspace, window, cx, |panel, window, cx| {
                                outcome = panel.remote_boot(cart, root, window, cx);
                            });
                            outcome
                        })
                    })
                    .map_err(|e| e.to_string())?,
                (None, None) => Err("workspace vanished".to_string()),
            };
            result?;
            Ok(serde_json::json!({ "booted": true }))
        }
        Cmd::Stop { .. } => {
            let panel = panel.ok_or("no emu panel open in this workspace")?;
            let window = window.ok_or("panel has no window (headless test?)")?;
            window
                .update(cx, |_, window, app| {
                    panel.update(app, |p, cx| p.remote_stop(window, cx))
                })
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "stopped": true }))
        }
        Cmd::Input { buttons, .. } => {
            let mask = buttons_to_mask(&buttons)?;
            let panel = panel.ok_or("no emu panel open — emu_boot first")?;
            panel
                .update(cx, |p, _| p.remote_input(mask))
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "mask": mask }))
        }
        Cmd::Pause { .. } => {
            let panel = panel.ok_or("no emu panel open — emu_boot first")?;
            panel
                .update(cx, |p, _| p.remote_pause())
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "paused": true }))
        }
        Cmd::Resume { .. } => {
            let panel = panel.ok_or("no emu panel open — emu_boot first")?;
            panel
                .update(cx, |p, _| p.remote_resume())
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "paused": false }))
        }
        Cmd::Step { frames, .. } => {
            let panel = panel.ok_or("no emu panel open — emu_boot first")?;
            panel
                .update(cx, |p, _| p.remote_step(frames))
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "stepped": frames }))
        }
        Cmd::Screenshot { .. } => {
            let panel = panel.ok_or("no emu panel open — emu_boot first")?;
            let (w, h, bgra) = panel
                .update(cx, |p, _| p.remote_screenshot())
                .map_err(|e| e.to_string())?
                .ok_or("no frame yet — boot a cart and let it present first")?;
            Ok(serde_json::json!({
                "width": w,
                "height": h,
                "bgra_base64": base64::engine::general_purpose::STANDARD.encode(&bgra),
            }))
        }
        Cmd::Uart { tail, .. } => {
            let panel = panel.ok_or("no emu panel open — emu_boot first")?;
            let lines = panel
                .update(cx, |p, _| p.remote_uart(tail))
                .map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "lines": lines }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_to_mask_maps_names_and_select() {
        let mask =
            buttons_to_mask(&["right".to_string(), "z".to_string(), "select".to_string()]).unwrap();
        assert_eq!(mask, (1 << 7) | (1 << 0) | SELECT_BIT);
        assert_eq!(buttons_to_mask(&[]).unwrap(), 0);
    }

    #[test]
    fn buttons_to_mask_rejects_unknown_names() {
        let err = buttons_to_mask(&["jump".to_string()]).unwrap_err();
        assert!(err.contains("unknown button \"jump\""), "{err}");
    }

    #[test]
    fn resolve_workspace_exact_single_and_ambiguous() {
        let keys = vec!["/a".to_string(), "/b".to_string()];
        assert_eq!(resolve_workspace(&keys, Some("/b")).unwrap(), "/b");
        assert!(resolve_workspace(&keys, Some("/c")).unwrap_err().contains("no workspace"));
        assert!(resolve_workspace(&keys, None).unwrap_err().contains("multiple workspaces"));
        assert_eq!(resolve_workspace(&keys[..1], None).unwrap(), "/a");
        assert!(resolve_workspace(&[], None).unwrap_err().contains("no emu panel"));
    }
}
