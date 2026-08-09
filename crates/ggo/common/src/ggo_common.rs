//! Shared helpers for GGO fork panels: the RGBA->BGRA `RenderImage`
//! bridge (gpui's `RenderImage` frames are BGRA, see `gpui/src/assets.rs`'s
//! "A cached and processed image, in BGRA format"; worldlib composes
//! straight-alpha RGBA, so only a channel swap is needed, no alpha
//! unpremultiply), and the unsaved-document close guard shared by the
//! world and metasprite panels. Deliberately depends on only `gpui` and
//! `image` -- no worldlib and no `workspace` dependency (the latter would
//! be a cycle: `workspace` is what calls the guard) -- so any GGO panel
//! can use it without pulling in world-doc types.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, PromptLevel, RenderImage, Task, Window};
use image::Frame;

/// In-place RGBA8 -> BGRA8 (straight alpha in, straight alpha out --
/// gpui's own non-SVG decode paths do exactly this `swap(0, 2)`, see
/// `gpui/src/elements/img.rs`'s WebP branch; the SVG path's extra alpha
/// divide is for tiny-skia's PREMULTIPLIED output, which worldlib's
/// composes are not).
pub fn rgba_to_bgra(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// Build the one gpui-side image for a composed `w`x`h` RGBA8 buffer.
/// Intended to be called once per source image at load time, never per
/// frame.
pub fn to_render_image(rgba: &[u8], w: u32, h: u32) -> Option<Arc<RenderImage>> {
    let mut data = rgba.to_vec();
    rgba_to_bgra(&mut data);
    let buffer = image::ImageBuffer::from_raw(w, h, data)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

// --------------------------------------------------------- shared db path

/// Database filename under `~/.ggo/`, matching `ggo-ide`'s
/// `backend/db.rs::DB_FILE`.
const DB_FILE: &str = "ggo_ide.db";
const DOT_GGO: &str = ".ggo";

/// `~/.ggo/ggo_ide.db`, matching `ggo-ide`'s `backend/db.rs::default_db_path`.
/// `None` only if neither `HOME` nor `USERPROFILE` resolves (mirrors that
/// function's `anyhow` error, downgraded to `Option` here since neither
/// caller treats an unresolvable home directory as a hard error).
///
/// Shared by `ggo_charts_panel::loader` (reads runs for the picker) and
/// `ggo_emu_panel::ingest` (writes a finished run) -- both touch the SAME
/// file, not copies, so a run the emu pane ingests shows up in the charts
/// panel's picker with no configuration. Kept in one place because the two
/// crates diverging here would be a silent split-brain: the round-trip test
/// that exercises both sides passes a `db_path_override`, so it would never
/// catch drift in this default.
pub fn default_db_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(DOT_GGO).join(DB_FILE))
}

// ------------------------------------------------- unsaved-document guard

/// What the user picked in the unsaved-document prompt raised by
/// [`prepare_to_close_dirty`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseChoice {
    /// Write the document, then close if the write succeeded.
    Save,
    /// Close without writing.
    Discard,
    /// Abort the close.
    Cancel,
}

/// Map a `Window::prompt` answer index onto a [`CloseChoice`], for the
/// `["Save", "Don't Save", "Cancel"]` button set `workspace`'s own dirty-item
/// prompt uses (`workspace/src/pane.rs`, `Pane::save_item`). Anything that
/// is not an explicit Save/Don't-Save -- Cancel, a dismissed dialog, a
/// dropped channel -- is Cancel, which is the safe answer: it keeps the
/// window (and the unsaved doc) alive. `pane.rs`'s `_ => return Ok(false)`
/// arm makes the same call.
pub fn close_choice(answer: Option<usize>) -> CloseChoice {
    match answer {
        Some(0) => CloseChoice::Save,
        Some(1) => CloseChoice::Discard,
        _ => CloseChoice::Cancel,
    }
}

/// The prompt text for a dirty document named `name`, worded like
/// `workspace/src/pane.rs`'s `dirty_message_for`.
pub fn dirty_message(name: &str) -> String {
    format!("{name} contains unsaved edits. Do you want to save it?")
}

/// The body of a GGO panel's `Panel::prepare_to_close`.
///
/// `dirty_name` is `Some(display name)` when the panel holds an unsaved
/// document and `None` when it has nothing to lose -- a clean panel never
/// prompts and never blocks. Otherwise this raises the same
/// Save/Don't-Save/Cancel warning `Pane::save_item` raises for a dirty
/// buffer, and resolves to `false` (cancel the close) on Cancel *and* on a
/// failed save, so a write error can't silently discard the document.
///
/// `save` runs on the panel and returns whether the write succeeded.
pub fn prepare_to_close_dirty<T: 'static>(
    dirty_name: Option<String>,
    window: &mut Window,
    cx: &mut Context<T>,
    save: impl FnOnce(&mut T, &mut Context<T>) -> bool + 'static,
) -> Task<bool> {
    let Some(name) = dirty_name else {
        return Task::ready(true);
    };
    let answer = window.prompt(
        PromptLevel::Warning,
        &dirty_message(&name),
        None,
        &["Save", "Don't Save", "Cancel"],
        cx,
    );
    cx.spawn(
        async move |this, cx| match close_choice(answer.await.ok()) {
            CloseChoice::Save => this.update(cx, save).unwrap_or(false),
            CloseChoice::Discard => true,
            CloseChoice::Cancel => false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic red/blue swap bug: RGBA in, BGRA out, alpha untouched.
    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_only() {
        let mut data = vec![10, 20, 30, 40, 1, 2, 3, 4];
        rgba_to_bgra(&mut data);
        assert_eq!(data, vec![30, 20, 10, 40, 3, 2, 1, 4]);
    }

    #[test]
    fn to_render_image_produces_one_frame_of_the_right_size() {
        let rgba = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let rendered = to_render_image(&rgba, 2, 1).unwrap();
        assert_eq!(rendered.frame_count(), 1);
        // Red pixel first: BGRA bytes [0, 0, 255, 255].
        assert_eq!(
            rendered.as_bytes(0).unwrap(),
            &[0, 0, 255, 255, 255, 0, 0, 255]
        );
    }

    /// The prompt's button order is `["Save", "Don't Save", "Cancel"]`, and
    /// every non-answer (dismissed dialog, dropped channel, an index the
    /// button list doesn't have) must fall back to Cancel -- the only
    /// choice that can't lose the document.
    #[test]
    fn close_choice_maps_button_indices_and_fails_safe() {
        assert_eq!(close_choice(Some(0)), CloseChoice::Save);
        assert_eq!(close_choice(Some(1)), CloseChoice::Discard);
        assert_eq!(close_choice(Some(2)), CloseChoice::Cancel);
        assert_eq!(close_choice(Some(99)), CloseChoice::Cancel);
        assert_eq!(close_choice(None), CloseChoice::Cancel);
    }

    /// `default_db_path` must land on `~/.ggo/ggo_ide.db` -- the file both
    /// `ggo_charts_panel` and `ggo_emu_panel` read/write. HOME reliably
    /// resolves in the test environment.
    #[test]
    fn default_db_path_is_dot_ggo_ggo_ide_db() {
        let path = default_db_path().expect("HOME resolves in the test env");
        assert!(path.ends_with(".ggo/ggo_ide.db"));
    }

    #[test]
    fn dirty_message_names_the_document() {
        assert_eq!(
            dirty_message("worlds/test.toml"),
            "worlds/test.toml contains unsaved edits. Do you want to save it?"
        );
    }
}
