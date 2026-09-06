//! A headless emulator run for one world view's viewer cart. It builds
//! `emd editor-cart --ggo`, boots the `.ggo` on its own emulator thread,
//! and publishes every presented frame into the `LinkEndpoint` the world
//! view polls. It is not the emulator pane: it opens no tab, shows no
//! status, and any number of them can exist at once (one per world tab).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ggo_common::{LinkEndpoint, ProcCapture, ProcRequest, ProcRunner, ViewerState};
use gpui::{App, AppContext as _, Context, Entity, RenderImage, Subscription, Task};
use workspace::Workspace;

use crate::drive::{self, Frame, Session};
use crate::menu;

/// Assemble the `emd editor-cart --ggo` invocation for the emerald project
/// holding `world_rel`, or the reason there isn't one. Free-standing so a
/// refusal can be decided -- and tested -- without starting a run.
pub(crate) fn viewer_build_request(
    project_root: &Path,
    world_rel: &str,
) -> Result<(ProcRequest, PathBuf), String> {
    let project_dir =
        ggo_common::emerald_project_root(&project_root.join(world_rel)).ok_or_else(|| {
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

/// Every viewer run this app has started. A `ViewerRun` stops itself
/// when it is dropped and the world view driving it holds only the
/// endpoint, so something has to keep the run alive for as long as that
/// view wants frames; this is that something.
#[derive(Default)]
pub(crate) struct ViewerRuns(pub(crate) Vec<Entity<ViewerRun>>);

impl gpui::Global for ViewerRuns {}

/// Test hook: the `emd` runner [`boot`] hands the run it creates, in
/// place of spawning the real binary.
#[cfg(test)]
pub(crate) struct TestViewerRunner(pub(crate) ProcRunner);

#[cfg(test)]
impl gpui::Global for TestViewerRunner {}

/// Test hook: the project root [`boot`] builds under. A test workspace's
/// worktree lives in a `FakeFs`, which neither `emd` nor
/// [`ggo_common::emerald_project_root`] (it stats the real disk) can see,
/// so a test that boots names a real directory here. The emulator pane's
/// `root_override` is the same hook for the same reason.
#[cfg(test)]
pub(crate) struct TestViewerRoot(pub(crate) PathBuf);

#[cfg(test)]
impl gpui::Global for TestViewerRoot {}

/// Boot a viewer for `world_rel` under the workspace's project root,
/// driving `endpoint`.
///
/// ALWAYS claims the boot (`true`), refusals included: a `false` sends
/// the world view down `ggo_common::boot_viewer`'s no-booter path, where
/// it has no endpoint to read and so can only say the emulator pane is
/// missing. Claiming and publishing `Stopped(reason)` is what puts the
/// real reason -- "no project folder is open" -- in front of the user.
/// Nothing here can decline on grounds another booter could satisfy.
///
/// Runs while the `Workspace` is leased: it reads the project and creates
/// an entity, and touches no pane. Opening the emulator tab from here --
/// what this replaced -- is the double-lease panic.
pub(crate) fn boot(
    workspace: &mut Workspace,
    world_rel: &str,
    endpoint: Arc<LinkEndpoint>,
    cx: &mut Context<Workspace>,
) -> bool {
    let project = workspace.project().clone();
    let Some(root) = project_root(&project, cx) else {
        endpoint.set_state(ViewerState::Stopped("no project folder is open".to_string()));
        // No run to register: there is nothing for one to build.
        return true;
    };
    let runner = proc_runner(cx);
    let world_rel = world_rel.to_string();
    let run = cx.new(|cx| ViewerRun::new(world_rel, root, runner, endpoint, Some(project), cx));
    // The runs of world views that have since gone away: their emulator
    // threads have stopped and nothing polls their endpoints any more, so
    // let go of them rather than grow this list for the life of the app.
    // Taken out of the global first because deciding which to keep reads
    // the entities, and so borrows the `App` the global lives in.
    let existing = std::mem::take(&mut cx.default_global::<ViewerRuns>().0);
    let mut live: Vec<Entity<ViewerRun>> = existing
        .into_iter()
        .filter(|run| !run.read(cx).is_stopped())
        .collect();
    live.push(run);
    cx.default_global::<ViewerRuns>().0 = live;
    true
}

#[cfg_attr(not(test), allow(unused_variables))]
fn proc_runner(cx: &App) -> ProcRunner {
    #[cfg(test)]
    if let Some(runner) = cx.try_global::<TestViewerRunner>() {
        return runner.0.clone();
    }
    ggo_common::system_proc_runner()
}

fn project_root(project: &Entity<project::Project>, cx: &App) -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(root) = cx.try_global::<TestViewerRoot>() {
        return Some(root.0.clone());
    }
    project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

pub struct ViewerRun {
    world_rel: String,
    project_root: PathBuf,
    proc_runner: ProcRunner,
    endpoint: Arc<LinkEndpoint>,
    session: Option<Session>,
    /// A build is in flight. The `emd` spawn it is waiting on BLOCKS a
    /// background thread and cannot be cancelled, so a save landing
    /// mid-build queues rather than starting a second `emd` over the
    /// first.
    building: bool,
    pending_rebuild: bool,
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
            building: false,
            pending_rebuild: false,
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
        if self.endpoint.stop_requested() {
            // The world view has left live mode. Nothing -- not a save
            // that queued behind the last build, not a caller that has
            // not noticed yet -- may start another run for it.
            return;
        }
        if self.building {
            // Every further save joins this one queued rebuild: an `emd`
            // build of the editor cart takes tens of seconds, and the
            // blocking spawn already in flight cannot be cancelled, so
            // starting another over it would just leave two `emd`
            // processes writing the same target directory.
            self.pending_rebuild = true;
            return;
        }
        // The pump too, not just the session: a pump left running would
        // keep publishing the outgoing run's last frames into the
        // endpoint mid-build, and would report that run's ending over the
        // `Building` this is about to write. Safe here and not in
        // `end_run`, because a rebuild is never asked for from inside the
        // pump task itself.
        self._pump_task = None;
        self.drop_session(cx);
        self.endpoint.set_state(ViewerState::Building);
        self.building = true;
        let request = viewer_build_request(&self.project_root, &self.world_rel);
        let runner = self.proc_runner.clone();
        self._build_task = Some(cx.spawn(async move |this, cx| {
            let (request, root) = match request {
                Ok(prepared) => prepared,
                Err(reason) => {
                    this.update(cx, |this, cx| {
                        this.stop_with(reason, cx);
                        this.build_done(cx);
                    })
                    .ok();
                    return;
                }
            };
            let capture = cx.background_spawn(async move { runner(request) }).await;
            this.update(cx, |this, cx| {
                this.build_finished(capture, root, cx);
                this.build_done(cx);
            })
            .ok();
        }));
    }

    /// What `emd editor-cart --ggo` had to say, on the UI thread.
    fn build_finished(&mut self, capture: ProcCapture, root: PathBuf, cx: &mut Context<Self>) {
        if self.endpoint.stop_requested() {
            // Asked to stop while the build ran. Booting anyway would
            // show the world view a `Running` it has already said it does
            // not want, and leave a cart running for nobody until its
            // first frame reached the pump.
            self.stop_with(drive::WORLD_PANEL_STOP.to_string(), cx);
            return;
        }
        if !capture.ok {
            let reason = format!("build failed: {}", menu::failure_reason(&capture));
            self.stop_with(reason, cx);
            return;
        }
        let Some(ggo) = menu::editor_cart_ggo_path(&capture.lines) else {
            self.stop_with("emd editor-cart --ggo printed no .ggo path".to_string(), cx);
            return;
        };
        self.boot(root, ggo, cx);
    }

    /// The build ended, whatever its outcome: clear the in-flight flag
    /// and run whatever queued behind it.
    fn build_done(&mut self, cx: &mut Context<Self>) {
        self.building = false;
        if std::mem::take(&mut self.pending_rebuild) {
            // Deferred, not called inline: this runs INSIDE the build
            // task, and `rebuild` assigns `_build_task` -- the handle of
            // the very task that is running.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.rebuild(cx)).ok();
            });
        }
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
                // See `EmuPanel::run`'s pump: without a yield a frame
                // whose update outruns the emulator's 16ms refill starves
                // every other foreground task.
                smol::future::yield_now().await;
            }
            this.update(cx, |this, cx| {
                // `end_run`, not `stop_with`: this IS the pump task.
                let reason = this.take_run_reason();
                this.end_run(reason, cx);
            })
            .ok();
        }));
    }

    /// Publish one frame. `false` ends the pump: the host asked to stop.
    fn on_frame(&mut self, frame: Frame, cx: &mut Context<Self>) -> bool {
        if self.endpoint.stop_requested() {
            self.end_run(drive::WORLD_PANEL_STOP.to_string(), cx);
            return false;
        }
        let Some(buffer) = image::ImageBuffer::from_raw(drive::WIDTH, drive::HEIGHT, frame.bgra)
        else {
            return true;
        };
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        // The publisher owns retiring what it replaces (see
        // `LinkEndpoint::frame`): the world view only ever clones the
        // `Arc`, so without this every published frame leaks its atlas
        // tile.
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
    /// `EmuPanel::finish_run` puts on its status row for the same outcome.
    fn take_run_reason(&mut self) -> String {
        // `Session::wait` joins the thread; it has already exited here
        // (the frame sender is dropped), so the join is immediate.
        match self.session.take() {
            Some(session) => session.wait().reason,
            None => "run ended".to_string(),
        }
    }

    /// Stop from OUTSIDE the pump task: publishes the reason and drops
    /// the run, pump included.
    fn stop_with(&mut self, reason: String, cx: &mut App) {
        self.end_run(reason, cx);
        // Not in `end_run`: that one is also called from inside the pump
        // task, and a task that drops its own handle would be dropping
        // the future it is running in.
        self._pump_task = None;
    }

    /// Report `reason` and let go of the emulator thread and its frames.
    ///
    /// The session goes FIRST, as it does in `rebuild`: `drop_session`
    /// releases the link before setting the stop flag, so from here on
    /// the outgoing thread cannot stamp its own terminal reason over the
    /// one written below.
    fn end_run(&mut self, reason: String, cx: &mut App) {
        self.drop_session(cx);
        if !self.is_stopped() {
            self.endpoint.set_state(ViewerState::Stopped(reason));
        }
    }

    fn drop_session(&mut self, cx: &mut App) {
        if let Some(session) = self.session.take() {
            // BEFORE the drop, which is what sets the stop flag: the
            // thread writes its own terminal `Stopped` through the link
            // whenever it gets round to noticing that flag -- which is
            // after this endpoint has been told what is really happening.
            // See `drive::Session::release_link`.
            session.release_link();
        }
        let published = self
            .endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .map(|(_number, image)| image);
        // Nothing will publish over them now, so both the frame in the
        // slot and the one the publish lag was keeping alive are retired.
        for stale in [self.previously_published.take(), published]
            .into_iter()
            .flatten()
        {
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
        let relevant = changes
            .iter()
            .any(|(path, _, change)| crate::watch_triggers(path.as_unix_str(), change, true));
        if !relevant {
            return;
        }
        self.on_sources_changed(cx);
    }

    /// Something this run's cart is built from was saved: rebuild once
    /// the saves stop landing.
    ///
    /// A run whose build FAILED rebuilds too -- fixing the error and
    /// saving is the loop the live view exists for, and refusing it would
    /// leave the view stuck on the first error until the user closed and
    /// reopened it. The one final state is a stop the HOST asked for: it
    /// left live mode, so no save may bring its run back.
    pub(crate) fn on_sources_changed(&mut self, cx: &mut Context<Self>) {
        if self.endpoint.stop_requested() {
            return;
        }
        self._watch_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(crate::WATCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.rebuild(cx)).ok();
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake `emd` whose trailer names `dir/demo-editor.ggo`; `with_cart`
    /// writes the green-screen cart there so frames actually flow.
    fn fake_emd(
        dir: &Path,
        with_cart: bool,
    ) -> (
        ggo_common::ProcRunner,
        Arc<Mutex<Vec<ggo_common::ProcRequest>>>,
    ) {
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
        assert_eq!(
            calls.lock().unwrap()[0].args,
            ["editor-cart", "--ggo", "--json"]
        );
        assert_eq!(endpoint.state(), ggo_common::ViewerState::Running);

        // Frames arrive on their own clock: wait for the first one.
        let ticks = endpoint.ticks();
        for _ in 0..600 {
            if endpoint.frame_number().is_some() {
                break;
            }
            cx.background_executor
                .timer(std::time::Duration::from_millis(5))
                .await;
            ticks.try_recv().ok();
        }
        assert!(
            endpoint.frame_number().is_some(),
            "the run publishes frames"
        );

        endpoint.request_stop();
        for _ in 0..600 {
            if matches!(endpoint.state(), ggo_common::ViewerState::Stopped(_)) {
                break;
            }
            cx.background_executor
                .timer(std::time::Duration::from_millis(5))
                .await;
            cx.run_until_parked();
        }
        assert_eq!(
            endpoint.state(),
            ggo_common::ViewerState::Stopped(crate::drive::WORLD_PANEL_STOP.to_string())
        );
        run.read_with(cx, |run, _| assert!(run.is_stopped()));
    }

    /// A rebuild (what a watched save ends up calling) reuses the
    /// endpoint the world view is already holding: `Building` while it
    /// runs, `Running` again once the new run boots -- never the stopped
    /// run's own terminal reason, which its thread writes on its way out.
    #[gpui::test]
    async fn a_rebuild_reboots_into_the_same_endpoint(cx: &mut TestAppContext) {
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
        assert_eq!(calls.lock().unwrap().len(), 1);

        run.update(cx, |run, cx| run.rebuild(cx));
        assert_eq!(
            endpoint.state(),
            ggo_common::ViewerState::Building,
            "the world view is told to wait rather than left on the old run"
        );
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "the viewer cart is built again"
        );
        assert_eq!(endpoint.state(), ggo_common::ViewerState::Running);
    }

    /// A build that FAILED is not the end of the live view: the user
    /// fixes the error, saves, and the run comes back. Refusing to
    /// rebuild a stopped run would strand the view on the first
    /// compile error -- the loop this whole feature is for.
    #[gpui::test]
    async fn a_save_rebuilds_a_run_whose_build_failed(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        let ggo = dir.path().join("demo-editor.ggo");
        std::fs::write(&ggo, crate::drive::fixture::green_screen_cart()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let runner: ggo_common::ProcRunner = Arc::new(move |_request| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                ggo_common::ProcCapture {
                    ok: false,
                    lines: vec!["error: could not compile demo_editor".to_string()],
                }
            } else {
                ggo_common::ProcCapture {
                    ok: true,
                    lines: vec![serde_json::json!({ "ggo": ggo }).to_string()],
                }
            }
        });
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
        assert!(
            matches!(endpoint.state(), ggo_common::ViewerState::Stopped(reason) if reason.contains("build failed")),
            "{:?}",
            endpoint.state()
        );

        run.update(cx, |run, cx| run.on_sources_changed(cx));
        cx.executor().advance_clock(crate::WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            endpoint.state(),
            ggo_common::ViewerState::Running,
            "the fixed source rebuilt and booted"
        );
    }

    /// Saves that land while `emd` is running coalesce into ONE rebuild:
    /// the build is a blocking spawn that cannot be cancelled, so a
    /// rebuild per save would overlap `emd` processes over the same
    /// target directory.
    #[gpui::test]
    async fn saves_during_a_build_coalesce_into_one_rebuild(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        let (runner, calls) = fake_emd(dir.path(), true);
        let endpoint = ggo_common::LinkEndpoint::new();
        // Nothing is pumped between here and the asserts, so the build
        // `new` starts is still in flight for both rebuilds below.
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
        run.update(cx, |run, cx| run.rebuild(cx));
        run.update(cx, |run, cx| run.rebuild(cx));
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "the build in flight, then one rebuild for both saves"
        );
        assert_eq!(endpoint.state(), ggo_common::ViewerState::Running);
    }

    /// A stop asked for while the build runs is honoured when it lands:
    /// no cart is booted for a world view that has already left live
    /// mode, and no save can bring it back.
    #[gpui::test]
    async fn a_stop_requested_during_a_build_is_honoured(cx: &mut TestAppContext) {
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
        endpoint.request_stop();
        cx.run_until_parked();
        assert_eq!(
            endpoint.state(),
            ggo_common::ViewerState::Stopped(crate::drive::WORLD_PANEL_STOP.to_string()),
            "the build's `.ggo` is never booted"
        );
        assert!(
            endpoint.frame_number().is_none(),
            "and no frame was ever published"
        );
        run.read_with(cx, |run, _| assert!(run.is_stopped()));

        let built = calls.lock().unwrap().len();
        run.update(cx, |run, cx| run.on_sources_changed(cx));
        cx.executor().advance_clock(crate::WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            built,
            "a save does not resurrect a run the host asked to stop"
        );
        assert!(matches!(
            endpoint.state(),
            ggo_common::ViewerState::Stopped(_)
        ));
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
            ViewerRun::new(
                "worlds/x.toml".into(),
                dir.path().to_path_buf(),
                runner,
                endpoint.clone(),
                None,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(calls.lock().unwrap().is_empty(), "nothing is built");
        assert!(
            matches!(endpoint.state(), ggo_common::ViewerState::Stopped(reason) if reason.contains(ggo_common::EMERALD_MANIFEST))
        );
    }

    #[gpui::test]
    async fn two_runs_coexist_with_independent_endpoints(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        let (runner, _) = fake_emd(dir.path(), true);
        let a = ggo_common::LinkEndpoint::new();
        let b = ggo_common::LinkEndpoint::new();
        let run_a = cx.new(|cx| {
            ViewerRun::new(
                "assets/worlds/main.toml".into(),
                dir.path().to_path_buf(),
                runner.clone(),
                a.clone(),
                None,
                cx,
            )
        });
        let run_b = cx.new(|cx| {
            ViewerRun::new(
                "assets/worlds/main.toml".into(),
                dir.path().to_path_buf(),
                runner,
                b.clone(),
                None,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(a.state(), ggo_common::ViewerState::Running);
        assert_eq!(b.state(), ggo_common::ViewerState::Running);
        drop(run_a);
        // gpui drops a released entity -- and so runs the release
        // listener that stops the run -- on the app's next turn, not at
        // the `drop` itself.
        cx.update(|_| {});
        assert!(
            matches!(a.state(), ggo_common::ViewerState::Stopped(_)),
            "dropping a run stops it"
        );
        assert_eq!(
            b.state(),
            ggo_common::ViewerState::Running,
            "the other run is untouched"
        );
        drop(run_b);
    }

    /// What the world panel's live view actually gets: the registered
    /// booter starts a headless run and registers it, and the emulator
    /// pane is left completely alone -- no tab of its own, no session, no
    /// status row. The user asked for a view in the world panel.
    #[gpui::test]
    async fn the_booter_starts_a_run_and_opens_no_emulator_tab(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        let (_db, workspace, panel, _worktree, cx) =
            crate::tests::run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_emd(dir.path(), false);
        cx.update(|_, cx| {
            cx.set_global(TestViewerRunner(runner));
            cx.set_global(TestViewerRoot(dir.path().to_path_buf()));
        });
        let front = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item(cx).map(|item| item.item_id())
            })
            .expect("a centre tab is in front");
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
            assert_eq!(
                workspace.active_item(cx).map(|item| item.item_id()),
                Some(front),
                "and the tab the user was on is still in front"
            );
        });
    }

    /// A refusal must reach the world view as a REASON. The booter claims
    /// the boot even when it cannot start one, so `boot_viewer` hands
    /// back an endpoint saying what is wrong -- rather than the `None`
    /// that means "this build has no emulator pane at all", which is the
    /// one thing that is NOT wrong here.
    #[gpui::test]
    async fn a_workspace_with_no_folder_is_refused_with_a_reason(cx: &mut TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            crate::init(cx);
        });
        let project = project::Project::test(project::FakeFs::new(cx.executor()), [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());

        let endpoint = workspace.update_in(cx, |workspace, window, cx| {
            ggo_common::boot_viewer(workspace, "assets/worlds/main.toml", window, cx)
        });
        let endpoint = endpoint.expect("the booter claims a boot it cannot start");
        assert_eq!(
            endpoint.state(),
            ggo_common::ViewerState::Stopped("no project folder is open".to_string()),
            "the world view is told why, not left waiting"
        );
        cx.update(|_, cx| {
            assert!(
                cx.try_global::<ViewerRuns>()
                    .is_none_or(|runs| runs.0.is_empty()),
                "nothing registered: there is no run to keep alive"
            );
        });
    }

    /// The run the booter started watches the project it was booted from:
    /// a save rebuilds it, with no emulator pane in the loop at all.
    #[gpui::test]
    async fn a_booted_run_rebuilds_when_the_project_is_saved(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        let (_db, workspace, _panel, _worktree, cx) =
            crate::tests::run_menu_workspace(cx, dir.path()).await;
        let (runner, calls) = fake_emd(dir.path(), false);
        cx.update(|_, cx| {
            cx.set_global(TestViewerRunner(runner));
            cx.set_global(TestViewerRoot(dir.path().to_path_buf()));
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                ggo_common::boot_viewer(workspace, "assets/worlds/main.toml", window, cx)
            })
            .expect("the booter claimed the boot");
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1);

        let fs = workspace.read_with(cx, |workspace, cx| workspace.project().read(cx).fs().clone());
        fs.as_fake()
            .insert_file("/proj/main.rs", b"fn main() {}".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(crate::WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "the save rebuilt the viewer cart"
        );
    }
}
