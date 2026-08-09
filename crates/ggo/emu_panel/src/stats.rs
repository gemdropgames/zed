//! The pane's live counters: frames per second, dropped frames, and the
//! per-frame emulator step cost.
//!
//! Ported from `ggo-ide`'s `pages/emulator.rs` -- its `fps`/
//! `fps_window_started`/`fps_window_count` rolling window (`FPS_WINDOW_SECS
//! = 1.0`), its `dropped_total` frame-number gap detection (`on_frame`),
//! and its `last_emu_ms` step cost (`emu/thread.rs::run_loop` times the
//! `step` call plus the framebuffer conversion, excluding any pacing hold).
//!
//! All of it is pure: [`RunStats::on_frame`] takes the elapsed time since
//! the window opened rather than reading a clock, so the whole thing is
//! unit-testable without sleeping.

use std::time::Duration;

/// Rolling window the FPS figure is averaged over -- `ggo-ide`'s
/// `pages/emulator.rs::FPS_WINDOW_SECS` (1.0 s).
pub const FPS_WINDOW: Duration = Duration::from_secs(1);

/// How many frames the emulator produced that the UI never saw, between
/// two consecutively DELIVERED frames.
///
/// The pane's frame channel is bounded at one and the emulator thread
/// `try_send`s, so a UI that falls behind silently loses frames -- but the
/// number each frame carries is the cart's own monotonic counter, so a gap
/// of more than one is exactly the drop count. `saturating_sub` because a
/// new run restarts the counter, which must read as "no drops", not as a
/// gigantic negative-turned-huge gap. `ggo-ide`'s `State::on_frame` does
/// the identical arithmetic on `FrameMsg::frame_idx`.
pub fn dropped_between(previous: u32, current: u32) -> u64 {
    u64::from(current.saturating_sub(previous).saturating_sub(1))
}

/// Frames per second over a closed window.
pub fn fps_over(frames: u32, elapsed: Duration) -> f32 {
    let secs = elapsed.as_secs_f32();
    if secs <= 0.0 {
        return 0.0;
    }
    frames as f32 / secs
}

/// One run's counters. Reset (recreated) per run, exactly as `ggo-ide`'s
/// `State::boot` zeroes `last_frame_idx`/`dropped_total`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunStats {
    /// Last closed window's rate; 0.0 until the first window closes,
    /// matching ggo-ide's `fps: 0.0` initial value.
    pub fps: f32,
    pub dropped: u64,
    /// Wall-clock cost of the most recent frame's emulation + pixel
    /// conversion, in milliseconds (`ggo-ide`'s `last_emu_ms`).
    pub step_ms: f32,
    /// Cart-visible frame number of the last delivered frame; `None`
    /// before the first one, which is what keeps the first frame from
    /// counting as `frame - 1` drops.
    last_frame: Option<u32>,
    /// Frames delivered since the current window opened.
    window_count: u32,
}

impl RunStats {
    /// Fold one delivered frame in. `window_elapsed` is how long the
    /// current FPS window has been open; returns `true` when that window
    /// just closed, which is the caller's cue to restart its clock.
    pub fn on_frame(&mut self, frame: u32, step_ms: f32, window_elapsed: Duration) -> bool {
        if let Some(previous) = self.last_frame {
            self.dropped += dropped_between(previous, frame);
        }
        self.last_frame = Some(frame);
        self.step_ms = step_ms;

        self.window_count += 1;
        if window_elapsed >= FPS_WINDOW {
            self.fps = fps_over(self.window_count, window_elapsed);
            self.window_count = 0;
            return true;
        }
        false
    }

    /// The stats row's text. Mirrors `ggo-ide`'s
    /// `State::status_text` (`"fps: {:.1}   drops: {}   step+blit: {:.2}ms"`),
    /// re-spaced with the middle dots this fork's panels use elsewhere and
    /// without the audio suffix (audio is out of F3 scope).
    pub fn label(&self) -> String {
        format!(
            "fps {:.1} · drops {} · step {:.2}ms",
            self.fps, self.dropped, self.step_ms
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------- drops

    #[test]
    fn consecutive_frames_drop_nothing() {
        assert_eq!(dropped_between(1, 2), 0);
        assert_eq!(dropped_between(41, 42), 0);
    }

    #[test]
    fn a_gap_reports_exactly_the_missing_frames() {
        assert_eq!(dropped_between(1, 2), 0);
        assert_eq!(dropped_between(1, 3), 1);
        assert_eq!(dropped_between(10, 70), 59);
    }

    /// A repeated or rewound frame number (a fresh run restarting the
    /// cart's counter) must read as zero drops, never as a huge gap.
    #[test]
    fn a_repeated_or_rewound_frame_number_reports_no_drops() {
        assert_eq!(dropped_between(5, 5), 0);
        assert_eq!(dropped_between(5, 1), 0);
        assert_eq!(dropped_between(u32::MAX, 0), 0);
    }

    // --------------------------------------------------------------- fps

    #[test]
    fn fps_over_a_window_is_frames_per_second() {
        assert_eq!(fps_over(60, Duration::from_secs(1)), 60.0);
        assert_eq!(fps_over(30, Duration::from_millis(500)), 60.0);
        assert_eq!(fps_over(0, Duration::from_secs(1)), 0.0);
    }

    /// A zero-length window would be a division by zero -- `f32` would
    /// happily produce `inf` and render as "fps inf".
    #[test]
    fn fps_over_a_zero_length_window_is_zero_not_infinity() {
        assert_eq!(fps_over(5, Duration::ZERO), 0.0);
        assert!(fps_over(5, Duration::ZERO).is_finite());
    }

    // ---------------------------------------------------------- RunStats

    #[test]
    fn the_first_frame_of_a_run_counts_no_drops() {
        let mut stats = RunStats::default();
        stats.on_frame(9_000, 1.0, Duration::ZERO);
        assert_eq!(stats.dropped, 0, "a run joined mid-count drops nothing");
    }

    #[test]
    fn drops_accumulate_across_frames() {
        let mut stats = RunStats::default();
        for frame in [1u32, 2, 5, 6, 10] {
            stats.on_frame(frame, 0.0, Duration::ZERO);
        }
        assert_eq!(stats.dropped, 2 + 3);
    }

    #[test]
    fn the_fps_window_closes_only_once_it_is_full_and_then_restarts() {
        let mut stats = RunStats::default();
        assert!(!stats.on_frame(1, 0.0, Duration::from_millis(500)));
        assert_eq!(stats.fps, 0.0, "no rate before the first window closes");

        assert!(
            stats.on_frame(2, 0.0, FPS_WINDOW),
            "a full window must report closed"
        );
        assert_eq!(stats.fps, 2.0, "two frames in one second");

        // The window count restarted, so the next second is measured on
        // its own frames only.
        assert!(!stats.on_frame(3, 0.0, Duration::from_millis(10)));
        assert!(stats.on_frame(4, 0.0, FPS_WINDOW));
        assert_eq!(stats.fps, 2.0);
    }

    #[test]
    fn step_ms_is_the_latest_frames_cost() {
        let mut stats = RunStats::default();
        stats.on_frame(1, 4.5, Duration::ZERO);
        assert_eq!(stats.step_ms, 4.5);
        stats.on_frame(2, 0.25, Duration::ZERO);
        assert_eq!(stats.step_ms, 0.25);
    }

    #[test]
    fn label_pins_the_formatting() {
        let stats = RunStats {
            fps: 59.94,
            dropped: 7,
            step_ms: 1.234,
            ..RunStats::default()
        };
        assert_eq!(stats.label(), "fps 59.9 · drops 7 · step 1.23ms");
    }
}
