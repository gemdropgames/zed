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

/// A rotation field's committed text -> the stored 256-step angle:
/// whole degrees (any sign, any number of turns), snapped to the
/// nearest of the hardware's 256 steps and wrapped onto the ring --
/// `round(deg * 256 / 360) mod 256`, in integer math. Junk (including
/// fractional degrees -- the display only ever shows whole ones) is
/// `None`, which the panel treats as "revert to the doc value", the
/// duration editor's rule.
pub fn parse_angle_deg(text: &str) -> Option<u8> {
    let degrees = text.trim().parse::<i64>().ok()?;
    let step = (degrees * 256 + 180).div_euclid(360).rem_euclid(256);
    Some(step as u8)
}

/// The display string for a stored angle step: the nearest whole degree
/// (`round(step * 360 / 256)`). Injective over all 256 steps (degrees
/// are finer than steps), so [`parse_angle_deg`] takes every displayed
/// value back to the exact step it came from.
pub fn format_angle_deg(step: u8) -> String {
    ((step as u32 * 360 + 128) / 256).to_string()
}

/// A scale/shear field's committed text -> signed 8.8 fixed point:
/// plain decimals (`"1.0"`, `"2.5"`, `"-0.25"`, `".5"`, `"3"`), rounded
/// to the nearest 1/256 and clamped into i16. Parsed with integer math
/// on the two decimal parts so binary fractions (halves, quarters, any
/// multiple of 1/256 written in <= 10 decimals) convert exactly. Junk
/// -> `None` (revert, like the duration editor).
pub fn parse_fixed88(text: &str) -> Option<i16> {
    let trimmed = text.trim();
    let (negative, unsigned) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let (int_part, frac_part) = match unsigned.split_once('.') {
        Some((int_part, frac_part)) => (int_part, frac_part),
        None => (unsigned, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    let mut magnitude = whole.checked_mul(256)?;
    let mut numerator: i64 = 0;
    let mut denominator: i64 = 1;
    for b in frac_part.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        // Ten digits already pin the value to well under 1/256; further
        // digits can't move the rounded result but could overflow i64.
        if denominator < 10_000_000_000 {
            numerator = numerator * 10 + (b - b'0') as i64;
            denominator *= 10;
        }
    }
    magnitude += (numerator * 256 + denominator / 2) / denominator;
    let value = if negative { -magnitude } else { magnitude };
    Some(value.clamp(i16::MIN as i64, i16::MAX as i64) as i16)
}

/// The display string for a stored 8.8 value: two decimal places,
/// nearest-hundredth. Coarser than the storage (100 hundredths vs 256
/// steps per unit), but every value a [`parse_fixed88`] commit stores
/// from two-decimal input round-trips exactly.
pub fn format_fixed88(value: i16) -> String {
    let magnitude = (value as i32).abs();
    let mut whole = magnitude / 256;
    let mut hundredths = ((magnitude % 256) * 100 + 128) / 256;
    if hundredths == 100 {
        whole += 1;
        hundredths = 0;
    }
    let sign = if value < 0 { "-" } else { "" };
    format!("{sign}{whole}.{hundredths:02}")
}

/// The FRAMES library's display list: one strip index per unique tile
/// map, first occurrence wins. Range clips duplicate frames physically
/// when a sequence reuses one; the library hides those copies so it
/// reads as "the sprite's unique frames" (durations are per-copy and
/// live in the clip editor, not here).
pub fn library_indices(frames: &[ggo_worldlib::sprites::cow::Frame]) -> Vec<usize> {
    let mut seen: Vec<&[u16]> = Vec::new();
    let mut out = Vec::new();
    for (ix, frame) in frames.iter().enumerate() {
        if seen.contains(&frame.map.as_slice()) {
            continue;
        }
        seen.push(&frame.map);
        out.push(ix);
    }
    out
}

#[cfg(test)]
mod tests {
    use ggo_worldlib::sprites::cow::{Frame, FrameTransform};

    fn frame(map: Vec<u16>) -> Frame {
        Frame {
            map,
            duration_ms: 100,
            transform: FrameTransform::IDENTITY,
        }
    }

    #[test]
    fn library_indices_keep_the_first_of_each_unique_map() {
        // Range clips duplicate frames physically; the LIBRARY shows one
        // entry per unique tile map, first occurrence wins.
        let frames = vec![
            frame(vec![0]),
            frame(vec![1]),
            frame(vec![0]),
            frame(vec![1]),
        ];
        assert_eq!(super::library_indices(&frames), vec![0, 1]);
    }

    #[test]
    fn library_indices_of_all_distinct_frames_is_identity() {
        let frames = vec![frame(vec![0]), frame(vec![1])];
        assert_eq!(super::library_indices(&frames), vec![0, 1]);
    }

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
    fn angle_and_fixed_parsers_round_trip_and_reject_junk() {
        assert_eq!(parse_angle_deg("90"), Some(64));
        assert_eq!(format_angle_deg(64), "90");
        assert_eq!(parse_fixed88("1.0"), Some(0x0100));
        assert_eq!(parse_fixed88("2.5"), Some(0x0280));
        assert_eq!(parse_fixed88("junk"), None);
    }

    #[test]
    fn parse_angle_deg_wraps_whole_turns_and_negatives_into_the_step_ring() {
        assert_eq!(parse_angle_deg("0"), Some(0));
        assert_eq!(parse_angle_deg("360"), Some(0));
        assert_eq!(parse_angle_deg("450"), Some(64));
        assert_eq!(parse_angle_deg("-90"), Some(192));
        assert_eq!(parse_angle_deg(" 180 "), Some(128));
        assert_eq!(parse_angle_deg("junk"), None);
        assert_eq!(parse_angle_deg(""), None);
        assert_eq!(parse_angle_deg("12.5"), None, "whole degrees only");
    }

    #[test]
    fn every_angle_step_survives_a_display_round_trip() {
        // format -> parse must land back on the same step for all 256:
        // the display rounds to whole degrees, but degrees are finer
        // than steps (360 > 256), so no two steps share a degree.
        for step in 0..=255u8 {
            assert_eq!(
                parse_angle_deg(&format_angle_deg(step)),
                Some(step),
                "step {step} (shown as {})",
                format_angle_deg(step)
            );
        }
    }

    #[test]
    fn parse_fixed88_is_exact_on_binary_fractions_and_clamps_to_i16() {
        assert_eq!(parse_fixed88("-0.25"), Some(-0x0040));
        assert_eq!(parse_fixed88(".5"), Some(0x0080));
        assert_eq!(parse_fixed88("-1"), Some(-0x0100));
        assert_eq!(parse_fixed88("0"), Some(0));
        assert_eq!(parse_fixed88("1000"), Some(i16::MAX), "clamped high");
        assert_eq!(parse_fixed88("-1000"), Some(i16::MIN), "clamped low");
        assert_eq!(parse_fixed88(""), None);
        assert_eq!(parse_fixed88("."), None);
        assert_eq!(parse_fixed88("--1"), None);
        assert_eq!(parse_fixed88("1.2.3"), None);
        assert_eq!(parse_fixed88("1e3"), None);
    }

    #[test]
    fn format_fixed88_shows_two_decimals_and_round_trips_editor_values() {
        assert_eq!(format_fixed88(0x0100), "1.00");
        assert_eq!(format_fixed88(0x0280), "2.50");
        assert_eq!(format_fixed88(-0x0040), "-0.25");
        assert_eq!(format_fixed88(0), "0.00");
        // Values the parsers themselves produce (whole hundredths, the
        // only shapes an editor commit stores) round-trip exactly.
        for text in ["1.00", "2.50", "-0.25", "0.75", "127.00"] {
            let v = parse_fixed88(text).unwrap();
            assert_eq!(format_fixed88(v), text, "{text}");
        }
    }

    #[test]
    fn active_clip_after_clip_delete_follows_ggo_ide_rules() {
        assert_eq!(active_clip_after_clip_delete(Some(1), 1), None);
        assert_eq!(active_clip_after_clip_delete(Some(2), 0), Some(1));
        assert_eq!(active_clip_after_clip_delete(Some(0), 1), Some(0));
        assert_eq!(active_clip_after_clip_delete(None, 0), None);
    }
}
