//! Pure playback-timing and strip-layout math for the sprite panel.
//! The transport semantics mirror ggo-ide's `sprites/timeline.rs`
//! (`play_loop`/`toggle_play`'s start-offset seeding); the
//! per-timestamp walk itself lives in worldlib
//! (`timeline_ops::playback_frame_at`) -- this module only resolves the
//! ACTIVE duration list and loop flag out of the clip list and computes
//! where in that list playback starts, so the panel's timer loop stays a
//! thin caller.

use ggo_worldlib::sprites::cow::{ClipEdit, DEFAULT_FRAME_DURATION_MS, SpriteState};
use ggo_worldlib::sprites::timeline_ops::{MIN_FRAME_MS, entry_durations};

/// The durations the transport walks: the active clip's entries in
/// sequence order, or -- for "All frames" (no clip, or a stale index) --
/// every frame once at the default duration.
pub fn play_durations(state: &SpriteState, active_clip: Option<usize>) -> Vec<u16> {
    match active_clip.and_then(|i| state.clips.get(i)) {
        Some(c) => entry_durations(&c.entries),
        None => vec![DEFAULT_FRAME_DURATION_MS; state.frames.len()],
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

/// The elapsed-ms seed that makes playback START at position `from`
/// (clamped into the list): the sum of the durations strictly before it,
/// floored to [`MIN_FRAME_MS`] exactly like `playback_frame_at`'s own
/// accounting -- ggo-ide `timeline::State::toggle_play`'s `start_ms` loop.
pub fn start_offset_ms(durations: &[u16], from: usize) -> i64 {
    let from = from.min(durations.len().saturating_sub(1));
    durations[..from]
        .iter()
        .map(|&d| i64::from(d.max(MIN_FRAME_MS)))
        .sum()
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
    use ggo_worldlib::sprites::cow::ClipEntry;
    use ggo_worldlib::sprites::sprite_doc::blank_sprite_state;
    use ggo_worldlib::sprites::timeline_ops::{playback_frame_at, playback_total_ms};

    /// A clip over frames `0..durations.len()`, one entry each, timed by
    /// `durations`.
    fn clip(name: &str, durations: &[u16], loop_: bool) -> ClipEdit {
        ClipEdit {
            name: name.to_string(),
            loop_,
            entries: durations
                .iter()
                .enumerate()
                .map(|(frame, &duration_ms)| ClipEntry {
                    duration_ms,
                    ..ClipEntry::of_frame(frame)
                })
                .collect(),
        }
    }

    fn state(frame_count: usize, clips: Vec<ClipEdit>) -> SpriteState {
        let mut state = blank_sprite_state(1, 1).expect("a 1x1 blank sprite");
        let blank = state.frames[0].clone();
        state.frames = vec![blank; frame_count];
        state.clips = clips;
        state
    }

    #[test]
    fn play_durations_of_the_active_clip_are_its_entries_in_sequence_order() {
        let s = state(4, vec![clip("walk", &[100, 250], false)]);
        assert_eq!(play_durations(&s, Some(0)), vec![100, 250]);
    }

    #[test]
    fn play_durations_without_a_clip_is_every_frame_at_the_default() {
        let s = state(3, vec![clip("walk", &[100, 250], false)]);
        assert_eq!(play_durations(&s, None), vec![DEFAULT_FRAME_DURATION_MS; 3]);
    }

    #[test]
    fn play_durations_of_a_stale_clip_index_falls_back_to_all_frames() {
        let s = state(3, vec![clip("walk", &[100, 250], false)]);
        assert_eq!(
            play_durations(&s, Some(9)),
            vec![DEFAULT_FRAME_DURATION_MS; 3]
        );
    }

    #[test]
    fn play_durations_of_an_empty_strip_is_empty() {
        assert_eq!(
            play_durations(&state(0, Vec::new()), None),
            Vec::<u16>::new()
        );
    }

    #[test]
    fn play_loop_defaults_true_for_whole_strip_and_reads_the_clips_flag() {
        let clips = [
            clip("once", &[100, 100], false),
            clip("cycle", &[100, 100], true),
        ];
        assert!(play_loop(&clips, None));
        assert!(!play_loop(&clips, Some(0)));
        assert!(play_loop(&clips, Some(1)));
        assert!(play_loop(&clips, Some(9)), "stale index = whole-strip loop");
    }

    #[test]
    fn start_offset_ms_sums_floored_durations_before_the_start_position() {
        assert_eq!(start_offset_ms(&[100, 250, 10], 0), 0);
        assert_eq!(start_offset_ms(&[100, 250, 10], 2), 350);
        // A sub-floor duration counts as MIN_FRAME_MS here, exactly as
        // playback_frame_at's own accounting floors it.
        assert_eq!(
            start_offset_ms(&[100, 0, 100], 2),
            100 + i64::from(MIN_FRAME_MS)
        );
    }

    #[test]
    fn start_offset_ms_clamps_the_start_position_into_the_list() {
        assert_eq!(start_offset_ms(&[100, 250, 10], 9), 350, "past the end");
        assert_eq!(start_offset_ms(&[], 3), 0, "nothing to sum");
    }

    /// The panel-side integration of `play_durations`/`play_loop` with
    /// worldlib's `playback_frame_at`, at synthetic timestamps, no timer
    /// involved. The result is a POSITION in the walked list.
    #[test]
    fn clip_playback_hits_expected_positions_at_synthetic_timestamps() {
        let s = state(
            3,
            vec![
                clip("walk", &[200, 50], true),
                clip("once", &[200, 50], false),
            ],
        );

        // Looping clip of two entries (200 + 50): walks, then wraps.
        let durations = play_durations(&s, Some(0));
        let loop_ = play_loop(&s.clips, Some(0));
        assert!(loop_);
        assert_eq!(playback_frame_at(&durations, 0, loop_), 0);
        assert_eq!(playback_frame_at(&durations, 199, loop_), 0);
        assert_eq!(playback_frame_at(&durations, 200, loop_), 1);
        assert_eq!(playback_frame_at(&durations, 249, loop_), 1);
        assert_eq!(playback_frame_at(&durations, 250, loop_), 0, "wraps");

        // Same timings, non-looping: holds on the last entry past total.
        let durations = play_durations(&s, Some(1));
        let loop_ = play_loop(&s.clips, Some(1));
        assert!(!loop_);
        assert_eq!(playback_total_ms(&durations), 250);
        assert_eq!(playback_frame_at(&durations, 9_999, loop_), 1);

        // No active clip: every frame at the default, looping, wraps at 300.
        let durations = play_durations(&s, None);
        let loop_ = play_loop(&s.clips, None);
        assert_eq!(playback_frame_at(&durations, 290, loop_), 2);
        assert_eq!(playback_frame_at(&durations, 300, loop_), 0);
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
        assert_eq!(
            preview_display_size(16, 16, 16, 16, 240.0),
            fit_size(16, 16, 240.0)
        );
        // A doubled (transformed) canvas shows at exactly TWICE the
        // identity fit -- same pixels-per-texel, bigger canvas -- rather
        // than being squeezed into the same box at half scale.
        let (fw, fh) = fit_size(16, 16, 240.0);
        assert_eq!(
            preview_display_size(32, 32, 16, 16, 240.0),
            (fw * 2.0, fh * 2.0)
        );
    }
}
