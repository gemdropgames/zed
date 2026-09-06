//! A headless emulator run for one world view's viewer cart. It builds
//! `emd editor-cart --ggo`, boots the `.ggo` on its own emulator thread,
//! and publishes every presented frame into the `LinkEndpoint` the world
//! view polls. It is not the emulator pane: it opens no tab, shows no
//! status, and any number of them can exist at once (one per world tab).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::Shared;
use ggo_common::{LinkEndpoint, ProcRequest, ProcRunner, ViewerState};
use gpui::{App, AppContext as _, Context, Entity, RenderImage, Subscription, Task};
use workspace::Workspace;

use crate::drive::{self, Frame, Session};
use crate::menu;

/// How many published frames may be waiting to be retired before the
/// oldest is retired regardless. A consumer that is painting keeps at most
/// the frame it last took, so anything beyond a handful means it has
/// stopped dropping them and the queue would grow with the run.
const RETIRE_QUEUE_CAP: usize = 8;

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

/// One `emd editor-cart --ggo` build: the `.ggo` it produced, or the
/// reason there isn't one. Shared because every viewer of the same
/// emerald project waits on the same build.
type SharedBuild = Shared<Task<Result<PathBuf, String>>>;

/// Every viewer run this app has started, and the builds they share.
///
/// A `ViewerRun` stops itself when it is dropped and the world view
/// driving it holds only the endpoint, so something has to keep the run
/// alive for as long as that view wants frames; this is that something.
#[derive(Default)]
pub(crate) struct ViewerRuns {
    pub(crate) runs: Vec<Entity<ViewerRun>>,
    /// The build in flight for each emerald project directory. N world
    /// tabs of one project are N runs, but they are built from the same
    /// sources into the same `.ggo`, so they get ONE `emd` per save.
    builds: HashMap<PathBuf, SharedBuild>,
}

impl gpui::Global for ViewerRuns {}

/// The build every viewer of `project_dir` is waiting on: the one already
/// in flight, or a new one started here.
///
/// Sharing is not only about the minutes an `emd editor-cart` build takes
/// and the background threads it blocks. `emd` rewrites ONE `.ggo` per
/// project in place, and `drive::start` reads that file: two concurrent
/// builds of the same project would have a viewer boot a half-written
/// cart. One build per project is what makes that read safe.
fn shared_build(
    project_dir: &Path,
    request: ProcRequest,
    runner: ProcRunner,
    cx: &mut App,
) -> SharedBuild {
    if let Some(build) = cx.default_global::<ViewerRuns>().builds.get(project_dir) {
        // Only a build that is really still in flight. `strong_count == 1`
        // means this map holds the last handle to it: every run that was
        // waiting has been dropped (its world tab closed), and joining it
        // would hand a viewer opened now a `.ggo` built before the edits
        // since. `peek` is the same staleness one step later -- a result
        // already in hand belongs to the save that asked for it.
        let in_flight = build.strong_count().is_some_and(|handles| handles > 1)
            && build.peek().is_none();
        if in_flight {
            return build.clone();
        }
    }
    let build = cx
        .background_spawn(async move {
            let capture = runner(request);
            if !capture.ok {
                return Err(format!("build failed: {}", menu::failure_reason(&capture)));
            }
            menu::editor_cart_ggo_path(&capture.lines)
                .ok_or_else(|| "emd editor-cart --ggo printed no .ggo path".to_string())
        })
        .shared();
    cx.default_global::<ViewerRuns>()
        .builds
        .insert(project_dir.to_path_buf(), build.clone());
    build
}

/// Let go of a finished build, so the next save starts a fresh one rather
/// than handing out a stale `.ggo` path forever. Every waiter calls this;
/// the `ptr_eq` is what keeps the second one from evicting the follow-up
/// build a queued save has already registered in the meantime.
fn forget_build(project_dir: &Path, finished: &SharedBuild, cx: &mut App) {
    let builds = &mut cx.default_global::<ViewerRuns>().builds;
    if builds
        .get(project_dir)
        .is_some_and(|current| current.ptr_eq(finished))
    {
        builds.remove(project_dir);
    }
}

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
    // The runs of world views that have since gone away: their host asked
    // them to stop, so nothing polls their endpoints any more and letting
    // go of them is what keeps this list from growing for the life of the
    // app. A merely STOPPED run is not that: `on_sources_changed`
    // deliberately keeps a run whose build failed alive so that fixing the
    // error and saving brings it back, and this list holds the only handle
    // to it -- pruning on `is_stopped` would have the next boot of any
    // other world view drop it, watch subscription and all.
    // Taken out of the global first because deciding which to keep reads
    // the entities, and so borrows the `App` the global lives in.
    let existing = std::mem::take(&mut cx.default_global::<ViewerRuns>().runs);
    let mut live: Vec<Entity<ViewerRun>> = existing
        .into_iter()
        .filter(|run| !run.read(cx).endpoint().stop_requested())
        .collect();
    live.push(run);
    cx.default_global::<ViewerRuns>().runs = live;
    true
}

/// Drop the registered run driving `endpoint`, if one is still registered.
///
/// Deferred by [`ViewerRun::end_run`], which runs INSIDE the entity being
/// dropped here: an entity cannot retain itself out of the global while it
/// is being updated. Keyed on the endpoint rather than on a handle so that
/// nothing here keeps the run alive past the retain.
fn forget_run(endpoint: &Arc<LinkEndpoint>, cx: &mut App) {
    let existing = std::mem::take(&mut cx.default_global::<ViewerRuns>().runs);
    let live: Vec<Entity<ViewerRun>> = existing
        .into_iter()
        .filter(|run| !Arc::ptr_eq(run.read(cx).endpoint(), endpoint))
        .collect();
    cx.default_global::<ViewerRuns>().runs = live;
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
    /// A build is in flight (this run's own view of the project's shared
    /// build). The `emd` spawn it is waiting on BLOCKS a background thread
    /// and cannot be cancelled, so a save landing mid-build queues rather
    /// than starting a second `emd` over the first.
    building: bool,
    pending_rebuild: bool,
    _build_task: Option<Task<()>>,
    _pump_task: Option<Task<()>>,
    /// Frames this run has published and replaced, waiting to be retired.
    /// They cannot be dropped at the publish that replaces them: the world
    /// view is an asynchronous consumer that clones the `Arc` out of the
    /// slot and paints it whenever it next draws, and retiring an image it
    /// still holds makes `paint_image` re-insert the dropped image --
    /// leaking the atlas tile this retirement exists to reclaim. So each
    /// one waits here until this run holds the only reference.
    retiring: Vec<Arc<RenderImage>>,
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
            retiring: Vec::new(),
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
            // blocking spawn already in flight cannot be cancelled. The
            // follow-up rebuilds of every run of this project then join
            // one shared build again, so N world tabs still cost one
            // `emd` per save.
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
        let (request, root) = match viewer_build_request(&self.project_root, &self.world_rel) {
            Ok(prepared) => prepared,
            Err(reason) => {
                self.stop_with(reason, cx);
                return;
            }
        };
        self.building = true;
        // The cwd `ProcRequest::emd` was built with IS the emerald project
        // root (`emd` discovers the project from its cwd), which is what
        // every viewer of this project coordinates on.
        let project_dir = request.cwd.clone();
        // Joined here, on the UI thread, rather than inside the task: two
        // world views booting in the same turn have to see each other's
        // build, and the task bodies do not run until the turn is over.
        let build = shared_build(&project_dir, request, self.proc_runner.clone(), cx);
        self._build_task = Some(cx.spawn(async move |this, cx| {
            let outcome = build.clone().await;
            cx.update(|cx| forget_build(&project_dir, &build, cx));
            this.update(cx, |this, cx| {
                this.build_finished(outcome, root, cx);
                this.build_done(cx);
            })
            .ok();
        }));
    }

    /// What the project's shared `emd editor-cart --ggo` produced, on the
    /// UI thread. Every run waiting on that build is told separately, and
    /// each boots its own session from the one `.ggo`.
    fn build_finished(
        &mut self,
        outcome: Result<PathBuf, String>,
        root: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.endpoint.stop_requested() {
            // Asked to stop while the build ran. Booting anyway would
            // show the world view a `Running` it has already said it does
            // not want, and leave a cart running for nobody until its
            // first frame reached the pump.
            self.stop_with(drive::WORLD_PANEL_STOP.to_string(), cx);
            return;
        }
        match outcome {
            Ok(ggo) => self.boot(root, ggo, cx),
            Err(reason) => self.stop_with(reason, cx),
        }
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
        if let Some(replaced) = replaced {
            self.retiring.push(replaced);
        }
        self.retire_unheld(cx);
        true
    }

    /// Retire every queued frame nobody else is holding.
    ///
    /// `strong_count == 1` means this queue owns the last reference: the
    /// slot has moved on and the world view has finished with it, so the
    /// atlas tile can go. The cap is there because a consumer that stops
    /// dropping its clones (a world view whose window is not drawing)
    /// would otherwise queue a frame per publish forever; past it the
    /// oldest goes anyway, which is what the single-slot retirement this
    /// replaced did on EVERY frame.
    fn retire_unheld(&mut self, cx: &mut App) {
        let mut queued = std::mem::take(&mut self.retiring);
        while queued.len() > RETIRE_QUEUE_CAP {
            cx.drop_image(queued.remove(0), None);
        }
        for image in queued {
            if Arc::strong_count(&image) == 1 {
                cx.drop_image(image, None);
            } else {
                self.retiring.push(image);
            }
        }
    }

    /// The reason the emulator thread ended on its own, in the words
    /// `EmuPanel::finish_run` puts on its status row for the same outcome.
    ///
    /// On a normal exit the thread has ALREADY published its own
    /// `Stopped(reason)` through the endpoint, so this is not how the
    /// world view learns what happened; the join is kept because it is
    /// what reaps the thread.
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
        // Read before the state is written, since `WORLD_PANEL_STOP` is
        // also how a dropped run reports itself.
        let host_stop = self.endpoint.stop_requested();
        self.drop_session(cx);
        if !self.is_stopped() {
            self.endpoint.set_state(ViewerState::Stopped(reason));
        }
        if host_stop {
            // The world tab left live mode: this run will never be asked
            // for anything again, so drop it from the registry now rather
            // than at whatever future boot happens to prune next.
            // Deferred because this runs while the entity is updating.
            let endpoint = self.endpoint.clone();
            cx.defer(move |cx| forget_run(&endpoint, cx));
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
        // Nothing will publish over them now, so the frame in the slot and
        // everything still queued go together -- including any the
        // consumer has not let go of, since there will be no later publish
        // to retire them at.
        for stale in std::mem::take(&mut self.retiring)
            .into_iter()
            .chain(published)
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
        for _ in 0..600 {
            if endpoint.frame_number().is_some() {
                break;
            }
            cx.background_executor
                .timer(std::time::Duration::from_millis(5))
                .await;
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
            assert_eq!(cx.global::<ViewerRuns>().runs.len(), 1, "one run registered");
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
                    .is_none_or(|runs| runs.runs.is_empty()),
                "nothing registered: there is no run to keep alive"
            );
        });
    }

    /// One synthetic presented frame, the size `drive` delivers.
    fn test_frame(number: u32) -> Frame {
        Frame {
            bgra: vec![0; (drive::WIDTH * drive::HEIGHT * 4) as usize],
            number,
            step_ms: 0.0,
        }
    }

    /// The world view paints a published frame LATER, out of a scene it
    /// has already submitted, so a replaced frame is retired only once
    /// nothing else holds it. Retiring one the consumer is still holding
    /// has `paint_image` re-insert a dropped image, leaking exactly the
    /// atlas tile this retirement exists to reclaim.
    #[gpui::test]
    async fn a_frame_the_consumer_still_holds_is_not_retired(cx: &mut TestAppContext) {
        let dir = project_dir();
        // A build that fails leaves the run with no session of its own, so
        // the only frames it publishes are the ones fed in here.
        let runner: ggo_common::ProcRunner = Arc::new(|_| ggo_common::ProcCapture {
            ok: false,
            lines: vec!["error: could not compile demo_editor".to_string()],
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

        run.update(cx, |run, cx| {
            for number in 0..3 {
                assert!(run.on_frame(test_frame(number), cx));
            }
            assert!(
                run.retiring.is_empty(),
                "frames nobody holds are retired as they are replaced"
            );
        });

        // What the world view takes out of the slot to paint.
        let held = endpoint
            .frame
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_number, image)| image.clone())
            .expect("a frame is published");
        run.update(cx, |run, cx| {
            assert!(run.on_frame(test_frame(3), cx));
            assert_eq!(
                run.retiring.len(),
                1,
                "the frame the consumer is still holding waits"
            );
        });

        drop(held);
        run.update(cx, |run, cx| {
            assert!(run.on_frame(test_frame(4), cx));
            assert!(
                run.retiring.is_empty(),
                "and is retired at the first publish after it lets go"
            );
        });
    }

    /// Build one `ViewerRun` for `world_rel` under `dir`, the way `boot`
    /// does. Free-standing so the shared-build tests can create several
    /// runs in ONE turn: what they are about is what happens before any
    /// task has run.
    fn spawn_run(
        cx: &mut TestAppContext,
        dir: &Path,
        world_rel: &str,
        runner: ggo_common::ProcRunner,
    ) -> (Entity<ViewerRun>, Arc<ggo_common::LinkEndpoint>) {
        let endpoint = ggo_common::LinkEndpoint::new();
        let run = cx.new(|cx| {
            ViewerRun::new(
                world_rel.to_string(),
                dir.to_path_buf(),
                runner,
                endpoint.clone(),
                None,
                cx,
            )
        });
        (run, endpoint)
    }

    /// Two world tabs of ONE emerald project are two runs but one build:
    /// `emd editor-cart` takes minutes and blocks a background thread, and
    /// it rewrites the project's single `.ggo` in place -- two of them at
    /// once would have a viewer boot a half-written cart.
    #[gpui::test]
    async fn two_runs_of_one_project_share_a_single_build(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        std::fs::write(dir.path().join("assets/worlds/other.toml"), "").unwrap();
        let (runner, calls) = fake_emd(dir.path(), true);
        let (_run_a, a) = spawn_run(cx, dir.path(), "assets/worlds/main.toml", runner.clone());
        let (_run_b, b) = spawn_run(cx, dir.path(), "assets/worlds/other.toml", runner);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "one `emd editor-cart` for both viewers"
        );
        assert_eq!(a.state(), ggo_common::ViewerState::Running);
        assert_eq!(
            b.state(),
            ggo_common::ViewerState::Running,
            "and both booted their own session from the one `.ggo`"
        );
    }

    /// A save rebuilds every run of the project -- through one more build,
    /// not one per run.
    #[gpui::test]
    async fn a_save_rebuilds_every_run_of_a_project_with_one_build(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        std::fs::write(dir.path().join("assets/worlds/other.toml"), "").unwrap();
        let (runner, calls) = fake_emd(dir.path(), true);
        let (run_a, a) = spawn_run(cx, dir.path(), "assets/worlds/main.toml", runner.clone());
        let (run_b, b) = spawn_run(cx, dir.path(), "assets/worlds/other.toml", runner);
        cx.run_until_parked();
        assert_eq!(calls.lock().unwrap().len(), 1);

        // Both saves land in the same turn, as the shared debounce makes
        // them: it is the joining that is under test, not the timer.
        run_a.update(cx, |run, cx| run.rebuild(cx));
        run_b.update(cx, |run, cx| run.rebuild(cx));
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "the save built the project once, for both viewers"
        );
        assert_eq!(a.state(), ggo_common::ViewerState::Running);
        assert_eq!(b.state(), ggo_common::ViewerState::Running);
    }

    /// Different emerald projects are different builds: sharing is keyed
    /// on the project directory `emd` is run in, not on "a build is in
    /// flight somewhere".
    #[gpui::test]
    async fn runs_of_different_projects_build_separately(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let one = project_dir();
        let two = project_dir();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let (ggo_one, ggo_two) = (
            one.path().join("demo-editor.ggo"),
            two.path().join("demo-editor.ggo"),
        );
        for ggo in [&ggo_one, &ggo_two] {
            std::fs::write(ggo, crate::drive::fixture::green_screen_cart()).unwrap();
        }
        let runner: ggo_common::ProcRunner = Arc::new(move |request: ggo_common::ProcRequest| {
            let ggo = request.cwd.join("demo-editor.ggo");
            recorded.lock().unwrap().push(request);
            ggo_common::ProcCapture {
                ok: true,
                lines: vec![serde_json::json!({ "ggo": ggo }).to_string()],
            }
        });
        let (_run_one, endpoint_one) =
            spawn_run(cx, one.path(), "assets/worlds/main.toml", runner.clone());
        let (_run_two, endpoint_two) =
            spawn_run(cx, two.path(), "assets/worlds/main.toml", runner);
        cx.run_until_parked();
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "each project builds its own cart"
        );
        assert_eq!(endpoint_one.state(), ggo_common::ViewerState::Running);
        assert_eq!(endpoint_two.state(), ggo_common::ViewerState::Running);
    }

    /// A shared build that fails stops every run waiting on it, each with
    /// the compiler's reason -- not just whichever one started it.
    #[gpui::test]
    async fn a_shared_build_failure_stops_every_waiting_run(cx: &mut TestAppContext) {
        let dir = project_dir();
        std::fs::write(dir.path().join("assets/worlds/other.toml"), "").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let attempts = calls.clone();
        let runner: ggo_common::ProcRunner = Arc::new(move |_request| {
            attempts.fetch_add(1, Ordering::SeqCst);
            ggo_common::ProcCapture {
                ok: false,
                lines: vec!["error: could not compile demo_editor".to_string()],
            }
        });
        let (_run_a, a) = spawn_run(cx, dir.path(), "assets/worlds/main.toml", runner.clone());
        let (_run_b, b) = spawn_run(cx, dir.path(), "assets/worlds/other.toml", runner);
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one build for both");
        for state in [a.state(), b.state()] {
            assert!(
                matches!(&state, ggo_common::ViewerState::Stopped(reason) if reason.contains("build failed")),
                "{state:?}"
            );
        }
    }

    /// A run whose build FAILED stays registered when another world view
    /// boots. The registry holds the only handle to it -- the world view
    /// keeps just the endpoint -- so pruning it there would drop its
    /// project watch, and the save that fixes the compile error would
    /// never reach it.
    #[gpui::test]
    async fn a_failed_run_survives_another_viewers_boot(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = project_dir();
        std::fs::write(dir.path().join("assets/worlds/other.toml"), "").unwrap();
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
        let (_db, workspace, _panel, _worktree, cx) =
            crate::tests::run_menu_workspace(cx, dir.path()).await;
        cx.update(|_, cx| {
            cx.set_global(TestViewerRunner(runner));
            cx.set_global(TestViewerRoot(dir.path().to_path_buf()));
        });
        let failed = workspace
            .update_in(cx, |workspace, window, cx| {
                ggo_common::boot_viewer(workspace, "assets/worlds/main.toml", window, cx)
            })
            .expect("the booter claimed the boot");
        cx.run_until_parked();
        assert!(
            matches!(failed.state(), ggo_common::ViewerState::Stopped(reason) if reason.contains("build failed")),
            "{:?}",
            failed.state()
        );

        workspace
            .update_in(cx, |workspace, window, cx| {
                ggo_common::boot_viewer(workspace, "assets/worlds/other.toml", window, cx)
            })
            .expect("the second world view booted too");
        cx.run_until_parked();
        let run = cx
            .update(|_, cx| {
                let runs = &cx.global::<ViewerRuns>().runs;
                assert_eq!(runs.len(), 2, "both runs are registered");
                runs.iter()
                    .find(|run| Arc::ptr_eq(run.read(cx).endpoint(), &failed))
                    .cloned()
            })
            .expect("the failed run outlived the other view's boot");

        run.update(cx, |run, cx| run.on_sources_changed(cx));
        cx.executor().advance_clock(crate::WATCH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            failed.state(),
            ggo_common::ViewerState::Running,
            "and the save that fixed the build still reaches it"
        );
    }

    /// A world view that leaves live mode takes its run out of the
    /// registry with it, rather than leaving it there until some other
    /// view happens to boot.
    #[gpui::test]
    async fn a_run_the_host_stopped_leaves_the_registry(cx: &mut TestAppContext) {
        let dir = project_dir();
        let (_db, workspace, _panel, _worktree, cx) =
            crate::tests::run_menu_workspace(cx, dir.path()).await;
        let (runner, _calls) = fake_emd(dir.path(), false);
        cx.update(|_, cx| {
            cx.set_global(TestViewerRunner(runner));
            cx.set_global(TestViewerRoot(dir.path().to_path_buf()));
        });
        let endpoint = workspace
            .update_in(cx, |workspace, window, cx| {
                ggo_common::boot_viewer(workspace, "assets/worlds/main.toml", window, cx)
            })
            .expect("the booter claimed the boot");
        cx.update(|_, cx| {
            assert_eq!(cx.global::<ViewerRuns>().runs.len(), 1, "one run registered");
        });

        endpoint.request_stop();
        cx.run_until_parked();
        assert!(matches!(
            endpoint.state(),
            ggo_common::ViewerState::Stopped(_)
        ));
        cx.update(|_, cx| {
            assert!(
                cx.global::<ViewerRuns>().runs.is_empty(),
                "the stopped run is not kept alive for the life of the app"
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
