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
use ggo_emu_remote::protocol::{Cmd, Request, Response, parse_request, response_line};
use ggo_emu_remote::registry::{self, SessionInfo};

use crate::EmuPanel;
use crate::input::{SELECT_BIT, button_bit};
use crate::menu;

/// A pack is a cargo build, and it runs ON the single foreground dispatch
/// loop -- every other socket request queues behind it. Longer than the
/// bridge's own pack timeout, so the bridge gives up first and this only
/// guards against a wedged `emd` owning the socket for the rest of the
/// process's life.
const HOST_PACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// The most frames one `NextFrame` will step: ~16.7 minutes of cart time
/// at 60 fps. The wait scales with the count, so an unbounded `frames`
/// would let one request own the dispatch loop indefinitely.
const MAX_STEP_FRAMES: u32 = 60_000;

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
    let accept_executor = cx.background_executor().clone();
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

async fn dispatch(req: Request, cx: &mut AsyncApp) -> Response {
    let id = req.id;
    match dispatch_inner(req.cmd, cx).await {
        Ok(data) => Response::ok(id, data),
        Err(e) => Response::err(id, e),
    }
}

/// Poll the panel until its delivered-frame counter reaches
/// `target_frame`, the run dies (its failure status becomes the error),
/// or `timeout` passes — the emulator thread and frame pump are
/// asynchronous, so only this makes a reply mean "the frame happened".
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
                "timed out at frame {frame} waiting for frame {target_frame}"
            ));
        }
        cx.background_executor().timer(std::time::Duration::from_millis(5)).await;
    }
}

/// The world half of a lock-step reply: the cart's inspection JSON parsed
/// into a value (`null` for worlds that don't declare `InspectWorld`).
fn world_value(panel: &WeakEntity<EmuPanel>, cx: &mut AsyncApp) -> serde_json::Value {
    let dump = panel.update(cx, |p, _| p.remote_world_json()).ok().flatten();
    match dump {
        Some((seq, json)) => match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("tap_seq".to_string(), serde_json::json!(seq));
                }
                v
            }
            // Truncated tap (dump exceeded the cap): ship raw for debugging.
            Err(_) => serde_json::json!({ "tap_seq": seq, "truncated_raw": &**json }),
        },
        None => serde_json::Value::Null,
    }
}

/// A framebuffer as the wire carries it; the bridge turns it into PNG.
pub(crate) fn bgra_reply(width: u32, height: u32, bgra: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "width": width,
        "height": height,
        "bgra_base64": base64::engine::general_purpose::STANDARD.encode(bgra),
    })
}

/// The world dock `ggo_world_panel::init` registers on every workspace,
/// leased through its window. Every world the agent reaches goes through
/// it: there is one panel per open world tab now, and a world loaded
/// behind the dock's back would leave its tab list keyed on the wrong
/// path.
fn world_dock_for(
    workspace: &Entity<Workspace>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<Entity<ggo_world_panel::WorldDock>, String> {
    window
        .update(cx, |_, _window, app| {
            workspace
                .read(app)
                .panel::<ggo_world_panel::WorldDock>(app)
                .ok_or_else(|| "no world dock in this workspace".to_string())
        })
        .map_err(|e| e.to_string())?
}

/// The panel behind the ACTIVE world tab: what a `world_read` or
/// `world_screenshot` with no `world` argument answers from.
fn world_panel_for(
    workspace: &Entity<Workspace>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<Entity<ggo_world_panel::WorldPanel>, String> {
    let dock = world_dock_for(workspace, window, cx)?;
    window
        .update(cx, |_, _window, app| {
            dock.read(app)
                .active()
                .ok_or_else(|| "no world is open".to_string())
        })
        .map_err(|e| e.to_string())?
}

/// A panel that can answer questions about the PROJECT's worlds -- the
/// listing, and the resolution of the name the agent typed. Any panel
/// can: both only need the project root. The active tab's when there is
/// one, otherwise a throwaway, because zero open world tabs is the normal
/// state for the first call an agent makes.
///
/// MUST NOT be called while the workspace is leased: the listing reads
/// the workspace to find that root.
fn world_resolver(
    dock: &Entity<ggo_world_panel::WorldDock>,
    workspace: &Entity<Workspace>,
    app: &mut App,
) -> Entity<ggo_world_panel::WorldPanel> {
    dock.read(app).active().unwrap_or_else(|| {
        let workspace = workspace.downgrade();
        app.new(|cx| ggo_world_panel::WorldPanel::new(Some(workspace), cx))
    })
}

/// The panel showing `world` (a stem or a rel path), opening a tab for it
/// when none does, plus the worktree-relative path it resolved to.
///
/// A different world's unsaved edits are no longer a reason to refuse:
/// every world has its own tab, so nothing an agent opens can reload --
/// let alone discard -- what the user is editing next door.
fn world_panel_open(
    workspace: &Entity<Workspace>,
    window: AnyWindowHandle,
    world: &str,
    cx: &mut AsyncApp,
) -> Result<(Entity<ggo_world_panel::WorldPanel>, String), String> {
    let dock = world_dock_for(workspace, window, cx)?;
    window
        .update(cx, |_, window, app| {
            // Resolved BEFORE the workspace update below, never inside it:
            // resolving re-enumerates the project's worlds, which reads
            // the workspace, and reading it while leased panics.
            let rel = world_resolver(&dock, workspace, app)
                .update(app, |panel, cx| panel.remote_resolve(world, cx))?;
            // The panel comes back out of `open_world_in` itself rather than
            // off `dock.active()` afterwards: a dock that declined to open
            // anything would otherwise hand back whatever tab was in front
            // before, which is a different world than the agent asked for.
            let opened = workspace.update(app, |workspace, cx| {
                let mut opened = None;
                ggo_common::open_in_panel(
                    workspace,
                    window,
                    cx,
                    |dock: &mut ggo_world_panel::WorldDock, window, cx| {
                        // Design, never the user's sticky mode: an agent
                        // surveying a project opens a tab per world it
                        // reads, and in Live each of those boots its own
                        // headless emulator run for a picture nobody is
                        // looking at.
                        opened = dock.open_world_in(
                            &rel,
                            ggo_world_panel::OpenMode::Design,
                            window,
                            cx,
                        );
                    },
                );
                opened
            });
            opened
                .map(|panel| (panel, rel))
                .ok_or_else(|| "the world dock could not open a tab".to_string())
        })
        .map_err(|e| e.to_string())?
}

/// Run `read` against the World panel once the world it is loading has
/// landed. A tab the dock has just opened is `Loading` with the file read
/// still off-thread, so the first ask can legitimately answer "still
/// loading"; every other failure is final and returns straight away.
/// Bounded at 5s so a load that never completes answers the agent
/// instead of hanging its tool call.
async fn await_world_ready<T>(
    panel: &Entity<ggo_world_panel::WorldPanel>,
    cx: &mut AsyncApp,
    read: impl Fn(&ggo_world_panel::WorldPanel) -> Result<T, String>,
) -> Result<T, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match panel.update(cx, |p, _| read(p)) {
            Ok(value) => return Ok(value),
            Err(reason)
                if reason.contains(ggo_world_panel::WORLD_STILL_LOADING)
                    && std::time::Instant::now() < deadline =>
            {
                cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
            }
            Err(reason) => return Err(reason),
        }
    }
}

/// The workspace's emu panel, opening one if none is up.
///
/// Two steps on purpose: the panel is opened INSIDE a `workspace.update`
/// and then driven outside it, because the flash path reads the
/// workspace itself (its root, and the world panel's open document) and
/// would lease it twice from within.
fn panel_or_open(
    target_root: &str,
    panel: Option<WeakEntity<EmuPanel>>,
    workspace: Option<Entity<Workspace>>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<WeakEntity<EmuPanel>, String> {
    if let Some(panel) = panel {
        return Ok(panel);
    }
    let workspace = workspace.ok_or("workspace vanished")?;
    let panel = window
        .update(cx, |_, window, app| {
            workspace.update(app, |workspace, cx| {
                crate::open_emu_item(workspace, window, cx, |_, _, _| {});
                workspace
                    .items_of_type::<crate::EmulatorItem>(cx)
                    .next()
                    .map(|item| item.read(cx).panel().downgrade())
                    .ok_or_else(|| "emu panel did not open".to_string())
            })
        })
        .map_err(|e| e.to_string())??;
    // Registered here rather than left to the panel's own deferred
    // `refresh_root`, so the very next `flash_status` finds it.
    cx.update(|cx| {
        if cx.try_global::<RemotePanels>().is_none() {
            cx.set_global(RemotePanels::default());
        }
        cx.global_mut::<RemotePanels>()
            .panels
            .insert(target_root.to_string(), (panel.clone(), Some(window)));
    });
    Ok(panel)
}

/// Boot `cart` on the workspace's emu panel, reopening a closed panel,
/// and register it with `RemotePanels` so later commands find it.
///
/// Shared by lock-step `Start` and free-running `Run`: the difference
/// between them is what they do to the run AFTER it boots, never how it
/// boots.
///
/// The tab comes to the front on the way: a remote boot is a heavy
/// action the user should be looking at, and an emulator already open
/// behind another tab would otherwise be driven where nobody can see it.
async fn boot_cart(
    panel: Option<WeakEntity<EmuPanel>>,
    workspace: Option<Entity<Workspace>>,
    target_root: &str,
    cart: String,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<WeakEntity<EmuPanel>, String> {
    let root = PathBuf::from(target_root);
    // The workspace path covers BOTH "no panel yet" and "the panel is
    // open in a background tab": `open_emu_item` collapses the center
    // splits and activates (or creates) the emulator tab, and the panel
    // it hands the closure is the one that tab shows — booting the
    // separately-registered handle instead could drive a panel the user
    // is not looking at. Only a workspace that has gone away falls back
    // to that handle.
    let booted: Result<WeakEntity<EmuPanel>, String> = match (workspace, panel) {
        (Some(workspace), _) => window
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
        (None, Some(panel)) => window
            .update(cx, |_, window, app| {
                panel
                    .update(app, |p, cx| p.remote_boot(cart, root, window, cx))
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                    .map(|()| panel.clone())
            })
            .map_err(|e| e.to_string())?,
        (None, None) => Err("workspace vanished".to_string()),
    };
    let panel = booted?;
    // Register the booted panel deterministically — its own deferred
    // refresh_root registration only fires on later UI activity.
    cx.update(|cx| {
        if cx.try_global::<RemotePanels>().is_none() {
            cx.set_global(RemotePanels::default());
        }
        cx.global_mut::<RemotePanels>()
            .panels
            .insert(target_root.to_string(), (panel.clone(), Some(window)));
    });
    Ok(panel)
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
                run_kind: ggo_emu_remote::protocol::RunKind::Cart,
                world: None,
            }));
        }
        return Ok(serde_json::json!({ "pid": std::process::id(), "workspaces": rows }));
    }

    let workspace_arg = match &cmd {
        Cmd::Status => unreachable!("handled above"),
        Cmd::Start { workspace, .. }
        | Cmd::NextFrame { workspace, .. }
        | Cmd::Stop { workspace }
        | Cmd::FlashWorld { workspace, .. }
        | Cmd::FlashStatus { workspace }
        | Cmd::OpenReport { workspace, .. }
        | Cmd::CloseReport { workspace, .. }
        | Cmd::HwEnv { workspace }
        | Cmd::FlashCancel { workspace }
        | Cmd::Screenshot { workspace }
        | Cmd::Uart { workspace, .. }
        | Cmd::Run { workspace, .. }
        | Cmd::Pause { workspace }
        | Cmd::Resume { workspace }
        | Cmd::Debug { workspace, .. }
        | Cmd::PackWorld { workspace, .. }
        | Cmd::WorldList { workspace }
        | Cmd::WorldOpen { workspace, .. }
        | Cmd::WorldRead { workspace, .. }
        | Cmd::WorldScreenshot { workspace, .. } => workspace.clone(),
    };
    let keys: Vec<String> = targets.iter().map(|t| t.root.clone()).collect();
    let target_root = resolve_workspace(&keys, workspace_arg.as_deref())?;
    let target = targets
        .into_iter()
        .find(|t| t.root == target_root)
        .expect("resolve_workspace returned a member of keys");
    // Answered before the window is demanded: a status read needs
    // neither a window nor an open panel -- a workspace nobody has
    // opened the emulator in has simply never flashed.
    if let Cmd::FlashStatus { .. } = cmd {
        let payload = match &target.panel {
            Some(panel) => panel
                .update(cx, |p, _| p.remote_flash_status())
                .map_err(|e| e.to_string())?,
            None => ggo_emu_remote::protocol::FlashStatusPayload::default(),
        };
        return Ok(serde_json::to_value(payload).expect("FlashStatusPayload serializes"));
    }
    // Likewise, and it must answer BEFORE anything is opened: this is the
    // tool an agent calls to find out whether flashing is possible at
    // all, so with no panel registered it runs the same probe the panel
    // would have run rather than demanding one be opened first.
    if let Cmd::HwEnv { .. } = cmd {
        let payload = match &target.panel {
            Some(panel) => panel.update(cx, |p, _| p.remote_env()).map_err(|e| e.to_string())?,
            None => crate::hardware::probe(
                Some(std::path::Path::new(&target_root)),
                std::env::var("PATH").ok().as_deref(),
                &std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default(),
            )
            .remote_payload(),
        };
        return Ok(serde_json::to_value(payload).expect("HwEnvPayload serializes"));
    }
    // Also pre-window: cancelling touches the panel's own state and no
    // window at all, and a flash in flight is exactly when demanding a
    // window would deny the caller the one thing this tool is for.
    if let Cmd::FlashCancel { .. } = cmd {
        // No panel registered is the strongest possible "nothing is
        // flashing", and the tool promises `{cancelled: false}` for that
        // -- erroring instead would fail the one call whose whole job is
        // to be safe to make when the caller is unsure.
        let Some(panel) = target.panel else {
            return Ok(serde_json::json!({ "cancelled": false }));
        };
        let cancelled = panel
            .update(cx, |p, cx| p.remote_flash_cancel(cx))
            .map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({ "cancelled": cancelled }));
    }
    // Pre-window too, and deliberately not lock-step: both read state the
    // panel already holds, so an agent can look at a free-running run
    // without stepping it or stopping it.
    if let Cmd::Screenshot { .. } = cmd {
        let panel = target.panel.ok_or("no emu panel open in this workspace")?;
        let shot = panel.update(cx, |p, _| p.remote_screenshot()).map_err(|e| e.to_string())?;
        let (width, height, bgra) = shot.ok_or("no frame presented yet — start a run first")?;
        return Ok(bgra_reply(width, height, &bgra));
    }
    if let Cmd::Uart { tail, .. } = &cmd {
        let panel = target.panel.ok_or("no emu panel open in this workspace")?;
        let lines = panel.update(cx, |p, _| p.remote_uart(*tail)).map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({ "lines": lines }));
    }
    // Pausing and resuming likewise only touch the live session, so they
    // stay usable on a run whose window the caller cannot supply.
    if matches!(cmd, Cmd::Pause { .. } | Cmd::Resume { .. }) {
        let panel = target.panel.ok_or("no run live — emu_start first")?;
        let paused = matches!(cmd, Cmd::Pause { .. });
        panel
            .update(cx, |p, _| if paused { p.remote_pause() } else { p.remote_resume() })
            .map_err(|e| e.to_string())??;
        let (frame, running, _) =
            panel.update(cx, |p, _| p.remote_progress()).map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({ "paused": paused, "frame": frame, "running": running }));
    }
    // Pre-window as well: every inspector view is decoded from the
    // snapshot the drive thread already published, so an agent can look
    // at a run it did not start and has no window handle for.
    if let Cmd::Debug { view, bank, palette, layer, .. } = &cmd {
        let panel = target.panel.ok_or("no run live — emu_start first")?;
        return panel
            .update(cx, |p, _| p.remote_debug_view(*view, *bank, *palette, *layer))
            .map_err(|e| e.to_string())?;
    }

    let window = target.window.ok_or("workspace has no window (headless test?)")?;

    match cmd {
        Cmd::Status
        | Cmd::FlashStatus { .. }
        | Cmd::HwEnv { .. }
        | Cmd::FlashCancel { .. }
        | Cmd::Screenshot { .. }
        | Cmd::Uart { .. }
        | Cmd::Pause { .. }
        | Cmd::Resume { .. }
        | Cmd::Debug { .. } => unreachable!("handled above"),
        Cmd::Start { cart, freerun: false, .. } => {
            let panel =
                boot_cart(target.panel, target.workspace, &target_root, cart, window, cx).await?;
            // Arm the cart's inspection tap FIRST — only remote lock-step
            // runs serialize; the panel's own Run button never arms it.
            panel
                .update(cx, |p, _| p.remote_enable_inspect())
                .map_err(|e| e.to_string())??;
            // Only a delivered frame proves the boot took, then park at
            // the next boundary and settle: that parked frame is the
            // lock-step baseline.
            await_frame(&panel, cx, 1, std::time::Duration::from_secs(5)).await?;
            panel.update(cx, |p, _| p.remote_pause()).map_err(|e| e.to_string())??;
            cx.background_executor().timer(std::time::Duration::from_millis(60)).await;
            let mut frame = panel
                .update(cx, |p, _| p.remote_progress().0)
                .map_err(|e| e.to_string())?;
            let mut world = world_value(&panel, cx);
            if world.is_null() {
                // The pause can land before any frame ran with the tap
                // armed; one paused step produces the first dump (still
                // null after that = cart built without the inspect
                // feature).
                panel.update(cx, |p, _| p.remote_step(1)).map_err(|e| e.to_string())??;
                frame = await_frame(&panel, cx, frame + 1, std::time::Duration::from_secs(3)).await?;
                world = world_value(&panel, cx);
            }
            Ok(serde_json::json!({ "started": true, "frame": frame, "world": world }))
        }
        Cmd::Run { cart, .. } | Cmd::Start { cart, freerun: true, .. } => {
            // The same boot as lock-step `Start`, minus the tap and the
            // pause: this run plays itself, and a delivered frame is all
            // the reply has to prove.
            let panel =
                boot_cart(target.panel, target.workspace, &target_root, cart, window, cx).await?;
            let frame = await_frame(&panel, cx, 1, std::time::Duration::from_secs(5)).await?;
            Ok(serde_json::json!({ "started": true, "frame": frame, "running": true }))
        }
        Cmd::NextFrame { buttons, screenshot, frames, .. } => {
            let panel = target.panel.ok_or("no run in lock-step — emu_start first")?;
            let mask = buttons_to_mask(&buttons)?;
            let frames = frames.unwrap_or(1).clamp(1, MAX_STEP_FRAMES);
            let start = panel
                .update(cx, |p, _| -> Result<u32, String> {
                    p.remote_input(mask)?;
                    p.remote_step(frames)?;
                    Ok(p.remote_progress().0)
                })
                .map_err(|e| e.to_string())??;
            // The stepped frames are paced at one frame time each, so the
            // wait has to grow with the count or a long step times out
            // mid-run.
            let frame = await_frame(
                &panel,
                cx,
                start.saturating_add(frames),
                std::time::Duration::from_secs(3) + crate::drive::FRAME_TIME * frames * 3,
            )
            .await?;
            let world = world_value(&panel, cx);
            let mut reply = serde_json::json!({ "frame": frame, "world": world });
            if screenshot {
                if let Ok(Some((w, h, bgra))) = panel.update(cx, |p, _| p.remote_screenshot()) {
                    reply["screenshot"] = bgra_reply(w, h, &bgra);
                }
            }
            Ok(reply)
        }
        Cmd::FlashWorld { config, .. } => {
            // Like a remote boot, a remote flash must not need a human to
            // have opened the emulator first -- the run needs a panel to
            // live on, not a panel someone clicked.
            let panel =
                panel_or_open(&target_root, target.panel, target.workspace, window, cx)?;
            // Returns as soon as the child is spawned: the reply says the
            // flash STARTED, and `flash_status` says how it is going.
            let effective = window
                .update(cx, |_, window, app| {
                    panel
                        .update(app, |p, cx| p.remote_flash(config, window, cx))
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                })
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "started": true, "config": effective }))
        }
        Cmd::OpenReport { run, fault, .. } => {
            // Refused before a tab is opened: an OpenReport naming neither
            // would otherwise pop the Reports panel on nothing.
            if run.is_none() && fault.is_none() {
                return Err("open_ggo_report needs run or fault".to_string());
            }
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let opened = fault.clone();
            window
                .update(cx, |_, window, app| {
                    workspace.update(app, |workspace, cx| {
                        ggo_charts_panel::open_charts_item(workspace, window, cx, |charts, _, cx| {
                            // `run` wins when both are given, matching the
                            // bridge; the neither case returned above.
                            if let Some(run) = run {
                                charts.open_run(run, cx);
                            } else if let Some(fault) = opened {
                                charts.open_fault(fault, cx);
                            }
                        });
                    })
                })
                .map_err(|e| e.to_string())?;
            // "requested", not "shown": the panel looks the report up
            // off-thread and lands on the runs list if its own database
            // has no such run or fault.
            Ok(serde_json::json!({ "requested": true, "run": run, "fault": fault }))
        }
        Cmd::CloseReport { run, .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let closed = window
                .update(cx, |_, window, app| {
                    workspace.update(app, |workspace, cx| {
                        ggo_charts_panel::close_charts_item(workspace, run, window, cx)
                    })
                })
                .map_err(|e| e.to_string())??;
            Ok(serde_json::json!({ "closed": closed }))
        }
        Cmd::PackWorld { world, .. } => {
            // Like a remote boot: the pack needs a panel to plan on, not a
            // panel someone clicked.
            let panel = panel_or_open(&target_root, target.panel, target.workspace, window, cx)?;
            let (request, runner, cart) = panel
                .update(cx, |p, cx| p.remote_pack_plan(&world, cx))
                .map_err(|e| e.to_string())??;
            // BLOCKING child: the runner is the panel's own (a test's fake
            // or the system one), run off the UI thread. Detached and
            // reported over a channel rather than awaited as a `Task`,
            // because dropping a `Task` cancels it: on timeout the child
            // must be left to finish its writes, not killed mid-pack.
            let (finished, wait) = smol::channel::bounded(1);
            cx.background_spawn(async move {
                finished.send(runner(request)).await.ok();
            })
            .detach();
            let timer = cx.background_executor().timer(HOST_PACK_TIMEOUT);
            let outcome = smol::future::or(
                async { Some(wait.recv().await) },
                async move {
                    timer.await;
                    None
                },
            )
            .await;
            let capture = match outcome {
                Some(Ok(capture)) => capture,
                Some(Err(e)) => return Err(format!("pack reported nothing: {e}")),
                None => {
                    return Err(format!(
                        "pack still running after {}s — it keeps going in the background; check target/ggo-emulate/ later",
                        HOST_PACK_TIMEOUT.as_secs()
                    ));
                }
            };
            if !capture.ok {
                return Err(format!("pack failed: {}", menu::failure_reason(&capture)));
            }
            let tail: Vec<&String> =
                capture.lines.iter().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
            Ok(serde_json::json!({ "cart": cart, "world": world, "lines": tail }))
        }
        Cmd::WorldList { .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let dock = world_dock_for(&workspace, window, cx)?;
            let worlds = window
                .update(cx, |_, _window, app| {
                    world_resolver(&dock, &workspace, app).update(app, |p, cx| p.remote_list(cx))
                })
                .map_err(|e| e.to_string())?;
            let rows: Vec<serde_json::Value> = worlds
                .into_iter()
                .map(|(stem, rel_path)| serde_json::json!({ "stem": stem, "rel_path": rel_path }))
                .collect();
            Ok(serde_json::json!({ "worlds": rows }))
        }
        Cmd::WorldOpen { world, .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let (_panel, rel) = world_panel_open(&workspace, window, &world, cx)?;
            Ok(serde_json::json!({ "opened": rel }))
        }
        Cmd::WorldRead { world, .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let panel = match world {
                Some(world) => world_panel_open(&workspace, window, &world, cx)?.0,
                None => world_panel_for(&workspace, window, cx)?,
            };
            await_world_ready(&panel, cx, |p| p.remote_read()).await
        }
        Cmd::WorldScreenshot { world, full, .. } => {
            let workspace = target.workspace.ok_or("workspace vanished")?;
            let panel = match world {
                Some(world) => world_panel_open(&workspace, window, &world, cx)?.0,
                None => world_panel_for(&workspace, window, cx)?,
            };
            let (width, height, bgra) =
                await_world_ready(&panel, cx, |p| p.remote_screenshot(full)).await?;
            Ok(bgra_reply(width, height, &bgra))
        }
        Cmd::Stop { .. } => {
            let panel = target.panel.ok_or("no emu panel open in this workspace")?;
            let uart = panel.update(cx, |p, _| p.remote_uart(None)).unwrap_or_default();
            window
                .update(cx, |_, window, app| {
                    panel.update(app, |p, cx| p.remote_stop(window, cx)).ok();
                })
                .ok();
            Ok(serde_json::json!({ "stopped": true, "uart": uart }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    /// A workspace in a window, registered the way production's
    /// `observe_new` in [`init`] does -- `init` registers nothing under
    /// `cfg(test)`, so the dispatch path has no target without this.
    ///
    /// The project root is a REAL directory: the world panel's loader
    /// reads worlds off the real filesystem, so the fake worktree is
    /// mirrored onto the same path the fixture was written to.
    async fn remote_workspace<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (Entity<Workspace>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            crate::init(cx);
            ggo_world_panel::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            serde_json::json!({
                "emerald.toml": "",
                "assets": { "worlds": { "main.toml": "" } },
            }),
        )
        .await;
        let project = Project::test(fs, [root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let handle = cx.update(|window, _cx| window.window_handle());
        let id = workspace.entity_id().as_u64();
        let weak = workspace.downgrade();
        cx.update(|_window, cx| {
            if cx.try_global::<RemotePanels>().is_none() {
                cx.set_global(RemotePanels::default());
            }
            cx.global_mut::<RemotePanels>()
                .workspaces
                .insert(id, (weak, Some(handle)));
        });
        (workspace, cx)
    }

    /// `world_open` is the FIRST call an agent makes, with no world tab
    /// open and nothing for `dock.active()` to answer with: it has to
    /// resolve the name against the project anyway, open the tab, and
    /// leave `world_read` able to answer from it.
    #[gpui::test]
    async fn world_open_opens_the_first_tab_and_world_read_answers_from_it(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\n").unwrap();
        std::fs::write(
            dir.path().join("assets/worlds/main.toml"),
            "[[entity]]\nTransform = { pos = [4.0, 4.0], z = 0.0 }\n",
        )
        .unwrap();
        let (workspace, cx) = remote_workspace(cx, dir.path()).await;
        let mut async_cx = cx.to_async();

        let opened = dispatch_inner(
            Cmd::WorldOpen {
                workspace: None,
                world: "worlds/main".to_string(),
            },
            &mut async_cx,
        )
        .await
        .expect("world_open with no tab open");
        assert_eq!(opened["opened"], "assets/worlds/main.toml");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .items_of_type::<ggo_world_panel::WorldCanvasItem>(cx)
                    .count(),
                1,
                "the open gave the world its own center tab"
            );
        });

        let read = dispatch_inner(
            Cmd::WorldRead {
                workspace: None,
                world: None,
            },
            &mut async_cx,
        )
        .await
        .expect("world_read answers from the tab world_open left active");
        assert_eq!(read["rel_path"], "assets/worlds/main.toml");
        assert_eq!(read["entities"].as_array().map(Vec::len), Some(1));
    }

    /// An agent surveying a project opens a tab per world it reads, and
    /// the user's sticky mode is Live: without the Design override each
    /// of those tabs boots a headless emulator run for a picture nobody
    /// is looking at.
    #[gpui::test]
    async fn world_open_comes_up_in_design_and_boots_no_viewer(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\n").unwrap();
        std::fs::write(dir.path().join("assets/worlds/main.toml"), "").unwrap();
        let (workspace, cx) = remote_workspace(cx, dir.path()).await;
        // A fake `emd`, so a regression registers a run here instead of
        // shelling out to the real binary.
        cx.update(|_, cx| {
            cx.set_global(crate::viewer_run::TestViewerRunner(std::sync::Arc::new(|_| {
                ggo_common::ProcCapture {
                    ok: false,
                    lines: vec!["no build in this test".to_string()],
                }
            })));
            cx.set_global(crate::viewer_run::TestViewerRoot(dir.path().to_path_buf()));
        });
        let mut async_cx = cx.to_async();

        dispatch_inner(
            Cmd::WorldOpen {
                workspace: None,
                world: "worlds/main".to_string(),
            },
            &mut async_cx,
        )
        .await
        .expect("world_open");
        cx.run_until_parked();

        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<ggo_world_panel::WorldDock>(cx)
                .expect("the dock")
                .read(cx)
                .active()
                .expect("world_open opened a tab")
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.canvas_mode()),
            ggo_world_panel::CanvasMode::Design,
            "an agent's open is a read, not a session"
        );
        cx.update(|_, cx| {
            assert!(
                cx.try_global::<crate::viewer_run::ViewerRuns>()
                    .is_none_or(|runs| runs.runs.is_empty()),
                "and no viewer cart was built for it"
            );
        });
    }

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
