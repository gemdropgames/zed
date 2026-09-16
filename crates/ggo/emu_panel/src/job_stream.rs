//! Hardware runs over the daemon instead of as child processes.
//!
//! The hardware page drives every long run -- a flash, a diagnostic, the
//! setup steps that install things -- through one
//! [`ggo_common::ProcStreamer`]: give it a [`ggo_common::ProcRequest`] and
//! a line sink, get a future that yields the transcript and kills the work
//! when dropped. [`EmuPanel::start_board_run`](crate::EmuPanel) owns the
//! console, the progress grammar and the cancel button on top of that one
//! seam.
//!
//! This module supplies a streamer with the same signature that runs `ggo
//! diag` **inside the daemon** rather than as a child of this editor. The
//! panel above it does not change at all: same request, same lines, same
//! cancel -- see `docs/daemon-api-plan.md` §6's P0 in the GGO repo for why
//! the work is moving there.
//!
//! # A router, not a replacement
//!
//! Only `ggo diag` moves. The setup steps run `git`, `cargo install` and
//! an `sh -c` update script, which are this machine's business and have no
//! daemon tool -- those keep spawning as children, through the fallback
//! streamer. [`is_daemon_run`] is the whole routing rule.
//!
//! # Polling, not subscribing
//!
//! The daemon's socket is request/response with no server-initiated
//! frames, so following a job means asking again from the last line index
//! seen. [`POLL_INTERVAL`] is the tick. The sleep is
//! `smol::unblock`-wrapped rather than `smol::Timer::after`, which this
//! checkout's `clippy.toml` disallows outright.

use std::sync::Arc;

use ggo_common::{LineSink, ProcCapture, ProcRequest, ProcStreamer};
use ggo_daemon_client::{Client, JobState};

use crate::menu::DIAG_MODE_ARG;

/// How often a running job is asked for new lines.
///
/// A compromise the transcript itself sets the scale for: `ggo-diag`
/// prints a handful of lines per phase over minutes, so a tick far below
/// human reaction time buys nothing and costs a socket round trip. It is
/// also the worst-case lag between pressing cancel and the daemon hearing
/// about it, since a drop is only noticed at an await point.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// How a client is obtained. Injected so tests can hand back a
/// [`ggo_daemon_client::FakeDaemon`]-backed client instead of connecting
/// to a real daemon -- the same reason [`ProcStreamer`] itself is a seam.
pub type Connect = Arc<dyn Fn() -> anyhow::Result<Arc<Client>> + Send + Sync>;

/// Connect to the daemon named by the environment, starting it if needed.
pub fn system_connect() -> Connect {
    Arc::new(|| Client::connect().map(Arc::new))
}

/// Is this request one the daemon runs?
///
/// Keyed on the mode argument this fork itself puts at the front of a
/// diagnostic argv ([`DIAG_MODE_ARG`]), not on the binary's name: the
/// binary is overridable (`GGO_DIAG_BIN`) and may be an absolute path, but
/// a request whose first argument is `diag` is a `ggo diag` run by
/// construction -- `menu::diag_args` and `hardware::flash_args` are the
/// only two things that build one.
pub fn is_daemon_run(request: &ProcRequest) -> bool {
    request.args.first().is_some_and(|arg| arg == DIAG_MODE_ARG)
}

/// A [`ProcStreamer`] that sends `ggo diag` runs to the daemon and
/// everything else to `fallback`.
pub fn daemon_proc_streamer(connect: Connect, fallback: ProcStreamer) -> ProcStreamer {
    Arc::new(move |request, on_line| {
        if !is_daemon_run(&request) {
            return fallback(request, on_line);
        }
        let connect = connect.clone();
        Box::pin(async move { run_job(connect, request, on_line).await })
    })
}

/// The production streamer: daemon for diagnostics, child processes for
/// the rest.
pub fn system_daemon_streamer() -> ProcStreamer {
    daemon_proc_streamer(system_connect(), ggo_common::system_proc_streamer())
}

/// Cancels the job if the future is dropped before it finishes.
///
/// This is the cancel button. `start_board_run` cancels by dropping the
/// run future, which used to reach the child through `kill_on_drop`; with
/// the work inside the daemon the same drop has to become a `ggo_job_cancel`
/// or the board's serial port stays held by a run nobody is watching.
///
/// The cancel is sent from a detached thread rather than inline: `Drop`
/// cannot await, and a socket round trip on whatever thread happens to be
/// dropping the future is not this module's to spend.
struct CancelOnDrop {
    client: Arc<Client>,
    job: u64,
    finished: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let client = self.client.clone();
        let job = self.job;
        std::thread::spawn(move || {
            if let Err(error) = client.job_cancel(job) {
                log::warn!("cancelling GemdropGo job {job}: {error:#}");
            }
        });
    }
}

/// Start `request` as a daemon job and follow it to its end.
async fn run_job(connect: Connect, request: ProcRequest, mut on_line: LineSink) -> ProcCapture {
    // Every failure below is reported the way a failed spawn is: one line
    // naming what went wrong, into the console AND the transcript, with
    // `ok: false`. A run that cannot start must never be a silent no-op --
    // the hardware page has nothing else to show.
    let mut lines: Vec<String> = Vec::new();
    let fail = |message: String, on_line: &mut LineSink, lines: &mut Vec<String>| {
        on_line(&message);
        lines.push(message);
    };

    let client = match smol::unblock(move || connect()).await {
        Ok(client) => client,
        Err(error) => {
            fail(
                format!("GemdropGo daemon unavailable: {error:#}"),
                &mut on_line,
                &mut lines,
            );
            return ProcCapture { ok: false, lines };
        }
    };

    // The daemon prepends its own mode argument, so the one this fork put
    // at the front comes back off here -- otherwise the child would be
    // asked to run `ggo diag diag …`.
    let args: Vec<String> = request.args.iter().skip(1).cloned().collect();
    let job = {
        let client = client.clone();
        match smol::unblock(move || client.diag_start(args)).await {
            Ok(job) => job,
            Err(error) => {
                fail(
                    format!("could not start the hardware run: {error:#}"),
                    &mut on_line,
                    &mut lines,
                );
                return ProcCapture { ok: false, lines };
            }
        }
    };

    let mut guard = CancelOnDrop {
        client: client.clone(),
        job: job.id,
        finished: false,
    };
    let mut since = 0usize;
    loop {
        let batch = {
            let client = client.clone();
            let job_id = job.id;
            match smol::unblock(move || client.job_lines(job_id, since)).await {
                Ok(batch) => batch,
                Err(error) => {
                    // The job may well still be running inside the daemon;
                    // the guard's cancel on the way out is what stops it.
                    fail(
                        format!("lost contact with the hardware run: {error:#}"),
                        &mut on_line,
                        &mut lines,
                    );
                    return ProcCapture { ok: false, lines };
                }
            }
        };
        for line in &batch.lines {
            on_line(&line.text);
            lines.push(line.text.clone());
        }
        since = batch.next_since(since);

        match batch.state {
            JobState::Running => {}
            state => {
                // Finished under its own power: nothing left to cancel,
                // and cancelling a finished job would be a stray SIGINT.
                guard.finished = true;
                if let JobState::Failed { error } = &state {
                    fail(
                        format!("the hardware run did not start: {error}"),
                        &mut on_line,
                        &mut lines,
                    );
                }
                return ProcCapture {
                    ok: !state.is_error(),
                    lines,
                };
            }
        }
        // Dropping the future parks here, which is where the guard fires.
        smol::unblock(|| std::thread::sleep(POLL_INTERVAL)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_daemon_client::FakeDaemon;
    use serde_json::json;
    use std::sync::Mutex;

    fn connect_to(fake: &Arc<FakeDaemon>) -> Connect {
        let fake = fake.clone();
        Arc::new(move || Client::with_transport(fake.transport()).map(Arc::new))
    }

    /// A sink that records, plus the handle to read it back.
    fn recording_sink() -> (LineSink, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink: LineSink = {
            let seen = seen.clone();
            Box::new(move |line: &str| {
                if let Ok(mut seen) = seen.lock() {
                    seen.push(line.to_string());
                }
            })
        };
        (sink, seen)
    }

    /// A streamer that must never be reached, for the daemon-routed cases.
    fn unused_fallback() -> ProcStreamer {
        Arc::new(|request: ProcRequest, _| {
            panic!("the fallback ran for {}", request.command_line())
        })
    }

    fn diag_request() -> ProcRequest {
        ProcRequest::new(
            "ggo",
            "/repo",
            vec![
                DIAG_MODE_ARG.to_string(),
                "--repo".to_string(),
                "/repo".to_string(),
                "--launch".to_string(),
            ],
        )
    }

    /// Only `ggo diag` moves to the daemon. `git`, `cargo install` and the
    /// `sh -c` update script are this machine's business and have no
    /// daemon tool -- routing them there would break setup entirely.
    #[test]
    fn only_diagnostics_are_routed_to_the_daemon() {
        assert!(is_daemon_run(&diag_request()));
        assert!(!is_daemon_run(&ProcRequest::new(
            "git",
            "/repo",
            vec!["clone".into(), "https://example/ggo".into()]
        )));
        assert!(!is_daemon_run(&ProcRequest::new(
            "cargo",
            "/repo",
            vec!["install".into(), "--locked".into()]
        )));
        assert!(!is_daemon_run(&ProcRequest::new(
            "sh",
            "/repo",
            vec!["-c".into(), "set -e; git pull".into()]
        )));
        assert!(
            !is_daemon_run(&ProcRequest::new("ggo", "/repo", Vec::new())),
            "an empty argv is not a diagnostic"
        );
    }

    #[test]
    fn a_non_daemon_request_goes_to_the_fallback_streamer() {
        let taken = Arc::new(Mutex::new(false));
        let fallback: ProcStreamer = {
            let taken = taken.clone();
            Arc::new(move |_request, _on_line| {
                if let Ok(mut taken) = taken.lock() {
                    *taken = true;
                }
                Box::pin(async {
                    ProcCapture {
                        ok: true,
                        lines: vec!["from the fallback".to_string()],
                    }
                })
            })
        };
        let fake = FakeDaemon::new();
        let streamer = daemon_proc_streamer(connect_to(&fake), fallback);

        let (sink, _seen) = recording_sink();
        let capture = smol::block_on(streamer(
            ProcRequest::new("git", "/repo", vec!["clone".into()]),
            sink,
        ));
        assert!(capture.ok);
        assert_eq!(capture.lines, vec!["from the fallback".to_string()]);
        assert_eq!(*taken.lock().unwrap(), true);
        assert!(
            fake.calls().iter().all(|(name, _)| name == "initialize"),
            "the daemon must not be asked to run a git clone: {:?}",
            fake.calls()
        );
    }

    /// The transcript must arrive through the sink as it lands AND be
    /// returned at the end: the console reads the first, the verdict
    /// parser the second.
    #[test]
    fn a_finished_job_streams_its_lines_and_reports_success() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_start",
            json!({"id": 1, "args": ["diag"], "state": "running", "line_count": 0}),
        );
        fake.on_tool(
            "ggo_job_lines",
            json!({
                "lines": [
                    {"index": 0, "text": "==> Flash board"},
                    {"index": 1, "text": "RESULT: PASS"},
                ],
                "state": "done",
                "exit_code": 0,
            }),
        );
        let streamer = daemon_proc_streamer(connect_to(&fake), unused_fallback());

        let (sink, seen) = recording_sink();
        let capture = smol::block_on(streamer(diag_request(), sink));

        assert!(capture.ok);
        assert_eq!(capture.lines, vec!["==> Flash board", "RESULT: PASS"]);
        assert_eq!(*seen.lock().unwrap(), capture.lines);
    }

    /// A non-zero exit is the tool's own verdict on itself, and
    /// `start_board_run` styles the run from `ok`.
    #[test]
    fn a_failing_job_reports_a_failed_capture() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_start",
            json!({"id": 1, "args": ["diag"], "state": "running", "line_count": 0}),
        );
        fake.on_tool(
            "ggo_job_lines",
            json!({
                "lines": [{"index": 0, "text": "fujprog: no board"}],
                "state": "done",
                "exit_code": 2,
            }),
        );
        let streamer = daemon_proc_streamer(connect_to(&fake), unused_fallback());

        let (sink, _seen) = recording_sink();
        let capture = smol::block_on(streamer(diag_request(), sink));
        assert!(!capture.ok);
        assert_eq!(capture.lines, vec!["fujprog: no board"]);
    }

    /// The daemon prepends its own mode argument, so ours must come off --
    /// otherwise the job runs `ggo diag diag --launch` and exits 2.
    #[test]
    fn the_mode_argument_is_not_sent_twice() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_start",
            json!({"id": 1, "args": ["diag"], "state": "running", "line_count": 0}),
        );
        fake.on_tool("ggo_job_lines", json!({"lines": [], "state": "done", "exit_code": 0}));
        let streamer = daemon_proc_streamer(connect_to(&fake), unused_fallback());

        let (sink, _seen) = recording_sink();
        smol::block_on(streamer(diag_request(), sink));

        let calls = fake.calls();
        let (_, arguments) = calls
            .iter()
            .find(|(name, _)| name == "ggo_diag_start")
            .expect("the diagnostic was started");
        assert_eq!(
            arguments["args"],
            json!(["--repo", "/repo", "--launch"]),
            "the leading `diag` belongs to the daemon, not the argv"
        );
    }

    /// "The daemon isn't running" is the likeliest first-run failure. It
    /// has to reach the console as text, not vanish.
    #[test]
    fn a_daemon_that_cannot_be_reached_fails_the_run_with_a_reason() {
        let connect: Connect = Arc::new(|| Err(anyhow::anyhow!("connect to /run/ggo.sock: absent")));
        let streamer = daemon_proc_streamer(connect, unused_fallback());

        let (sink, seen) = recording_sink();
        let capture = smol::block_on(streamer(diag_request(), sink));

        assert!(!capture.ok);
        assert_eq!(capture.lines.len(), 1);
        assert!(capture.lines[0].contains("/run/ggo.sock"), "{:?}", capture.lines);
        assert_eq!(*seen.lock().unwrap(), capture.lines, "the console sees it too");
    }

    /// The daemon runs one diagnostic at a time; a refusal must land on
    /// the page rather than looking like a run that produced nothing.
    #[test]
    fn a_refused_start_fails_the_run_with_the_daemons_reason() {
        let fake = FakeDaemon::new();
        fake.on_tool_error("ggo_diag_start", "a diag job is already running (id 3)");
        let streamer = daemon_proc_streamer(connect_to(&fake), unused_fallback());

        let (sink, _seen) = recording_sink();
        let capture = smol::block_on(streamer(diag_request(), sink));

        assert!(!capture.ok);
        assert!(capture.lines[0].contains("id 3"), "{:?}", capture.lines);
    }

    /// A job the daemon could not spawn at all reports why.
    #[test]
    fn a_job_that_never_started_reports_its_error() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_start",
            json!({"id": 1, "args": ["diag"], "state": "running", "line_count": 0}),
        );
        fake.on_tool(
            "ggo_job_lines",
            json!({"lines": [], "state": "failed", "error": "spawn ggo: No such file"}),
        );
        let streamer = daemon_proc_streamer(connect_to(&fake), unused_fallback());

        let (sink, _seen) = recording_sink();
        let capture = smol::block_on(streamer(diag_request(), sink));

        assert!(!capture.ok);
        assert!(
            capture.lines.iter().any(|l| l.contains("No such file")),
            "{:?}",
            capture.lines
        );
    }

    /// **The cancel button.** `start_board_run` cancels by dropping the
    /// run future; with the work inside the daemon that drop has to become
    /// a cancel call, or the board's serial port stays held by a run
    /// nobody is watching.
    #[test]
    fn dropping_the_future_cancels_the_job_in_the_daemon() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_start",
            json!({"id": 9, "args": ["diag"], "state": "running", "line_count": 0}),
        );
        // Never finishes: the only way out of this run is a cancel.
        fake.on_tool("ggo_job_lines", json!({"lines": [], "state": "running"}));
        fake.on_tool("ggo_job_cancel", json!({"cancelled": true}));
        let streamer = daemon_proc_streamer(connect_to(&fake), unused_fallback());

        let (sink, _seen) = recording_sink();
        let mut future = streamer(diag_request(), sink);

        // Poll it far enough to have started the job, then drop it.
        let started = std::time::Instant::now();
        smol::block_on(async {
            while started.elapsed() < std::time::Duration::from_secs(5) {
                if fake
                    .calls()
                    .iter()
                    .any(|(name, _)| name == "ggo_job_lines")
                {
                    return;
                }
                let polled = futures::poll!(&mut future);
                assert!(polled.is_pending(), "a never-finishing job must not finish");
                smol::future::yield_now().await;
            }
            panic!("the job never started");
        });
        drop(future);

        // The cancel is sent from a detached thread, so wait for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if fake
                .calls()
                .iter()
                .any(|(name, _)| name == "ggo_job_cancel")
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("dropping the run future did not cancel the job: {:?}", fake.calls());
    }

    /// A job that ended on its own must NOT be cancelled -- that would be
    /// a stray SIGINT at whatever the daemon runs next.
    #[test]
    fn a_job_that_finished_is_not_cancelled_afterwards() {
        let fake = FakeDaemon::new();
        fake.on_tool(
            "ggo_diag_start",
            json!({"id": 1, "args": ["diag"], "state": "running", "line_count": 0}),
        );
        fake.on_tool(
            "ggo_job_lines",
            json!({"lines": [], "state": "done", "exit_code": 0}),
        );
        let streamer = daemon_proc_streamer(connect_to(&fake), unused_fallback());

        let (sink, _seen) = recording_sink();
        let capture = smol::block_on(streamer(diag_request(), sink));
        assert!(capture.ok);

        // Give a stray cancel thread every chance to show up.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !fake.calls().iter().any(|(name, _)| name == "ggo_job_cancel"),
            "{:?}",
            fake.calls()
        );
    }
}
