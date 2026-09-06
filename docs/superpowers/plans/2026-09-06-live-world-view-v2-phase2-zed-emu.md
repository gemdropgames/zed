# Live World View v2 — Phase 2 (headless viewer runs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A world view's viewer cart runs in its own headless emulator entity, so N world views can each have one and none of them opens or touches the emulator tab.

**Architecture:** New `ViewerRun` entity in `ggo_emu_panel` (build task + `drive::Session` + frame pump + own watch-rebuild subscription) publishing into the `ggo_common::LinkEndpoint` it was handed. The registered `ViewerBooter` creates one per call and parks it in a global registry that prunes stopped runs. Every viewer code path leaves `EmuPanel`.

**Tech Stack:** Rust, GPUI, `ggo_common` (`LinkEndpoint`, `ProcRunner`), `drive::Session`, `project::Event::WorktreeUpdatedEntries`.

**Spec:** `docs/superpowers/specs/2026-09-06-live-world-view-v2-design.md` ("`ggo_emu_panel`: `ViewerRun`").

## Global Constraints

- Branch `live-world-view-v2` in `/home/clay/projects/zed` (already exists, off `ggo`). Commit per task, no AI trailers.
- Gate before every commit: `./script/clippy -p ggo_emu_panel && cargo test -p ggo_emu_panel --lib`. Task 4 also gates `ggo_world_panel` and `ggo_smoke`.
- `ViewerRun` never reads or writes `EmuPanel` state and never creates an `EmulatorItem`.
- Frames published into an endpoint are retired one publish late with `cx.drop_image(stale, None)` (see `EmuPanel::on_frame` today and the `LinkEndpoint::frame` doc in `ggo_common.rs:592-601`).
- The endpoint's state is set on every path: `Building` at build start, `Running` once the session starts, `Stopped(reason)` on build failure, run end, or stop request. `drive::WORLD_PANEL_STOP` is the reason for a host-requested stop.
- Viewer sessions start with `audio: None`.
- Fork hook rule: the booter runs inside a `Workspace` update; anything that reads a pane goes through `cx.defer_in`. Reading `workspace.project()` for the root is fine inline.
- No `unwrap()` outside tests; comments explain why only.

---

### Task 1: `ViewerRun` entity

**Files:**
- Create: `crates/ggo/emu_panel/src/viewer_run.rs`
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (add `mod viewer_run;` next to the other `mod` lines; move `prepare_viewer_build`'s body into the new file as a free function)
- Modify: `crates/ggo/emu_panel/src/menu.rs` only if `editor_cart_args`, `editor_cart_ggo_path`, `failure_reason` are private (make them `pub(crate)`).

**Interfaces:**
- Consumes: `drive::start(cart_path, cart, None, Some(endpoint)) -> (Session, Receiver<Frame>)`; `drive::Frame { bgra, number, step_ms }`; `drive::WIDTH/HEIGHT`; `Session::wait(self) -> FinishedRun` (its `outcome`/reason text as `EmuPanel::finish_run` reads it); `ggo_common::{LinkEndpoint, ViewerState, ProcRequest, ProcRunner, emerald_project_root, EMERALD_MANIFEST}`; `menu::{editor_cart_args, editor_cart_ggo_path, failure_reason}`; `watch_triggers(rel, change, viewer = true)` and `WATCH_DEBOUNCE` from `ggo_emu_panel.rs` (make them `pub(crate)`).
- Produces:

```rust
pub(crate) fn viewer_build_request(
    project_root: &Path,
    world_rel: &str,
) -> Result<(ggo_common::ProcRequest, PathBuf), String>;

pub struct ViewerRun { .. }
impl ViewerRun {
    pub fn new(
        world_rel: String,
        project_root: PathBuf,
        proc_runner: ggo_common::ProcRunner,
        endpoint: Arc<ggo_common::LinkEndpoint>,
        project: Option<Entity<project::Project>>,
        cx: &mut Context<Self>,
    ) -> Self;              // starts the first build immediately
    pub fn endpoint(&self) -> &Arc<ggo_common::LinkEndpoint>;
    pub fn is_stopped(&self, ) -> bool;   // endpoint state is Stopped
    pub(crate) fn rebuild(&mut self, cx: &mut Context<Self>);
}
```

- [ ] **Step 1: Write the failing tests**

At the bottom of `viewer_run.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Mutex;

    /// A fake `emd` whose trailer names `dir/demo-editor.ggo`; `with_cart`
    /// writes the green-screen cart there so frames actually flow.
    fn fake_emd(dir: &Path, with_cart: bool) -> (ggo_common::ProcRunner, Arc<Mutex<Vec<ggo_common::ProcRequest>>>) {
        let ggo = dir.join("demo-editor.ggo");
        if with_cart {
            std::fs::write(&ggo, crate::drive::fixture::green_screen_cart()).unwrap();
        }
        let calls: Arc<Mutex<Vec<ggo_common::ProcRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let runner: ggo_common::ProcRunner = Arc::new(move |request| {
            recorded.lock().unwrap().push(request);
            ggo_common::ProcCapture {
                ok: true,
                lines: vec![serde_json::json!({ "ggo": ggo }).to_string()],
            }
        });
        (runner, calls)
    }

    fn project_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets/worlds")).unwrap();
        std::fs::write(dir.path().join("emerald.toml"), "[project]\n").unwrap();
        std::fs::write(dir.path().join("assets/worlds/main.toml"), "").unwrap();
        dir
    }

    #[gpui::test]
    async fn a_run_builds_boots_publishes_frames_and_stops_on_request(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        let (runner, calls) = fake_emd(dir.path(), true);
        let endpoint = ggo_common::LinkEndpoint::new();
        let run = cx.new(|cx| {
            ViewerRun::new(
                "assets/worlds/main.toml".into(),
                dir.path().to_path_buf(),
                runner,
                endpoint.clone(),
                None,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap()[0].args, ["editor-cart", "--ggo", "--json"]);
        assert_eq!(endpoint.state(), ggo_common::ViewerState::Running);

        // Frames arrive on their own clock: wait for the first one.
        let ticks = endpoint.ticks();
        for _ in 0..600 {
            if endpoint.frame_number().is_some() {
                break;
            }
            cx.background_executor.timer(std::time::Duration::from_millis(5)).await;
            let _ = ticks.try_recv();
        }
        assert!(endpoint.frame_number().is_some(), "the run publishes frames");

        endpoint.request_stop();
        for _ in 0..600 {
            if matches!(endpoint.state(), ggo_common::ViewerState::Stopped(_)) {
                break;
            }
            cx.background_executor.timer(std::time::Duration::from_millis(5)).await;
            cx.run_until_parked();
        }
        assert_eq!(
            endpoint.state(),
            ggo_common::ViewerState::Stopped(crate::drive::WORLD_PANEL_STOP.to_string())
        );
        run.read_with(cx, |run, _| assert!(run.is_stopped()));
    }

    #[gpui::test]
    async fn a_failed_build_stops_the_endpoint_with_the_reason(cx: &mut TestAppContext) {
        let dir = project_dir();
        let runner: ggo_common::ProcRunner = Arc::new(|_| ggo_common::ProcCapture {
            ok: false,
            lines: vec!["error: could not compile demo_editor".to_string()],
        });
        let endpoint = ggo_common::LinkEndpoint::new();
        let _run = cx.new(|cx| {
            ViewerRun::new(
                "assets/worlds/main.toml".into(),
                dir.path().to_path_buf(),
                runner,
                endpoint.clone(),
                None,
                cx,
            )
        });
        cx.run_until_parked();
        match endpoint.state() {
            ggo_common::ViewerState::Stopped(reason) => {
                assert!(reason.contains("build failed"), "{reason}")
            }
            other => panic!("expected Stopped, got {other:?}"),
        }
    }

    #[gpui::test]
    async fn a_world_outside_an_emerald_project_is_refused(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("worlds")).unwrap();
        std::fs::write(dir.path().join("worlds/x.toml"), "").unwrap();
        let (runner, calls) = fake_emd(dir.path(), false);
        let endpoint = ggo_common::LinkEndpoint::new();
        let _run = cx.new(|cx| {
            ViewerRun::new("worlds/x.toml".into(), dir.path().to_path_buf(), runner, endpoint.clone(), None, cx)
        });
        cx.run_until_parked();
        assert!(calls.lock().unwrap().is_empty(), "nothing is built");
        assert!(matches!(endpoint.state(), ggo_common::ViewerState::Stopped(reason) if reason.contains(ggo_common::EMERALD_MANIFEST)));
    }

    #[gpui::test]
    async fn two_runs_coexist_with_independent_endpoints(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        let (runner, _) = fake_emd(dir.path(), true);
        let a = ggo_common::LinkEndpoint::new();
        let b = ggo_common::LinkEndpoint::new();
        let run_a = cx.new(|cx| ViewerRun::new("assets/worlds/main.toml".into(), dir.path().to_path_buf(), runner.clone(), a.clone(), None, cx));
        let run_b = cx.new(|cx| ViewerRun::new("assets/worlds/main.toml".into(), dir.path().to_path_buf(), runner, b.clone(), None, cx));
        cx.run_until_parked();
        assert_eq!(a.state(), ggo_common::ViewerState::Running);
        assert_eq!(b.state(), ggo_common::ViewerState::Running);
        drop(run_a);
        cx.run_until_parked();
        assert!(matches!(a.state(), ggo_common::ViewerState::Stopped(_)), "dropping a run stops it");
        assert_eq!(b.state(), ggo_common::ViewerState::Running, "the other run is untouched");
        drop(run_b);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ggo_emu_panel --lib viewer_run`
Expected: compile error, `viewer_run` module missing.

- [ ] **Step 3: Implement `viewer_run.rs`**

```rust
//! A headless emulator run for one world view's viewer cart. It builds
//! `emd editor-cart --ggo`, boots the `.ggo` on its own emulator thread,
//! and publishes every presented frame into the `LinkEndpoint` the world
//! view polls. It is not the emulator pane: it opens no tab, shows no
//! status, and any number of them can exist at once (one per world tab).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Context, Entity, RenderImage, Subscription, Task};
use ggo_common::{LinkEndpoint, ProcRequest, ProcRunner, ViewerState};

use crate::drive::{self, Frame, Session};
use crate::menu;

/// Assemble the `emd editor-cart --ggo` invocation for the emerald project
/// holding `world_rel`, or the reason there isn't one. Shared with nothing
/// else now that the pane no longer boots viewers, but kept free-standing
/// so it can be tested without a run.
pub(crate) fn viewer_build_request(
    project_root: &Path,
    world_rel: &str,
) -> Result<(ProcRequest, PathBuf), String> {
    let project_dir = ggo_common::emerald_project_root(&project_root.join(world_rel))
        .ok_or_else(|| {
            format!(
                "no {} above {world_rel} — emd needs an emerald project",
                ggo_common::EMERALD_MANIFEST
            )
        })?;
    Ok((
        ProcRequest::emd(&project_dir, menu::editor_cart_args()),
        project_root.to_path_buf(),
    ))
}

pub struct ViewerRun {
    world_rel: String,
    project_root: PathBuf,
    proc_runner: ProcRunner,
    endpoint: Arc<LinkEndpoint>,
    session: Option<Session>,
    build_generation: u64,
    _build_task: Option<Task<()>>,
    _pump_task: Option<Task<()>>,
    /// The frame published one publish ago -- retired one late, like the
    /// pane's own double buffer, because the world view's most recently
    /// submitted scene still draws it.
    previously_published: Option<Arc<RenderImage>>,
    _watch: Option<Subscription>,
    _watch_debounce: Option<Task<()>>,
}

impl ViewerRun {
    pub fn new(
        world_rel: String,
        project_root: PathBuf,
        proc_runner: ProcRunner,
        endpoint: Arc<LinkEndpoint>,
        project: Option<Entity<project::Project>>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|this, cx| {
            // No one is left to poll the endpoint, so say so and let the
            // session's `Drop` stop the thread.
            this.stop_with(drive::WORLD_PANEL_STOP.to_string(), cx);
        })
        .detach();
        let _watch = project.map(|project| cx.subscribe(&project, Self::on_project_event));
        let mut this = Self {
            world_rel,
            project_root,
            proc_runner,
            endpoint,
            session: None,
            build_generation: 0,
            _build_task: None,
            _pump_task: None,
            previously_published: None,
            _watch,
            _watch_debounce: None,
        };
        this.rebuild(cx);
        this
    }

    pub fn endpoint(&self) -> &Arc<LinkEndpoint> {
        &self.endpoint
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.endpoint.state(), ViewerState::Stopped(_))
    }

    /// Build the viewer cart and boot it, replacing any run in flight.
    pub(crate) fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.drop_session(cx);
        self.endpoint.set_state(ViewerState::Building);
        self.build_generation += 1;
        let generation = self.build_generation;
        let request = viewer_build_request(&self.project_root, &self.world_rel);
        let runner = self.proc_runner.clone();
        self._build_task = Some(cx.spawn(async move |this, cx| {
            let (request, root) = match request {
                Ok(prepared) => prepared,
                Err(reason) => {
                    this.update(cx, |this, cx| this.stop_with(reason, cx)).ok();
                    return;
                }
            };
            let capture = cx.background_spawn(async move { runner(request) }).await;
            this.update(cx, |this, cx| {
                if this.build_generation != generation {
                    return;
                }
                if !capture.ok {
                    let reason = format!("build failed: {}", menu::failure_reason(&capture));
                    this.stop_with(reason, cx);
                    return;
                }
                let Some(ggo) = menu::editor_cart_ggo_path(&capture.lines) else {
                    this.stop_with("emd editor-cart --ggo printed no .ggo path".to_string(), cx);
                    return;
                };
                this.boot(root, ggo, cx);
            })
            .ok();
        }));
    }

    fn boot(&mut self, root: PathBuf, ggo: PathBuf, cx: &mut Context<Self>) {
        let cart = menu::cart_selection(&root, &ggo);
        let (session, frames) =
            drive::start(root.join(&cart), cart, None, Some(self.endpoint.clone()));
        self.session = Some(session);
        self.endpoint.set_state(ViewerState::Running);
        self._pump_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(frame) = frames.recv().await {
                let keep_going = this
                    .update(cx, |this, cx| this.on_frame(frame, cx))
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
                // See `EmuPanel::run`'s pump: without a yield a slow frame
                // starves every other foreground task.
                smol::future::yield_now().await;
            }
            this.update(cx, |this, cx| {
                let reason = this.take_run_reason();
                this.stop_with(reason, cx);
            })
            .ok();
        }));
    }

    /// Publish one frame. `false` ends the pump: the host asked to stop.
    fn on_frame(&mut self, frame: Frame, cx: &mut Context<Self>) -> bool {
        if self.endpoint.stop_requested() {
            self.stop_with(drive::WORLD_PANEL_STOP.to_string(), cx);
            return false;
        }
        let Some(buffer) = image::ImageBuffer::from_raw(drive::WIDTH, drive::HEIGHT, frame.bgra)
        else {
            return true;
        };
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        let replaced = self
            .endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace((frame.number, image))
            .map(|(_number, image)| image);
        self.endpoint.tick();
        if let Some(stale) = self.previously_published.take() {
            cx.drop_image(stale, None);
        }
        self.previously_published = replaced;
        true
    }

    /// The reason the emulator thread ended on its own, in the words
    /// `EmuPanel::finish_run` uses for the same outcome.
    fn take_run_reason(&mut self) -> String {
        // `Session::wait` joins the thread; it has already exited here
        // (the frame sender is dropped), so the join is immediate.
        match self.session.take() {
            Some(session) => session.wait().reason_text(),
            None => "run ended".to_string(),
        }
    }

    fn stop_with(&mut self, reason: String, cx: &mut Context<Self>) {
        if !self.is_stopped() {
            self.endpoint.set_state(ViewerState::Stopped(reason));
        }
        self.drop_session(cx);
    }

    fn drop_session(&mut self, cx: &mut Context<Self>) {
        self._pump_task = None;
        self.session = None; // `Session::drop` sets the stop flag
        let published = self
            .endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .map(|(_number, image)| image);
        for stale in [self.previously_published.take(), published].into_iter().flatten() {
            cx.drop_image(stale, None);
        }
    }

    fn on_project_event(
        &mut self,
        _project: Entity<project::Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        let project::Event::WorktreeUpdatedEntries(_, changes) = event else {
            return;
        };
        if self.is_stopped() {
            return;
        }
        let relevant = changes
            .iter()
            .any(|(path, _, change)| crate::watch_triggers(path.as_unix_str(), change, true));
        if !relevant {
            return;
        }
        self._watch_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(crate::WATCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.rebuild(cx)).ok();
        }));
    }
}
```

`FinishedRun::reason_text()` does not exist yet: look at how `EmuPanel::finish_run` (~line 2026) turns a `FinishedRun` into its status string and lift that into a `pub(crate) fn reason_text(&self) -> String` on `FinishedRun` in `drive.rs`, then call it from both places. If `menu::cart_selection` needs `&PathBuf`, pass what it takes.

Watch out for the `cx.on_release` closure: `stop_with` takes `&mut Context<Self>` but `on_release` hands `&mut App`. Split it: make `stop_with` take `&mut gpui::App` (both `Context<Self>` and `App` deref; `cx.drop_image(.., None)` is an `App` method), so `on_release` can call it directly. Adjust the signatures above accordingly (`drop_session(&mut self, cx: &mut App)`).

- [ ] **Step 4: Run tests**

Run: `./script/clippy -p ggo_emu_panel && cargo test -p ggo_emu_panel --lib viewer_run`
Expected: PASS. If `drive::start` needs the cart file to exist and the two-run test races on the same `.ggo`, that is fine: both read it.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/emu_panel/src/viewer_run.rs crates/ggo/emu_panel/src/ggo_emu_panel.rs crates/ggo/emu_panel/src/drive.rs crates/ggo/emu_panel/src/menu.rs
git commit -m "ggo_emu_panel: headless ViewerRun for live world views"
```

---

### Task 2: The booter creates a `ViewerRun`; a registry keeps them alive

**Files:**
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (`register_viewer_booter` closure ~line 232-247; delete `open_emu_item_quietly` ~line 294-330)
- Modify: `crates/ggo/emu_panel/src/viewer_run.rs` (registry)

**Interfaces:**
- Produces in `viewer_run.rs`:

```rust
#[derive(Default)]
pub(crate) struct ViewerRuns(pub(crate) Vec<Entity<ViewerRun>>);
impl gpui::Global for ViewerRuns {}

/// Boot a viewer for `world_rel` under the workspace's project root,
/// driving `endpoint`. `false` when the workspace has no local root.
pub(crate) fn boot(
    workspace: &mut Workspace,
    world_rel: &str,
    endpoint: Arc<LinkEndpoint>,
    proc_runner: ProcRunner,
    cx: &mut Context<Workspace>,
) -> bool;
```

- [ ] **Step 1: Write the failing test**

In the `tests` module of `viewer_run.rs`, add (it needs `run_menu_workspace` from the crate's tests; make that helper `pub(crate)` if it is not, it already is):

```rust
    #[gpui::test]
    async fn the_booter_starts_a_run_and_opens_no_emulator_tab(cx: &mut TestAppContext) {
        let dir = project_dir();
        let (_db, workspace, panel, _worktree, cx) = crate::tests::run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_emd(dir.path(), false);
        cx.update(|_, cx| cx.set_global(TestViewerRunner(runner)));
        let endpoint = workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::boot_viewer(workspace, "assets/worlds/main.toml", window, cx)
        });
        cx.run_until_parked();
        let endpoint = endpoint.expect("the booter claimed the boot");
        assert_eq!(calls.lock().unwrap().len(), 1, "one editor-cart build");
        assert_ne!(endpoint.state(), ggo_common::ViewerState::Building);
        cx.update(|_, cx| {
            assert_eq!(cx.global::<ViewerRuns>().0.len(), 1, "one run registered");
        });
        panel.read_with(cx, |panel, _| {
            assert!(panel.session.is_none(), "the pane runs nothing");
            assert!(panel.status.is_none(), "the pane's status row is untouched");
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<crate::EmulatorItem>(cx).count(),
                1,
                "only the tab run_menu_workspace itself opened"
            );
        });
    }
```

`TestViewerRunner` is a `#[cfg(test)]` global (`struct TestViewerRunner(ProcRunner); impl Global`) that `boot` consults before falling back to `ggo_common::system_proc_runner()` — the only way a test can keep `emd` out of the booter path. `run_menu_workspace` opens one `EmulatorItem` via `emu_panel_via_item`; that is the `1` above. If it opens none, assert `0`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ggo_emu_panel --lib the_booter_starts_a_run`
Expected: FAIL (`ViewerRuns` missing / the old booter opens the tab and sets `panel.session`).

- [ ] **Step 3: Implement**

In `viewer_run.rs`:

```rust
#[derive(Default)]
pub(crate) struct ViewerRuns(pub(crate) Vec<Entity<ViewerRun>>);
impl gpui::Global for ViewerRuns {}

#[cfg(test)]
pub(crate) struct TestViewerRunner(pub(crate) ProcRunner);
#[cfg(test)]
impl gpui::Global for TestViewerRunner {}

fn proc_runner(cx: &gpui::App) -> ProcRunner {
    #[cfg(test)]
    if let Some(runner) = cx.try_global::<TestViewerRunner>() {
        return runner.0.clone();
    }
    ggo_common::system_proc_runner()
}

pub(crate) fn boot(
    workspace: &mut Workspace,
    world_rel: &str,
    endpoint: Arc<LinkEndpoint>,
    cx: &mut Context<Workspace>,
) -> bool {
    let project = workspace.project().clone();
    let Some(root) = project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    else {
        endpoint.set_state(ViewerState::Stopped("no project folder is open".to_string()));
        return false;
    };
    let runner = proc_runner(cx);
    let world_rel = world_rel.to_string();
    let run = cx.new(|cx| ViewerRun::new(world_rel, root, runner, endpoint, Some(project), cx));
    let runs = cx.default_global::<ViewerRuns>();
    runs.0.retain(|run| !run.read(cx).is_stopped());
    runs.0.push(run);
    true
}
```

(`runs.0.retain(.. run.read(cx) ..)` borrows `cx` while `runs` is borrowed mutably from it: collect the stopped-ness first — `let live: Vec<bool> = cx.default_global::<ViewerRuns>().0.iter().map(|r| !r.read(cx).is_stopped()).collect();` then retain by index — or use `cx.update_global::<ViewerRuns, _>(|runs, cx| ..)`. Pick whichever compiles cleanly.)

Replace the booter registration in `init`:

```rust
    ggo_common::register_viewer_booter(cx, |workspace, world_rel, endpoint, _window, cx| {
        viewer_run::boot(workspace, world_rel, endpoint, cx)
    });
```

Delete `open_emu_item_quietly` and its doc comment. `boot` needs `ProcRunner` only from `cx`, so drop the `proc_runner` parameter from the interface above if you kept it.

- [ ] **Step 4: Run tests**

Run: `./script/clippy -p ggo_emu_panel && cargo test -p ggo_emu_panel --lib viewer_run`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/emu_panel/src
git commit -m "ggo_emu_panel: viewer booter spawns headless runs, no tab"
```

---

### Task 3: Remove the viewer paths from `EmuPanel`

**Files:**
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs`
- Modify: `crates/ggo/emu_panel/src/drive.rs` only if something becomes dead (`Session::release_link`, `link_owned` stay: the thread's stop check still uses them).

**Interfaces:**
- Consumes: nothing new.
- Produces: `EmuPanel` has no `viewer_link`, `previously_published`, `RunKind::Viewer`, `boot_viewer`, `fail_viewer`, `prepare_viewer_build`, `release_link_of_current_run`, `stop_for_world_panel`, `take_link_frame`, `FramePumped`. `drive::start` is called with `None` for the link from the pane.

- [ ] **Step 1: Delete, guided by the compiler**

Remove, in this order, re-running `cargo check -p ggo_emu_panel` after each:

1. `RunKind::Viewer(String)` and every `matches!(.., RunKind::Viewer(_))` (they become `false`; simplify the surrounding `if`/`match`). `RunKind::world()` keeps `World(world) => Some(world)`.
2. Fields `viewer_link`, `previously_published`; their initialisers in `new`.
3. `boot_viewer`, `fail_viewer`, `prepare_viewer_build`, `release_link_of_current_run`, `stop_for_world_panel`, `take_link_frame`, `FramePumped`.
4. In `run`: the `let link = match self.run_kind {..}` block becomes nothing; `drive::start(root.join(&cart), cart, Some(self.audio.clone()), None)`. The `_pump_task` loop body becomes `if this.update(cx, |this, cx| this.on_frame(frame, cx)).is_err() { return; }` and the tail is just `this.update(cx, |this, cx| this.finish_run(cx)).ok();`. `on_frame` returns `()` and only keeps the stats + `latest_frame` + `cx.notify()` part.
5. `run_selected_cart` becomes a plain `self.run(window, cx)` (delete the wrapper if nothing else distinguishes it).
6. `schedule_watch_rebuild`: delete the `viewer_link` branch; `on_project_event`: `watch_triggers(.., false)`.
7. `auto_pause`'s viewer exemption (grep `viewer` in its body) goes.
8. `release_atlas_all`: drop the endpoint-frame part.
9. Tests: delete every test that calls `panel.boot_viewer` (the block ~lines 6566-7470: `boot_viewer_*`, `a_failed_editor_cart_build_*`, the `viewer_fixture*` helpers if nothing else uses them, `VIEWER_GGO`). Keep `boot_viewer_returns_none_without_a_registered_booter` (it exercises `ggo_common`) — but note `run_menu_workspace` calls `init`, which now registers a booter; if that test relied on no booter, construct it without `init` (use the pattern `test_toggle_focus_opens_panel` in the world panel uses, minus `init`).

Grep until empty: `grep -n "viewer_link\|RunKind::Viewer\|previously_published\|FramePumped\|boot_viewer\|stop_for_world_panel\|open_emu_item_quietly" crates/ggo/emu_panel/src/ggo_emu_panel.rs`.

- [ ] **Step 2: Run the crate gate**

Run: `./script/clippy -p ggo_emu_panel && cargo test -p ggo_emu_panel --lib`
Expected: PASS. Two pre-existing failures may come from foreign uncommitted `ggo-hal` edits in `~/projects/ggo` (see memory); confirm they fail on `ggo` too before ignoring them, and name them in the commit message if so.

- [ ] **Step 3: Commit**

```bash
git add crates/ggo/emu_panel/src
git commit -m "ggo_emu_panel: drop the pane's viewer run paths"
```

---

### Task 4: Downstream crates still build; review; merge

**Files:** none new.

- [ ] **Step 1: Cross-crate gates**

Run:

```bash
./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib && ./script/clippy -p ggo_smoke && cargo test -p ggo_smoke --lib
```

Expected: PASS. The world panel's Live tests that boot through `ggo_common::boot_viewer` with a fake booter registered in-test are unaffected; a world-panel test that asserted "an `EmulatorItem` appears" for Live (grep `EmulatorItem` in `ggo_world_panel.rs` and `ggo_smoke.rs`) flips to asserting none appears.

- [ ] **Step 2: Review**

Dispatch a fresh opus reviewer over `git diff ggo...live-world-view-v2 -- crates/ggo/emu_panel crates/ggo/common` for practices and for the spec's `ViewerRun` section: headless, N runs, endpoint states on every path, frames retired one late, no `EmuPanel` state touched. Fix findings, re-run the gates, commit.

- [ ] **Step 3: Merge**

Per `feedback-branch-per-feature-review-then-merge`: this phase merges together with Phases 3 and 4 as one feature branch. Leave it on `live-world-view-v2`; the merge step is at the end of the Phase 4 plan.
