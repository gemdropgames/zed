//! Guarded-positional animation preservation for a sprite re-import.
//!
//! An artist-supplied PNG is the source of truth for PIXELS, but the
//! animation work -- clip ranges, per-frame durations, per-frame
//! transforms -- only exists in the `.spr`, and a plain re-import writes
//! a fresh [`SpriteState`] that has none of it (worldlib's
//! `import::sprite_import` builds every frame at
//! `DEFAULT_FRAME_DURATION_MS` with an identity transform and no clips).
//!
//! A PNG frame carries no identity, so the ONLY defensible match between
//! the old document's frames and the new import's is POSITION: frame `i`
//! is still frame `i`. That holds exactly while the artist appends --
//! which is the workflow this supports -- and silently lies as soon as
//! frames are reordered or removed. So the match is guarded rather than
//! guessed: [`check`] refuses outright whenever position can no longer be
//! trusted (a shrunk frame list, a changed footprint) or when the write
//! would reach further than this sprite (a shared tileset), and the
//! caller confirms with the user before [`merge`] carries anything over.
//! Frame REMAPPING (an explicit old-frame -> new-frame table for a
//! reordered sheet) is a strictly additive future step: it can only widen
//! what [`check`] admits, so nothing here has to change to allow it.
//!
//! Frame NAMES need no work here at all -- they live in the sprite
//! panel's `.ggo-ide/<rel>.editor.json` sidecar, which an import never
//! writes; they are covered by this module's tests only to pin that.

use std::path::Path;

use ggo_worldlib::sprites::cow::SpriteState;
use ggo_worldlib::sprites::io;

/// Why an existing sprite's animations cannot be carried onto a new
/// import. Every variant is a REFUSAL: the import writes nothing, rather
/// than preserving something that would be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mismatch {
    /// The new cut has a different tile footprint, so the old frames'
    /// tile grids describe a different sprite entirely.
    Footprint { old: (u8, u8), new: (u8, u8) },
    /// The new import has fewer frames than the document, so some clip
    /// could only be preserved by truncating it.
    FewerFrames { old: usize, new: usize },
    /// The `.spr`'s `.til` is bound by other sprites too -- rewriting it
    /// from this PNG would rewrite THEIR tiles as well.
    SharedTileset,
}

/// May `new`'s artwork replace `old`'s while keeping `old`'s animations?
pub(crate) fn check(old: &SpriteState, new: &SpriteState) -> Result<(), Mismatch> {
    if old.pool_shared {
        return Err(Mismatch::SharedTileset);
    }
    let old_footprint = (old.w_tiles, old.h_tiles);
    let new_footprint = (new.w_tiles, new.h_tiles);
    if old_footprint != new_footprint {
        return Err(Mismatch::Footprint {
            old: old_footprint,
            new: new_footprint,
        });
    }
    if new.frames.len() < old.frames.len() {
        return Err(Mismatch::FewerFrames {
            old: old.frames.len(),
            new: new.frames.len(),
        });
    }
    Ok(())
}

/// Carry `old`'s animation work onto `new`'s artwork, matching frames by
/// position: frame `i` keeps its duration and transform, frames past the
/// old document's end keep the import's defaults, and the clips come over
/// whole.
///
/// Only meaningful after [`check`] has passed, but written not to depend
/// on it: a clip whose range does not address `new`'s frames is DROPPED
/// rather than clamped or trusted, so a caller that skips the check can
/// still not produce a document that addresses a frame it doesn't have.
pub(crate) fn merge(old: &SpriteState, mut new: SpriteState) -> SpriteState {
    for (frame, previous) in new.frames.iter_mut().zip(old.frames.iter()) {
        frame.duration_ms = previous.duration_ms;
        frame.transform = previous.transform;
    }
    let frame_count = new.frames.len();
    new.clips = old
        .clips
        .iter()
        .filter(|clip| clip.from < frame_count && clip.to < frame_count)
        .cloned()
        .collect();
    new
}

/// Every `.spr` under `root` whose tileset resolves to `til_rel`.
///
/// Deliberately NOT `io::scan_til_sharers`, which gates on two or more
/// referrers because it answers a different question -- "is this pool
/// SHARED", i.e. must the sprite editor treat it as append-only. A plain
/// tileset import rewrites the tiles by index for every sprite bound to
/// it, and the very first binder is already one whose artwork silently
/// rearranges, so the gate would hide exactly the common case.
pub(crate) fn sprites_bound_to(root: &Path, til_rel: &str) -> Vec<String> {
    io::list_sprites(root)
        .into_iter()
        .filter(|spr_rel| {
            io::open_sprite(root, spr_rel).is_ok_and(|opened| opened.til_path == til_rel)
        })
        .collect()
}

/// The cascade line naming what a tileset overwrite reaches past itself:
/// the sprites whose frames address that `.til` BY INDEX. Empty when
/// nothing is bound to it.
pub(crate) fn bound_cascade(bound: &[String]) -> Vec<String> {
    if bound.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "{} bound to this tileset and address it by tile index: {}",
        if bound.len() == 1 {
            "1 sprite is".to_string()
        } else {
            format!("{} sprites are", bound.len())
        },
        bound.join(", ")
    )]
}

/// What a [`Mismatch`] means for `spr_rel`, and what the user can do
/// about it -- the status line of a refused re-import.
pub(crate) fn mismatch_message(mismatch: &Mismatch, spr_rel: &str) -> String {
    let reason = match mismatch {
        Mismatch::Footprint { old, new } => format!(
            "it is {}x{} tiles but the new import is {}x{}",
            old.0, old.1, new.0, new.1
        ),
        Mismatch::FewerFrames { old, new } => format!(
            "it has {old} frames but the new import has only {new}"
        ),
        Mismatch::SharedTileset => {
            "its tileset is shared with other sprites, which this import would rewrite".to_string()
        }
    };
    format!(
        "cannot keep {spr_rel}'s animations: {reason}. \
         They are kept only when existing frames stay in place and new ones are appended."
    )
}

/// The confirm shown before an existing sprite's artwork is replaced --
/// states plainly what survives and on what assumption. `frames` is the
/// EXISTING document's frame count: those are the frames the positional
/// match assumes still line up, and the ones the user can check.
pub(crate) fn preserve_message(spr_rel: &str, clips: usize, frames: usize) -> String {
    let kept = match clips {
        0 => "frame timing".to_string(),
        1 => "1 clip and frame timing".to_string(),
        n => format!("{n} clips and frame timing"),
    };
    format!(
        "{spr_rel} already exists — replace its artwork and keep its {kept}? \
         Existing frames are matched by position, so this assumes the first {frames} frames \
         of the image still line up."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::cow::{ClipEdit, Frame, FrameTransform};
    use ggo_worldlib::sprites::hw::TILE_BYTES;

    fn frame(duration_ms: u16) -> Frame {
        Frame {
            map: vec![0],
            duration_ms,
            transform: FrameTransform::IDENTITY,
        }
    }

    fn state(frames: usize, footprint: (u8, u8)) -> SpriteState {
        SpriteState {
            pool: vec![0; TILE_BYTES],
            tile_count: 1,
            session_tiles: Default::default(),
            palette: [0; 16],
            frames: (0..frames).map(|_| frame(100)).collect(),
            clips: Vec::new(),
            w_tiles: footprint.0,
            h_tiles: footprint.1,
            pool_shared: false,
        }
    }

    fn clip(name: &str, from: usize, to: usize) -> ClipEdit {
        ClipEdit {
            name: name.to_string(),
            from,
            to,
            loop_: true,
        }
    }

    /// The workflow this exists for: the artist appended frames, so every
    /// old frame is still where it was. Timing, transforms and clips come
    /// over untouched; the appended frames keep the import's defaults.
    #[test]
    fn appending_frames_keeps_every_clip_timing_and_transform() {
        let mut old = state(2, (1, 1));
        old.frames[0].duration_ms = 250;
        old.frames[1].duration_ms = 400;
        old.frames[1].transform = FrameTransform {
            angle256: 64,
            ..FrameTransform::IDENTITY
        };
        old.clips = vec![clip("idle", 0, 1)];
        let new = state(4, (1, 1));

        assert_eq!(check(&old, &new), Ok(()));
        let merged = merge(&old, new);
        assert_eq!(
            merged.frames.iter().map(|f| f.duration_ms).collect::<Vec<_>>(),
            vec![250, 400, 100, 100],
            "old frames keep their timing, appended ones take the import default"
        );
        assert_eq!(
            merged.frames[1].transform,
            FrameTransform {
                angle256: 64,
                ..FrameTransform::IDENTITY
            }
        );
        assert_eq!(merged.frames[2].transform, FrameTransform::IDENTITY);
        assert_eq!(merged.clips, vec![clip("idle", 0, 1)]);
    }

    /// The artwork is the import's, not the document's -- preservation is
    /// animation-only.
    #[test]
    fn the_merged_document_keeps_the_new_artwork() {
        let mut old = state(1, (1, 1));
        old.pool = vec![0xab; TILE_BYTES];
        old.palette = [1; 16];
        let mut new = state(1, (1, 1));
        new.pool = vec![0xcd; TILE_BYTES];
        new.palette = [2; 16];

        let merged = merge(&old, new);
        assert_eq!(merged.pool, vec![0xcd; TILE_BYTES]);
        assert_eq!(merged.palette, [2; 16]);
    }

    #[test]
    fn a_shrunk_frame_list_or_changed_footprint_is_refused() {
        let old = state(3, (2, 1));
        assert_eq!(
            check(&old, &state(2, (2, 1))),
            Err(Mismatch::FewerFrames { old: 3, new: 2 })
        );
        assert_eq!(
            check(&old, &state(3, (1, 1))),
            Err(Mismatch::Footprint {
                old: (2, 1),
                new: (1, 1)
            })
        );
        assert_eq!(check(&old, &state(3, (2, 1))), Ok(()), "equal counts are fine");
    }

    /// A shared `.til` makes the write reach past this sprite, so it is
    /// refused before the footprint/frame questions are even asked.
    #[test]
    fn a_shared_tileset_is_refused_outright() {
        let mut old = state(1, (1, 1));
        old.pool_shared = true;
        assert_eq!(check(&old, &state(1, (1, 1))), Err(Mismatch::SharedTileset));
    }

    /// `merge` must never hand back a document whose clips address a
    /// frame it does not have, even when called without `check`.
    #[test]
    fn merge_drops_a_clip_that_the_new_frame_list_cannot_hold() {
        let mut old = state(3, (1, 1));
        old.clips = vec![clip("keep", 0, 1), clip("drop", 1, 2)];
        let merged = merge(&old, state(2, (1, 1)));
        assert_eq!(merged.clips, vec![clip("keep", 0, 1)]);
    }

    /// The gate is ONE binder, not two: `io::scan_til_sharers` answers
    /// "is this pool shared" and returns nothing below two referrers, so
    /// using it here would stay silent in the ordinary case of a single
    /// sprite built on the tileset being re-imported.
    #[test]
    fn a_single_bound_sprite_is_already_a_cascade() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(1, (1, 1));
        ggo_worldlib::sprites::io::save_sprite(
            dir.path(),
            "art/hero.spr",
            &state,
            "art/hero.til",
            "art/hero.pal",
        )
        .expect("writing the fixture trio");

        let bound = sprites_bound_to(dir.path(), "art/hero.til");
        assert_eq!(bound, vec!["art/hero.spr".to_string()]);
        assert_eq!(
            ggo_worldlib::sprites::io::scan_til_sharers(dir.path(), "art/hero.til"),
            Vec::<String>::new(),
            "the sharer scan's own two-referrer gate is what this must not inherit"
        );
        let cascade = bound_cascade(&bound);
        assert_eq!(cascade.len(), 1);
        assert!(cascade[0].contains("1 sprite is bound"), "{}", cascade[0]);
        assert!(cascade[0].contains("art/hero.spr"), "{}", cascade[0]);

        assert!(
            sprites_bound_to(dir.path(), "art/other.til").is_empty(),
            "an unbound tileset has no cascade"
        );
        assert!(bound_cascade(&[]).is_empty());
    }

    #[test]
    fn the_messages_name_the_sprite_and_the_reason() {
        let message = mismatch_message(&Mismatch::FewerFrames { old: 4, new: 2 }, "art/hero.spr");
        assert!(message.starts_with("cannot keep art/hero.spr's animations: "));
        assert!(message.contains("4 frames but the new import has only 2"));
        let confirm = preserve_message("art/hero.spr", 1, 3);
        assert!(confirm.contains("keep its 1 clip and frame timing"));
        assert!(
            confirm.contains("the first 3 frames"),
            "the confirm names what the positional match assumes"
        );
        assert!(preserve_message("art/hero.spr", 2, 3).contains("keep its 2 clips and frame timing"));
        assert!(
            preserve_message("art/hero.spr", 0, 3).contains("keep its frame timing"),
            "a sprite with no clips still has timing worth keeping"
        );
    }
}
