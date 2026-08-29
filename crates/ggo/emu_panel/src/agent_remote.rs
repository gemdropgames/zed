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
use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Global, WeakEntity};
use ggo_emu_remote::protocol::{Cmd, Request, Response, parse_request, response_line};
use ggo_emu_remote::registry::{self, SessionInfo};

use crate::EmuPanel;
use crate::input::{SELECT_BIT, button_bit};

/// Foreground registry of live panels, keyed by absolute project root.
#[derive(Default)]
pub struct RemotePanels {
    panels: HashMap<String, (WeakEntity<EmuPanel>, Option<AnyWindowHandle>)>,
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
    let dir = registry::dir();
    let pid = std::process::id();
    let (_, socket) = registry::session_paths(&dir, pid);
    registry::publish(&dir, &SessionInfo { pid, socket, workspaces }).ok();
}

/// Start the socket host. Called once from `ggo_emu_panel::init`.
pub fn init(cx: &mut App) {
    cx.set_global(RemotePanels::default());
    publish_advertisement(cx);

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
    let entries: Vec<(String, WeakEntity<EmuPanel>, Option<AnyWindowHandle>)> = cx
        .update(|cx| {
            cx.try_global::<RemotePanels>()
                .map(|g| {
                    g.panels
                        .iter()
                        .map(|(k, (p, w))| (k.clone(), p.clone(), *w))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });

    if let Cmd::Status = cmd {
        let mut rows = Vec::new();
        for (root, panel, _) in &entries {
            if let Ok(status) = panel.update(cx, |p, _| p.remote_status(root.clone())) {
                rows.push(status);
            }
        }
        return Ok(serde_json::json!({ "pid": std::process::id(), "workspaces": rows }));
    }

    let keys: Vec<String> = entries.iter().map(|(k, ..)| k.clone()).collect();
    let target = resolve_workspace(&keys, workspace_of(&cmd))?;
    let (_, panel, window) = entries
        .into_iter()
        .find(|(k, ..)| *k == target)
        .expect("resolve_workspace returned a member of keys");

    match cmd {
        Cmd::Status => unreachable!("handled above"),
        Cmd::Boot { cart, .. } => {
            let window = window.ok_or("panel has no window (headless test?)")?;
            window
                .update(cx, |_, window, app| {
                    panel.update(app, |p, cx| p.remote_boot(cart, window, cx))
                })
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "booted": true }))
        }
        Cmd::Stop { .. } => {
            let window = window.ok_or("panel has no window (headless test?)")?;
            window
                .update(cx, |_, window, app| {
                    panel.update(app, |p, cx| p.remote_stop(window, cx))
                })
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "stopped": true }))
        }
        Cmd::Input { buttons, .. } => {
            let mask = buttons_to_mask(&buttons)?;
            panel
                .update(cx, |p, _| p.remote_input(mask))
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "mask": mask }))
        }
        Cmd::Pause { .. } => {
            panel
                .update(cx, |p, _| p.remote_pause())
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "paused": true }))
        }
        Cmd::Resume { .. } => {
            panel
                .update(cx, |p, _| p.remote_resume())
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "paused": false }))
        }
        Cmd::Step { frames, .. } => {
            panel
                .update(cx, |p, _| p.remote_step(frames))
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "stepped": frames }))
        }
        Cmd::Screenshot { .. } => {
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
