//! A headless emulator run for one world view's viewer cart. It builds
//! `emd editor-cart --ggo`, boots the `.ggo` on its own emulator thread,
//! and publishes every presented frame into the `LinkEndpoint` the world
//! view polls. It is not the emulator pane: it opens no tab, shows no
//! status, and any number of them can exist at once (one per world tab).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ggo_common::{LinkEndpoint, ProcRequest, ProcRunner, ViewerState};
use gpui::{App, AppContext as _, Context, Entity, RenderImage, Subscription, Task};

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
        // The pump too, not just the session: a pump left running would
        // keep publishing the outgoing run's last frames into the
        // endpoint mid-build, and would report that run's ending over the
        // `Building` this is about to write. Safe here and not in
        // `end_run`, because a rebuild is never asked for from inside the
        // pump task itself.
        self._pump_task = None;
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
        // Not in `end_run`: that one is also called from inside this
        // task, and a task that drops its own handle is exactly what
        // `EmuPanel::stop_for_world_panel` exists to avoid.
        self._pump_task = None;
    }

    /// Report `reason` and let go of the emulator thread and its frames.
    fn end_run(&mut self, reason: String, cx: &mut App) {
        if !self.is_stopped() {
            self.endpoint.set_state(ViewerState::Stopped(reason));
        }
        self.drop_session(cx);
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Mutex;

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
            let _ = ticks.try_recv();
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
}
