//! Agent remote-control host: a per-process unix socket (advertised via
//! `ggo_emu_remote::registry`) through which the `zedgg-emu-mcp` bridge
//! runs complete emulation scripts in this process's emu panels: boot a
//! cart, apply frame-scheduled inputs, capture labeled screenshots, then
//! stop and report. Script-only by design — no interactive drive surface
//! that could leave an emulator half-driven between tool calls.
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
use ggo_emu_remote::protocol::{Cmd, Request, Response, Script, Shot, parse_request, response_line};
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
    /// Roots as last written to the on-disk advertisement, so dispatch
    /// only rewrites the file when the set actually changes.
    advertised: Vec<String>,
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
    if let Err(e) = registry::publish(&dir, &SessionInfo { pid, socket, workspaces }) {
        log::warn!("zedgg-emu remote: advertising under {} failed: {e}", dir.display());
    }
}

/// Start the socket host. Called once from `ggo_emu_panel::init`.
pub fn init(cx: &mut App) {
    // Test processes must not advertise into (or bind sockets in) the
    // developer's real registry — a bridge would try to drive them.
    if cfg!(test) {
        return;
    }
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

/// Upper bound on one script's length: two minutes of frames. Long
/// soaks should be several scripts, each reporting.
const MAX_SCRIPT_FRAMES: u32 = 7200;

async fn dispatch(req: Request, cx: &mut AsyncApp) -> Response {
    let id = req.id;
    match dispatch_inner(req.cmd, cx).await {
        Ok(data) => Response::ok(id, data),
        Err(e) => Response::err(id, e),
    }
}

/// Poll the panel until its delivered-frame counter reaches
/// `target_frame`, the run dies (its failure status becomes the error),
/// or `timeout` passes. This is what makes the report mean "it actually
/// happened": the emulator thread and the frame pump are asynchronous.
async fn await_frame(
    panel: &WeakEntity<EmuPanel>,
    cx: &mut AsyncApp,
    target_frame: u32,
    timeout: std::time::Duration,
) -> Result<u32, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let (frame, running, error) = panel
            .update(cx, |p, _| p.remote_progress())
            .map_err(|e| e.to_string())?;
        if let Some(error) = error {
            return Err(error);
        }
        if frame >= target_frame {
            return Ok(frame);
        }
        if !running {
            return Err(format!("run ended at frame {frame} before frame {target_frame}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out at frame {frame} waiting for frame {target_frame} — is the run auto-paused (emulator tab hidden)?"
            ));
        }
        cx.background_executor().timer(std::time::Duration::from_millis(10)).await;
    }
}

/// Validate a script before touching the emulator: frame bounds, button
/// vocabulary, and (for now) the reserved event slot.
fn validate_script(script: &Script) -> Result<(), String> {
    if script.frames == 0 || script.frames > MAX_SCRIPT_FRAMES {
        return Err(format!("frames must be 1..={MAX_SCRIPT_FRAMES}, got {}", script.frames));
    }
    for step in &script.steps {
        if step.at > script.frames {
            return Err(format!("step at frame {} is past the script's {} frames", step.at, script.frames));
        }
        if let Some(buttons) = &step.input {
            buttons_to_mask(buttons)?;
        }
        if step.event.is_some() {
            return Err(
                "scripted world events (component insertion/removal, world edits) are reserved but not implemented yet — the engine has no mid-run mutation channel"
                    .to_string(),
            );
        }
    }
    Ok(())
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
        // A closed panel must fall through to the workspace path (which
        // reopens it) rather than shadowing its root with a dead entity.
        let (live, dead): (Vec<_>, Vec<_>) =
            panels.into_iter().partition(|(_, p, _)| p.upgrade().is_some());
        if !dead.is_empty() {
            let g = cx.global_mut::<RemotePanels>();
            for (root, ..) in &dead {
                g.panels.remove(root);
            }
        }
        let mut targets: Vec<Target> = live
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
        // Keep the on-disk advertisement in step with live worktrees —
        // rewritten only when the root set actually changed.
        let mut roots: Vec<String> = targets.iter().map(|t| t.root.clone()).collect();
        roots.sort();
        roots.dedup();
        if cx.try_global::<RemotePanels>().is_some_and(|g| g.advertised != roots) {
            cx.global_mut::<RemotePanels>().advertised = roots.clone();
            publish_advertisement_roots(roots);
        }
        targets
    });

    let (workspace_arg, script) = match cmd {
        Cmd::Status => {
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
        Cmd::Script { workspace, script } => (workspace, script),
    };
    validate_script(&script)?;

    let keys: Vec<String> = targets.iter().map(|t| t.root.clone()).collect();
    let target_root = resolve_workspace(&keys, workspace_arg.as_deref())?;
    let target = targets
        .into_iter()
        .find(|t| t.root == target_root)
        .expect("resolve_workspace returned a member of keys");
    let window = target.window.ok_or("workspace has no window (headless test?)")?;
    let root = std::path::PathBuf::from(&target.root);

    // ---- start: boot (opening the panel if none is live) -------------
    let cart = script.cart.clone();
    let booted: Result<WeakEntity<EmuPanel>, String> = match (target.panel, target.workspace) {
        (Some(panel), _) => window
            .update(cx, |_, window, app| {
                panel
                    .update(app, |p, cx| p.remote_boot(cart, root, window, cx))
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                    .map(|()| panel.clone())
            })
            .map_err(|e| e.to_string())?,
        (None, Some(workspace)) => window
            .update(cx, |_, window, app| {
                workspace.update(app, |workspace, cx| {
                    let mut outcome = Err("emu panel did not open".to_string());
                    crate::open_emu_item(workspace, window, cx, |panel, window, cx| {
                        outcome = panel.remote_boot(cart, root, window, cx);
                    });
                    let panel = workspace
                        .items_of_type::<crate::EmulatorItem>(cx)
                        .next()
                        .map(|item| item.read(cx).panel().downgrade());
                    outcome.and_then(|()| panel.ok_or("emu panel did not open".to_string()))
                })
            })
            .map_err(|e| e.to_string())?,
        (None, None) => Err("workspace vanished".to_string()),
    };
    let panel = booted?;
    cx.update(|cx| {
        if cx.try_global::<RemotePanels>().is_none() {
            cx.set_global(RemotePanels::default());
        }
        cx.global_mut::<RemotePanels>()
            .panels
            .insert(target_root.clone(), (panel.clone(), Some(window)));
    });
    // Only a delivered frame proves the boot took (load failures surface
    // asynchronously on the emulator thread).
    await_frame(&panel, cx, 1, std::time::Duration::from_secs(5)).await?;

    // Park, let the pause land, and take the baseline frame: script
    // frame 0 is wherever the pause settled.
    panel.update(cx, |p, _| p.remote_pause()).map_err(|e| e.to_string())??;
    cx.background_executor().timer(std::time::Duration::from_millis(60)).await;
    let baseline = panel
        .update(cx, |p, _| p.remote_progress().0)
        .map_err(|e| e.to_string())?;

    // ---- body: step to each mark, then apply its actions -------------
    let mut marks: std::collections::BTreeMap<u32, Vec<&ggo_emu_remote::protocol::Step>> =
        std::collections::BTreeMap::new();
    for step in &script.steps {
        marks.entry(step.at).or_default().push(step);
    }
    marks.entry(script.frames).or_default(); // implicit finish mark

    let mut shots: Vec<Shot> = Vec::new();
    let mut current: u32 = 0;
    let run_result: Result<(), String> = async {
        for (&at, steps) in &marks {
            let chunk = at - current;
            if chunk > 0 {
                panel
                    .update(cx, |p, _| p.remote_step(chunk))
                    .map_err(|e| e.to_string())??;
                let timeout = std::time::Duration::from_millis(100 * u64::from(chunk) + 2000);
                await_frame(&panel, cx, baseline + at, timeout).await?;
            }
            current = at;
            for step in steps {
                if let Some(buttons) = &step.input {
                    let mask = buttons_to_mask(buttons)?;
                    panel
                        .update(cx, |p, _| p.remote_input(mask))
                        .map_err(|e| e.to_string())??;
                }
                if let Some(label) = &step.screenshot {
                    let (width, height, bgra) = panel
                        .update(cx, |p, _| p.remote_screenshot())
                        .map_err(|e| e.to_string())?
                        .ok_or("no frame available for screenshot")?;
                    shots.push(Shot {
                        label: label.clone(),
                        at,
                        width,
                        height,
                        bgra_base64: base64::engine::general_purpose::STANDARD.encode(&bgra),
                    });
                }
            }
        }
        Ok(())
    }
    .await;

    // ---- finish: always capture the final frame and stop the run -----
    if run_result.is_ok() {
        if let Ok(Some((width, height, bgra))) = panel.update(cx, |p, _| p.remote_screenshot()) {
            shots.push(Shot {
                label: "final".to_string(),
                at: current,
                width,
                height,
                bgra_base64: base64::engine::general_purpose::STANDARD.encode(&bgra),
            });
        }
    }
    let uart = panel
        .update(cx, |p, _| p.remote_uart(None))
        .unwrap_or_default();
    window
        .update(cx, |_, window, app| {
            panel.update(app, |p, cx| p.remote_stop(window, cx)).ok();
        })
        .ok();

    run_result?;
    Ok(serde_json::json!({
        "frames": script.frames,
        "screenshots": shots,
        "uart": uart,
    }))
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
    fn validate_script_rejects_bad_frames_marks_buttons_and_events() {
        use ggo_emu_remote::protocol::{Script, Step};
        let ok = Script {
            cart: "a.ggo".into(),
            frames: 120,
            steps: vec![Step { at: 0, input: Some(vec!["right".into()]), ..Default::default() }],
        };
        assert_eq!(validate_script(&ok), Ok(()));

        let zero = Script { cart: "a.ggo".into(), frames: 0, steps: vec![] };
        assert!(validate_script(&zero).unwrap_err().contains("frames"));

        let too_long = Script { cart: "a.ggo".into(), frames: MAX_SCRIPT_FRAMES + 1, steps: vec![] };
        assert!(validate_script(&too_long).unwrap_err().contains("frames"));

        let past_end = Script {
            cart: "a.ggo".into(),
            frames: 10,
            steps: vec![Step { at: 11, ..Default::default() }],
        };
        assert!(validate_script(&past_end).unwrap_err().contains("past"));

        let bad_button = Script {
            cart: "a.ggo".into(),
            frames: 10,
            steps: vec![Step { at: 0, input: Some(vec!["jump".into()]), ..Default::default() }],
        };
        assert!(validate_script(&bad_button).unwrap_err().contains("unknown button"));

        let event = Script {
            cart: "a.ggo".into(),
            frames: 10,
            steps: vec![Step { at: 0, event: Some(serde_json::json!({"insert": {}})), ..Default::default() }],
        };
        assert!(validate_script(&event).unwrap_err().contains("not implemented"));
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
