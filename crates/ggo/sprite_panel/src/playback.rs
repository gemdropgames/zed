//! Pure playback-range and strip-layout math for the sprite panel.
//! The transport semantics mirror ggo-ide's `sprites/timeline.rs`
//! (`play_range`/`play_loop`/`toggle_play`'s start-offset seeding); the
//! per-timestamp frame walk itself lives in worldlib
//! (`timeline_ops::playback_frame_at`) -- this module only resolves the
//! ACTIVE range/loop pair out of the clip list and computes where in that
//! range playback starts, so the panel's timer loop stays a thin caller.

use ggo_worldlib::sprites::cow::ClipEdit;
use ggo_worldlib::sprites::timeline_ops::MIN_FRAME_MS;

/// The active playback range: the active clip's `(from, to)` (which may
/// be stored reversed -- worldlib's walk normalizes), or the whole strip
/// `(0, frame_count - 1)` when no clip is active (or the index has gone
/// stale against the clip list) -- ggo-ide `timeline::State::play_range`.
pub fn play_range(
    clips: &[ClipEdit],
    active_clip: Option<usize>,
    frame_count: usize,
) -> (usize, usize) {
    match active_clip.and_then(|i| clips.get(i)) {
        Some(c) => (c.from, c.to),
        None => (0, frame_count.saturating_sub(1)),
    }
}

/// The active loop flag: the active clip's own `loop_`, or `true` for
/// whole-strip playback -- ggo-ide `timeline::State::play_loop`
/// (`Timeline.tsx`'s `activeClip()?.loop ?? true`).
pub fn play_loop(clips: &[ClipEdit], active_clip: Option<usize>) -> bool {
    active_clip
        .and_then(|i| clips.get(i))
        .is_none_or(|c| c.loop_)
}

/// The elapsed-ms seed that makes playback START on `from` (clamped into
/// the range): the sum of the range's frame durations strictly before it,
/// floored to [`MIN_FRAME_MS`] exactly like `playback_frame_at`'s own
/// accounting -- ggo-ide `timeline::State::toggle_play`'s `start_ms` loop.
pub fn start_offset_ms(durations: &[u16], range: (usize, usize), from: usize) -> i64 {
    let lo = range.0.min(range.1);
    let hi = range.0.max(range.1);
    let from = from.clamp(lo, hi);
    let mut ms = 0i64;
    for i in lo..from {
        ms += i64::from(durations.get(i).copied().unwrap_or(0).max(MIN_FRAME_MS));
    }
    ms
}

/// Fit a `w`x`h` image into a `max_px` square preserving aspect ratio
/// (upscaling allowed -- these are 16px-tile pixel sprites, a tiny frame
/// SHOULD grow to the box). Zero-sized input maps to a zero-sized box.
pub fn fit_size(w: u32, h: u32, max_px: f32) -> (f32, f32) {
    if w == 0 || h == 0 {
        return (0.0, 0.0);
    }
    let scale = (max_px / w as f32).min(max_px / h as f32);
    (w as f32 * scale, h as f32 * scale)
}


/// The on-screen size for the big preview's image: the FRAME's
/// dimensions pick the fit scale (into `box_px`), and the image -- which
/// may be the transform composer's doubled canvas -- displays at that
/// same pixels-per-texel scale. A rotated frame therefore keeps its
/// on-screen size and simply owns a larger canvas, instead of being
/// shrunk to squeeze the doubled bounds into the box.
pub fn preview_display_size(
    image_w: u32,
    image_h: u32,
    frame_w: u32,
    frame_h: u32,
    box_px: f32,
) -> (f32, f32) {
    let (fit_w, fit_h) = fit_size(frame_w, frame_h, box_px);
    if frame_w == 0 || frame_h == 0 {
        return (fit_w, fit_h);
    }
    (
        fit_w * image_w as f32 / frame_w as f32,
        fit_h * image_h as f32 / frame_h as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::timeline_ops::{playback_frame_at, playback_total_ms};

    fn clip(name: &str, from: usize, to: usize, loop_: bool) -> ClipEdit {
        ClipEdit {
            name: name.to_string(),
            from,
            to,
            loop_,
        }
    }

    #[test]
    fn play_range_none_is_the_whole_strip() {
        assert_eq!(play_range(&[], None, 4), (0, 3));
        assert_eq!(play_range(&[clip("walk", 1, 2, true)], None, 4), (0, 3));
    }

    #[test]
    fn play_range_active_clip_is_its_from_to() {
        let clips = [clip("idle", 0, 0, true), clip("walk", 1, 3, false)];
        assert_eq!(play_range(&clips, Some(1), 4), (1, 3));
    }

    #[test]
    fn play_range_stale_clip_index_falls_back_to_the_whole_strip() {
        assert_eq!(play_range(&[clip("walk", 1, 2, true)], Some(9), 4), (0, 3));
    }

    #[test]
    fn play_range_of_an_empty_strip_is_0_0() {
        assert_eq!(play_range(&[], None, 0), (0, 0));
    }

    #[test]
    fn play_loop_defaults_true_for_whole_strip_and_reads_the_clips_flag() {
        let clips = [clip("once", 0, 2, false), clip("cycle", 0, 2, true)];
        assert!(play_loop(&clips, None));
        assert!(!play_loop(&clips, Some(0)));
        assert!(play_loop(&clips, Some(1)));
        assert!(play_loop(&clips, Some(9)), "stale index = whole-strip loop");
    }

    #[test]
    fn start_offset_ms_sums_floored_durations_before_the_start_frame() {
        let d = [100u16, 200, 50];
        assert_eq!(start_offset_ms(&d, (0, 2), 0), 0);
        assert_eq!(start_offset_ms(&d, (0, 2), 2), 300);
        // 50 floors to MIN_FRAME_MS in playback_frame_at's accounting too.
        let d2 = [100u16, 0, 100];
        assert_eq!(
            start_offset_ms(&d2, (0, 2), 2),
            100 + i64::from(MIN_FRAME_MS)
        );
    }

    #[test]
    fn start_offset_ms_clamps_the_start_frame_into_the_range() {
        let d = [100u16, 200, 50];
        // Frame 0 selected but the range starts at 1: start ON frame 1.
        assert_eq!(start_offset_ms(&d, (1, 2), 0), 0);
        // Selected past the range end: start on the last frame.
        assert_eq!(start_offset_ms(&d, (0, 1), 5), 100);
    }

    /// The brief's test (b): clip-range playback frame selection at
    /// synthetic timestamps -- the panel-side integration of
    /// `play_range`/`play_loop` with worldlib's `playback_frame_at`, no
    /// timer involved.
    #[test]
    fn clip_range_playback_hits_expected_frames_at_synthetic_timestamps() {
        let d = [100u16, 200, 50];
        let clips = [clip("walk", 1, 2, true), clip("once", 1, 2, false)];

        // Looping clip over frames 1..=2 (durations 200 + 50): walks,
        // wraps.
        let range = play_range(&clips, Some(0), 3);
        let loop_ = play_loop(&clips, Some(0));
        assert_eq!(range, (1, 2));
        assert!(loop_);
        assert_eq!(playback_frame_at(&d, range, 0, loop_), 1);
        assert_eq!(playback_frame_at(&d, range, 199, loop_), 1);
        assert_eq!(playback_frame_at(&d, range, 200, loop_), 2);
        assert_eq!(playback_frame_at(&d, range, 249, loop_), 2);
        assert_eq!(playback_frame_at(&d, range, 250, loop_), 1, "wraps");

        // Same range, non-looping: holds on the last frame past total.
        let loop_ = play_loop(&clips, Some(1));
        assert!(!loop_);
        assert_eq!(playback_total_ms(&d, range), 250);
        assert_eq!(playback_frame_at(&d, range, 9_999, loop_), 2);

        // No active clip: whole strip, loop default, wraps at 350.
        let range = play_range(&clips, None, 3);
        let loop_ = play_loop(&clips, None);
        assert_eq!(playback_frame_at(&d, range, 340, loop_), 2);
        assert_eq!(playback_frame_at(&d, range, 350, loop_), 0);
    }

    #[test]
    fn fit_size_fits_within_the_box_preserving_aspect() {
        assert_eq!(fit_size(16, 16, 48.0), (48.0, 48.0)); // upscale square
        assert_eq!(fit_size(32, 16, 48.0), (48.0, 24.0)); // wide
        assert_eq!(fit_size(16, 64, 32.0), (8.0, 32.0)); // tall
        assert_eq!(fit_size(0, 16, 48.0), (0.0, 0.0));
    }

    #[test]
    fn preview_display_size_keeps_the_texel_scale_constant() {
        // Identity: image == frame -> the plain fit.
        assert_eq!(preview_display_size(16, 16, 16, 16, 240.0), fit_size(16, 16, 240.0));
        // A doubled (transformed) canvas shows at exactly TWICE the
        // identity fit -- same pixels-per-texel, bigger canvas -- rather
        // than being squeezed into the same box at half scale.
        let (fw, fh) = fit_size(16, 16, 240.0);
        assert_eq!(preview_display_size(32, 32, 16, 16, 240.0), (fw * 2.0, fh * 2.0));
    }
}
