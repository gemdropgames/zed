use gpui::{App, actions};
use workspace::Workspace;

// ZedGG
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{EntityId, Global, ImageSource};

/// ZedGG: an extra image resolver for one buffer's preview, consulted
/// before the file-system-relative resolution. Lets a buffer that has no
/// file on disk (ZedGG's design docs live in `zedgg.sqlite`) still show
/// `![](images/x.png)`.
pub type BufferImageResolver = Arc<dyn Fn(&str) -> Option<ImageSource> + Send + Sync>;

#[derive(Default)]
struct BufferImageResolvers(HashMap<EntityId, BufferImageResolver>);

impl Global for BufferImageResolvers {}

/// ZedGG: register `resolver` for the singleton `Buffer` with id `buffer`.
pub fn set_buffer_image_resolver(cx: &mut App, buffer: EntityId, resolver: BufferImageResolver) {
    cx.default_global::<BufferImageResolvers>()
        .0
        .insert(buffer, resolver);
}

/// ZedGG: forget the resolver registered for `buffer`.
pub fn remove_buffer_image_resolver(cx: &mut App, buffer: EntityId) {
    cx.default_global::<BufferImageResolvers>()
        .0
        .remove(&buffer);
}

/// ZedGG: the resolver registered for `buffer`, if any.
pub fn buffer_image_resolver(cx: &App, buffer: EntityId) -> Option<BufferImageResolver> {
    cx.try_global::<BufferImageResolvers>()
        .and_then(|resolvers| resolvers.0.get(&buffer).cloned())
}
// ZedGG

pub mod markdown_preview_settings;
pub mod markdown_preview_view;

pub use zed_actions::preview::markdown::{OpenPreview, OpenPreviewToTheSide};

use crate::markdown_preview_view::MarkdownPreviewView;

actions!(
    markdown,
    [
        /// Scrolls up by one page in the markdown preview.
        #[action(deprecated_aliases = ["markdown::MovePageUp"])]
        ScrollPageUp,
        /// Scrolls down by one page in the markdown preview.
        #[action(deprecated_aliases = ["markdown::MovePageDown"])]
        ScrollPageDown,
        /// Scrolls up by approximately one visual line.
        ScrollUp,
        /// Scrolls down by approximately one visual line.
        ScrollDown,
        /// Scrolls up by one markdown element in the markdown preview
        ScrollUpByItem,
        /// Scrolls down by one markdown element in the markdown preview
        ScrollDownByItem,
        /// Scrolls to the top of the markdown preview.
        ScrollToTop,
        /// Scrolls to the bottom of the markdown preview.
        ScrollToBottom,
        /// Opens a following markdown preview that syncs with the editor.
        OpenFollowingPreview,
        /// Closes the markdown preview and returns focus to the source editor.
        CloseAndReturnToEditor
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<MarkdownPreviewView>(cx);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        markdown_preview_view::MarkdownPreviewView::register(workspace, window, cx);
    })
    .detach();
}
