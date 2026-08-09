//! Pure animation-edit helpers: new-clip defaults, clip-range validation,
//! duration parsing, and the post-op selection bookkeeping rules -- the
//! framework-free half of the M5 editing wiring, mirrored from ggo-ide's
//! `sprites/timeline.rs` message handlers (`AddClip`, `DeleteClip`,
//! `DeleteFrame`, `DurationSubmit`) so the panel's gpui layer stays thin
//! and every rule here is directly unit-testable.

use ggo_worldlib::sprites::cow::ClipEdit;
use ggo_worldlib::sprites::timeline_ops::{MIN_FRAME_MS, clip_ranges_valid};

/// A freshly-added clip: ggo-ide `Msg::AddClip`'s defaults verbatim --
/// incrementing `clip{N}` name, a single-frame range on the currently
/// selected frame (clamped into the strip), not looping.
pub fn default_new_clip(clip_count: usize, selected_frame: usize, frame_count: usize) -> ClipEdit {
    let f = selected_frame.min(frame_count.saturating_sub(1));
    ClipEdit {
        name: format!("clip{}", clip_count + 1),
        from: f,
        to: f,
        loop_: false,
    }
}

/// Validate a clip-range edit before it becomes a `ClipSet`: both
/// endpoints must address a real frame (`timeline_ops::clip_ranges_valid`
/// on the candidate) and `from <= to` (the brief's rule for TYPED range
/// edits -- stricter than storage, where a reversed pair is legal and
/// playback normalizes it; ggo-ide's sliders clamp instead and so can
/// never produce either failure). `Some(message)` is the inline error to
/// show; the op must not be applied.
pub fn clip_range_error(from: usize, to: usize, frame_count: usize) -> Option<String> {
    let candidate = ClipEdit {
        name: String::new(),
        from,
        to,
        loop_: false,
    };
    if !clip_ranges_valid(std::slice::from_ref(&candidate), frame_count) {
        return Some(format!(
            "range {from}-{to} is outside frames 0-{}",
            frame_count.saturating_sub(1)
        ));
    }
    if from > to {
        return Some(format!("from {from} > to {to}"));
    }
    None
}

/// A duration field's committed text -> the ms to store: ggo-ide
/// `Msg::DurationSubmit` verbatim (unparsable input reads as 0, then
/// everything floors to [`MIN_FRAME_MS`] -- the same floor
/// `playback_frame_at` applies at play time, so what's stored is what
/// plays).
pub fn parse_duration_ms(text: &str) -> u16 {
    text.trim().parse::<u16>().unwrap_or(0).max(MIN_FRAME_MS)
}

/// Where the strip selection lands after deleting frame `deleted` from a
/// strip of `old_len` (>= 2 -- the last frame can't be deleted): ggo-ide
/// `Msg::DeleteFrame`'s rule -- a selection past the deleted frame slides
/// down with its frame, anything else stays put, then clamp into the
/// shrunk strip.
pub fn selection_after_frame_delete(selected: usize, deleted: usize, old_len: usize) -> usize {
    let next = if selected > deleted {
        selected - 1
    } else {
        selected
    };
    next.min(old_len.saturating_sub(2))
}

/// Where the active-clip selection lands after deleting clip `deleted`:
/// ggo-ide `Msg::DeleteClip`'s rule -- deleting the active clip clears the
/// selection (back to whole-strip), a later selection shifts down with its
/// clip, an earlier one is untouched.
pub fn active_clip_after_clip_delete(active: Option<usize>, deleted: usize) -> Option<usize> {
    match active {
        Some(a) if a == deleted => None,
        Some(a) if a > deleted => Some(a - 1),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_new_clip_uses_incrementing_name_and_the_selected_frame_as_a_point_range() {
        assert_eq!(
            default_new_clip(0, 2, 4),
            ClipEdit {
                name: "clip1".into(),
                from: 2,
                to: 2,
                loop_: false
            }
        );
        assert_eq!(default_new_clip(2, 0, 4).name, "clip3");
    }

    #[test]
    fn default_new_clip_clamps_a_stale_selection_into_the_strip() {
        let c = default_new_clip(0, 9, 3);
        assert_eq!((c.from, c.to), (2, 2));
    }

    #[test]
    fn clip_range_error_accepts_in_range_forward_ranges() {
        assert_eq!(clip_range_error(0, 2, 3), None);
        assert_eq!(clip_range_error(2, 2, 3), None);
    }

    #[test]
    fn clip_range_error_rejects_out_of_range_endpoints() {
        let e = clip_range_error(0, 3, 3).unwrap();
        assert!(e.contains("0-3"), "message names the bad range: {e}");
        assert!(clip_range_error(3, 3, 3).is_some());
        assert!(clip_range_error(0, 0, 0).is_some(), "no frames, no ranges");
    }

    #[test]
    fn clip_range_error_rejects_a_reversed_range() {
        let e = clip_range_error(2, 1, 3).unwrap();
        assert!(e.contains('>'), "message shows the inversion: {e}");
    }

    #[test]
    fn parse_duration_ms_parses_floors_and_defaults_unparsable_to_the_floor() {
        assert_eq!(parse_duration_ms("250"), 250);
        assert_eq!(parse_duration_ms(" 40 "), 40);
        assert_eq!(parse_duration_ms("1"), MIN_FRAME_MS);
        assert_eq!(parse_duration_ms("abc"), MIN_FRAME_MS);
        assert_eq!(parse_duration_ms(""), MIN_FRAME_MS);
        assert_eq!(parse_duration_ms("99999"), MIN_FRAME_MS); // > u16::MAX -> unparsable -> floor
    }

    #[test]
    fn selection_after_frame_delete_follows_ggo_ide_rules() {
        assert_eq!(selection_after_frame_delete(2, 1, 4), 1); // past the delete: slides down
        assert_eq!(selection_after_frame_delete(1, 1, 4), 1); // at the delete: next frame slides in
        assert_eq!(selection_after_frame_delete(0, 2, 4), 0); // before the delete: untouched
        assert_eq!(selection_after_frame_delete(3, 3, 4), 2); // deleted the last frame: clamp to new end
    }

    #[test]
    fn active_clip_after_clip_delete_follows_ggo_ide_rules() {
        assert_eq!(active_clip_after_clip_delete(Some(1), 1), None);
        assert_eq!(active_clip_after_clip_delete(Some(2), 0), Some(1));
        assert_eq!(active_clip_after_clip_delete(Some(0), 1), Some(0));
        assert_eq!(active_clip_after_clip_delete(None, 0), None);
    }
}
