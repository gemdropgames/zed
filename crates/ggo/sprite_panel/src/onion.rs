//! Onion skin: the panel's toggle/back/forward/opacity control state, and
//! the ghost list it resolves into for the big preview.
//!
//! The FRAME SELECTION is not implemented here -- it is
//! `ggo_worldlib::sprites::timeline_ops::onion_frames`, which already owns
//! the "walk back/forward, clamp to the document or wrap inside a looping
//! clip" rule and is unit-tested there. This module only holds the control
//! state ggo-ide's `sprites/timeline.rs` holds (`onion_on`/`onion_back`/
//! `onion_fwd`/`onion_opacity`, same defaults and same clamps) and turns
//! each selected ghost's signed distance into the alpha it draws at.
//!
//! One deviation from ggo-ide, forced by the target toolkit:
//!
//! - **Opacity is a stepper, not a slider.** `ui` has no slider primitive
//!   (see its `components/` list -- toggle, dropdown, button, no range
//!   control). The `-`/`+` buttons move by ggo-ide's own slider step, over
//!   ggo-ide's own range, so the reachable values are identical.
//!
//! Red/blue tint (spec §4: red behind, blue ahead) IS implemented, just not
//! here: gpui has no image tint (`Img` exposes only `grayscale`, and
//! `paint_image`'s `PolychromeSprite` carries no colour field either), so
//! there is no element-level tint to apply the way ggo-ide's pixel surface
//! does. Instead [`loader::compose_ghost`] composes a tinted BGRA image per
//! ghost, per frame, using [`tint_for`]/[`tint_strength`] below for the
//! colour and blend amount. This module only owns those two pure
//! functions (plus each ghost's signed [`Ghost::dist`], which decides
//! direction) -- the pixel work lives in `loader` next to the rest of the
//! frame composition it reuses.

use ggo_worldlib::sprites::cow::ClipEdit;
use ggo_worldlib::sprites::timeline_ops::{self, OnionClip};

/// Back/forward ghost-count clamp -- ggo-ide `ONION_COUNT_MIN/MAX`.
pub const COUNT_MIN: u32 = 0;
pub const COUNT_MAX: u32 = 3;
/// Opacity range and step -- ggo-ide `ONION_OPACITY_MIN/MAX/STEP`.
pub const OPACITY_MIN: f32 = 0.0;
pub const OPACITY_MAX: f32 = 1.0;
pub const OPACITY_STEP: f32 = 0.05;
/// Freshly-opened defaults -- ggo-ide `DEFAULT_ONION_COUNT/OPACITY`.
pub const DEFAULT_COUNT: u32 = 1;
pub const DEFAULT_OPACITY: f32 = 0.5;

/// How much each extra step of distance dims a ghost:
/// `1 - (dist - 1) * FALLOFF_PER_STEP`, floored at 0 -- ggo-ide
/// `ONION_FALLOFF_PER_STEP`.
const FALLOFF_PER_STEP: f32 = 0.3;

/// Onion ghost tint colours -- ggo-ide `ONION_TINT_PREV`/`ONION_TINT_NEXT`
/// (spec §4: red behind, blue ahead), byte for byte.
pub(crate) const TINT_PREV: (u8, u8, u8) = (0xff, 0x44, 0x44);
pub(crate) const TINT_NEXT: (u8, u8, u8) = (0x44, 0x88, 0xff);

/// Which tint a ghost `dist` steps from the current frame takes -- past
/// (`dist < 0`) red, future (`dist > 0`) blue, matching
/// `timeline::State::ghost_layers`' `if g.dist < 0` branch. `dist == 0`
/// never reaches a ghost (it would be the current frame), so it falls to
/// the `else` arm same as ggo-ide's does; the branch is never taken in
/// practice.
pub(crate) fn tint_for(dist: i32) -> (u8, u8, u8) {
    if dist < 0 { TINT_PREV } else { TINT_NEXT }
}

/// How strongly a ghost `dist` steps away is blended toward its tint --
/// the same falloff shape `alpha_at` dims WITH (one full step at the
/// nearest ghost, `FALLOFF_PER_STEP` less per extra step, floored at 0),
/// scaled by [`DEFAULT_OPACITY`] rather than the live [`OnionState::opacity`].
///
/// ggo-ide multiplies this falloff by the user's live opacity control
/// (`ghost_layers`' `self.onion_opacity * (1 - ...)`), so its tint tracks
/// the opacity slider. Here the tint is baked into a `RenderImage` cached
/// by `(dist, frame idx)` alone (`loader::compose_ghost` /
/// `OpenSprite::ghost_cache`) -- keying that cache on the live opacity too
/// would mean rebuilding every ghost image on every opacity step, which is
/// exactly the per-render recompose the cache exists to avoid. Fixing the
/// multiplier at ggo-ide's own default (0.5) instead means the two match
/// exactly at ggo-ide's (and this port's) freshly-opened defaults, and the
/// live opacity control still governs each ghost's overall on-screen
/// strength via [`OnionState::alpha_at`]'s unchanged element-level
/// compositing -- tint direction/distance read consistently no matter
/// where the opacity stepper sits, only the ghost's overall visibility
/// dims with it, same division of labour as before this fast-follow.
pub(crate) fn tint_strength(dist: i32) -> f32 {
    let abs = dist.unsigned_abs() as f32;
    (DEFAULT_OPACITY * (1.0 - (abs - 1.0) * FALLOFF_PER_STEP)).clamp(0.0, 1.0)
}

/// One resolved ghost: which frame to draw, how far (signed -- negative
/// is behind/past, positive is ahead/future; see [`tint_for`]), and at
/// what alpha. Farthest first, so a nearer (brighter) ghost paints over a
/// farther one -- the same ordering ggo-ide's `ghost_layers` hands its
/// pixel surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ghost {
    pub idx: usize,
    pub dist: i32,
    pub alpha: f32,
}

/// The onion-skin control state -- ggo-ide `timeline::State`'s four
/// `onion_*` fields, split out so the panel's controls are testable
/// without a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnionState {
    pub on: bool,
    pub back: u32,
    pub fwd: u32,
    pub opacity: f32,
}

impl Default for OnionState {
    fn default() -> Self {
        Self {
            on: false,
            back: DEFAULT_COUNT,
            fwd: DEFAULT_COUNT,
            opacity: DEFAULT_OPACITY,
        }
    }
}

fn clamp_count(v: i64) -> u32 {
    v.clamp(i64::from(COUNT_MIN), i64::from(COUNT_MAX)) as u32
}

impl OnionState {
    pub fn toggle(&mut self) {
        self.on = !self.on;
    }

    /// Step the BACK count by `delta`, clamped to `[COUNT_MIN, COUNT_MAX]`
    /// -- ggo-ide `Msg::OnionBack` + `clamp_onion_count`.
    pub fn step_back(&mut self, delta: i32) {
        self.back = clamp_count(i64::from(self.back) + i64::from(delta));
    }

    /// Step the FORWARD count by `delta`, same clamp.
    pub fn step_fwd(&mut self, delta: i32) {
        self.fwd = clamp_count(i64::from(self.fwd) + i64::from(delta));
    }

    /// Step opacity by `steps` x [`OPACITY_STEP`], clamped to the range --
    /// the stepper standing in for ggo-ide's slider (module doc).
    pub fn step_opacity(&mut self, steps: i32) {
        let next = self.opacity + steps as f32 * OPACITY_STEP;
        // Re-quantize onto the step ladder so repeated float adds can't
        // drift the displayed percentage off a whole step.
        let quantized = (next / OPACITY_STEP).round() * OPACITY_STEP;
        self.opacity = quantized.clamp(OPACITY_MIN, OPACITY_MAX);
    }

    /// Whether the `-`/`+` for each control can still move (drives the
    /// buttons' `disabled`).
    pub fn can_step_back(&self, delta: i32) -> bool {
        clamp_count(i64::from(self.back) + i64::from(delta)) != self.back
    }

    pub fn can_step_fwd(&self, delta: i32) -> bool {
        clamp_count(i64::from(self.fwd) + i64::from(delta)) != self.fwd
    }

    pub fn can_step_opacity(&self, steps: i32) -> bool {
        let mut probe = *self;
        probe.step_opacity(steps);
        probe.opacity != self.opacity
    }

    /// The ghosts to draw under `frame`, farthest first. Empty when the
    /// toggle is off (and, naturally, when both counts are 0).
    ///
    /// `clip` is the ACTIVE clip, if any: onion walking is confined to it
    /// and wraps when it loops, exactly as `onion_frames` documents. Its
    /// `from`/`to` are normalized here because a `ClipEdit` may legally
    /// store a reversed range and `OnionClip` is specified as the already
    /// normalized pair.
    pub fn ghosts(&self, frame: usize, frame_count: usize, clip: Option<&ClipEdit>) -> Vec<Ghost> {
        if !self.on {
            return Vec::new();
        }
        let clip = clip.map(|c| OnionClip {
            from: c.from.min(c.to),
            to: c.from.max(c.to),
            loop_: c.loop_,
        });
        let mut ghosts = timeline_ops::onion_frames(frame, frame_count, self.back, self.fwd, clip);
        ghosts.sort_by_key(|g| std::cmp::Reverse(g.dist.unsigned_abs()));
        ghosts
            .into_iter()
            .map(|g| Ghost {
                idx: g.idx,
                dist: g.dist,
                alpha: self.alpha_at(g.dist),
            })
            .collect()
    }

    /// Alpha for a ghost `dist` steps from the current frame -- ggo-ide's
    /// `opacity * (1 - (|dist| - 1) * FALLOFF_PER_STEP)`, floored at 0.
    fn alpha_at(&self, dist: i32) -> f32 {
        let abs = dist.unsigned_abs() as f32;
        (self.opacity * (1.0 - (abs - 1.0) * FALLOFF_PER_STEP)).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::timeline_ops::onion_frames;

    fn clip(from: usize, to: usize, loop_: bool) -> ClipEdit {
        ClipEdit {
            name: "walk".to_string(),
            from,
            to,
            loop_,
        }
    }

    #[test]
    fn defaults_match_ggo_ide() {
        let s = OnionState::default();
        assert!(!s.on, "onion skin starts off");
        assert_eq!(s.back, DEFAULT_COUNT);
        assert_eq!(s.fwd, DEFAULT_COUNT);
        assert_eq!(s.opacity, DEFAULT_OPACITY);
    }

    #[test]
    fn toggle_flips_and_back_again() {
        let mut s = OnionState::default();
        s.toggle();
        assert!(s.on);
        s.toggle();
        assert!(!s.on);
    }

    #[test]
    fn counts_clamp_to_the_0_3_range() {
        let mut s = OnionState::default();
        for _ in 0..5 {
            s.step_back(-1);
            s.step_fwd(1);
        }
        assert_eq!(s.back, COUNT_MIN);
        assert_eq!(s.fwd, COUNT_MAX);
        assert!(!s.can_step_back(-1), "already at the floor");
        assert!(!s.can_step_fwd(1), "already at the ceiling");
        assert!(s.can_step_back(1));
        assert!(s.can_step_fwd(-1));
    }

    #[test]
    fn opacity_steps_by_the_slider_step_and_clamps() {
        let mut s = OnionState::default();
        s.step_opacity(1);
        assert!((s.opacity - (DEFAULT_OPACITY + OPACITY_STEP)).abs() < 1e-6);
        for _ in 0..100 {
            s.step_opacity(1);
        }
        assert_eq!(s.opacity, OPACITY_MAX);
        assert!(!s.can_step_opacity(1));
        for _ in 0..100 {
            s.step_opacity(-1);
        }
        assert_eq!(s.opacity, OPACITY_MIN);
        assert!(!s.can_step_opacity(-1));
        assert!(s.can_step_opacity(1));
    }

    #[test]
    fn ghosts_are_empty_while_the_toggle_is_off() {
        let s = OnionState {
            on: false,
            back: 3,
            fwd: 3,
            ..OnionState::default()
        };
        assert!(s.ghosts(2, 5, None).is_empty());
    }

    /// The selection itself is `onion_frames`' job -- this pins that the
    /// panel asks it the right question and reorders without dropping or
    /// inventing frames, for interior AND both edge positions.
    #[test]
    fn ghost_frames_match_onion_frames_for_every_position_in_the_strip() {
        let frame_count = 5;
        for back in 0..=COUNT_MAX {
            for fwd in 0..=COUNT_MAX {
                let s = OnionState {
                    on: true,
                    back,
                    fwd,
                    opacity: 1.0,
                };
                for frame in 0..frame_count {
                    let mut expected: Vec<usize> =
                        onion_frames(frame, frame_count, back, fwd, None)
                            .into_iter()
                            .map(|g| g.idx)
                            .collect();
                    expected.sort_unstable();
                    let mut got: Vec<usize> = s
                        .ghosts(frame, frame_count, None)
                        .into_iter()
                        .map(|g| g.idx)
                        .collect();
                    got.sort_unstable();
                    assert_eq!(got, expected, "frame {frame}, back {back}, fwd {fwd}");
                }
            }
        }
    }

    #[test]
    fn the_first_frame_has_no_ghosts_behind_it_and_the_last_none_ahead() {
        let s = OnionState {
            on: true,
            back: 2,
            fwd: 2,
            opacity: 1.0,
        };
        // Frame 0: only the two frames ahead.
        assert_eq!(
            s.ghosts(0, 5, None)
                .iter()
                .map(|g| g.idx)
                .collect::<Vec<_>>(),
            vec![2, 1],
            "farthest first"
        );
        // Last frame: only the two behind.
        assert_eq!(
            s.ghosts(4, 5, None)
                .iter()
                .map(|g| g.idx)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        // A one-frame document has no neighbours at all.
        assert!(s.ghosts(0, 1, None).is_empty());
    }

    #[test]
    fn ghosts_are_ordered_farthest_first_so_nearer_ones_paint_on_top() {
        let s = OnionState {
            on: true,
            back: 2,
            fwd: 2,
            opacity: 1.0,
        };
        let ghosts = s.ghosts(3, 7, None);
        let alphas: Vec<f32> = ghosts.iter().map(|g| g.alpha).collect();
        assert_eq!(
            ghosts.iter().map(|g| g.idx).collect::<Vec<_>>(),
            vec![1, 5, 2, 4]
        );
        // Distance 2 first (dimmer), then distance 1.
        assert!(alphas[0] < alphas[2], "farther ghosts are dimmer");
        assert!(
            (alphas[2] - 1.0).abs() < 1e-6,
            "an adjacent ghost is at full opacity"
        );
        assert!((alphas[0] - 0.7).abs() < 1e-6, "one step of falloff");
    }

    #[test]
    fn alpha_scales_with_the_opacity_control_and_floors_at_zero() {
        let s = OnionState {
            on: true,
            back: 3,
            fwd: 0,
            opacity: 0.5,
        };
        let ghosts = s.ghosts(3, 5, None);
        // dist 3 -> 0.5 * (1 - 0.6) = 0.2, dist 2 -> 0.35, dist 1 -> 0.5.
        let alphas: Vec<f32> = ghosts.iter().map(|g| g.alpha).collect();
        assert!((alphas[0] - 0.2).abs() < 1e-6);
        assert!((alphas[1] - 0.35).abs() < 1e-6);
        assert!((alphas[2] - 0.5).abs() < 1e-6);
        // Nothing can go negative even at max distance and min opacity.
        let dark = OnionState { opacity: 0.0, ..s };
        assert!(dark.ghosts(3, 5, None).iter().all(|g| g.alpha >= 0.0));
    }

    /// The active clip confines the walk, and a looping clip wraps -- the
    /// panel must pass the clip through, not ignore it.
    #[test]
    fn an_active_clip_confines_the_ghosts_and_a_looping_one_wraps() {
        let s = OnionState {
            on: true,
            back: 1,
            fwd: 1,
            opacity: 1.0,
        };
        // Non-looping clip [1, 3], sitting on its last frame: nothing ahead.
        let ghosts = s.ghosts(3, 6, Some(&clip(1, 3, false)));
        assert_eq!(ghosts.iter().map(|g| g.idx).collect::<Vec<_>>(), vec![2]);
        // Looping clip, same position: forward wraps to the clip's start.
        let ghosts = s.ghosts(3, 6, Some(&clip(1, 3, true)));
        let idxs: Vec<usize> = ghosts.iter().map(|g| g.idx).collect();
        assert!(idxs.contains(&2) && idxs.contains(&1), "wrapped: {idxs:?}");
    }

    // ------------------------------------------------------------ tint

    #[test]
    fn tint_for_is_red_behind_and_blue_ahead() {
        assert_eq!(tint_for(-3), TINT_PREV);
        assert_eq!(tint_for(-1), TINT_PREV);
        assert_eq!(tint_for(1), TINT_NEXT);
        assert_eq!(tint_for(3), TINT_NEXT);
    }

    #[test]
    fn tint_strength_matches_ggo_ides_default_opacity_at_the_nearest_ghost_and_falls_off() {
        // dist 1 -> DEFAULT_OPACITY * 1.0 == ggo-ide's own default-opacity
        // tint strength at the nearest ghost.
        assert!((tint_strength(1) - DEFAULT_OPACITY).abs() < 1e-6);
        assert!(
            (tint_strength(-1) - DEFAULT_OPACITY).abs() < 1e-6,
            "sign doesn't affect strength"
        );
        // dist 2 -> one falloff step dimmer, same shape as alpha_at.
        assert!((tint_strength(2) - DEFAULT_OPACITY * 0.7).abs() < 1e-6);
        // Never negative even past where the raw falloff would go negative.
        assert!(tint_strength(10) >= 0.0);
    }

    /// A clip may legally store its range reversed; the ghosts must be the
    /// same either way (`OnionClip` is specified as the normalized pair).
    #[test]
    fn a_reversed_clip_range_resolves_the_same_as_the_forward_one() {
        let s = OnionState {
            on: true,
            back: 1,
            fwd: 1,
            opacity: 1.0,
        };
        assert_eq!(
            s.ghosts(2, 6, Some(&clip(1, 3, false))),
            s.ghosts(2, 6, Some(&clip(3, 1, false)))
        );
    }
}
