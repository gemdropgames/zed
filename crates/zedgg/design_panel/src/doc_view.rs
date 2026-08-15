//! `DesignDocView`: a workspace pane item that is a full `Editor` over a
//! design document's markdown, backed by `zedgg.sqlite` instead of a file.
//! Save (cmd-s, the close prompt) writes the buffer back to the DB;
//! `reload` re-reads it. Shape follows `collab_ui::channel_view` (editor
//! wrapping + event re-emit) and `repl`'s notebook (custom `save`).
//!
//! The buffer has NO `language::File`: `Buffer::is_dirty` tracks unsaved
//! edits against `saved_version` on its own, and `has_conflict` is never
//! raised for a fileless buffer. What DOES need saying explicitly is
//! `buffer_kind == Singleton` -- without it `Pane::skip_save_on_close`
//! treats the item as having nothing to save and closes a dirty tab with no
//! prompt.

use std::any::TypeId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyEntity, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Image, ImageFormat, ImageSource, IntoElement, Render, SharedString, Subscription, Task,
    WeakEntity, Window, div,
};
use language::Buffer;
use project::Project;
use ui::prelude::*;
use workspace::item::{Item, ItemBufferKind, ItemEvent, SaveOptions};
use workspace::Workspace;
use zedgg_project_db::design_docs::{self, NodeKind};
use zedgg_project_db::open as open_db;

/// Per-view cache of images resolved for the markdown preview, keyed by
/// the reference as written (`images/x.png`). `None` = looked up, missing.
type ImageCache = Arc<Mutex<HashMap<String, Option<Arc<Image>>>>>;

pub struct DesignDocView {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    doc_id: i64,
    name: SharedString,
    project_root: PathBuf,
    images: ImageCache,
    _editor_event_subscription: Subscription,
}

/// Open (or re-activate) the tab for doc `id`. Name and body are read
/// from the DB on a background thread.
pub fn open_doc(
    workspace: WeakEntity<Workspace>,
    project_root: PathBuf,
    id: i64,
    window: &mut Window,
    cx: &mut App,
) {
    let existing = workspace
        .read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<DesignDocView>(cx)
                .find(|view| view.read(cx).doc_id == id)
        })
        .ok()
        .flatten();
    if let Some(existing) = existing {
        workspace
            .update(cx, |workspace, cx| {
                workspace.activate_item(&existing, true, true, window, cx);
            })
            .ok();
        return;
    }

    let load = cx.background_spawn({
        let project_root = project_root.clone();
        async move {
            let connection = open_db(&project_root)?;
            let node = design_docs::get_node(&connection, id)?
                .with_context(|| format!("no design doc with id {id}"))?;
            anyhow::ensure!(node.kind == NodeKind::Doc, "{:?} is not a document", node.name);
            let body = design_docs::load_body(&connection, id)?;
            anyhow::Ok((node, body))
        }
    });
    window
        .spawn(cx, async move |cx| {
            let (node, body) = load.await?;
            let markdown = cx
                .update(|_, cx| {
                    workspace.read_with(cx, |workspace, cx| {
                        workspace
                            .project()
                            .read(cx)
                            .languages()
                            .language_for_name("Markdown")
                    })
                })??
                .await
                .ok();
            workspace.update_in(cx, |workspace, window, cx| {
                let project = workspace.project().clone();
                let view = cx.new(|cx| {
                    DesignDocView::new(
                        project_root,
                        node.id,
                        node.parent_id.unwrap_or(design_docs::ROOT_ID),
                        node.name.into(),
                        body,
                        markdown,
                        project,
                        window,
                        cx,
                    )
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
        })
        .detach_and_log_err(cx);
}

impl DesignDocView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        project_root: PathBuf,
        doc_id: i64,
        parent_id: i64,
        name: SharedString,
        body: String,
        markdown: Option<Arc<language::Language>>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let languages = project.read(cx).languages().clone();
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(body, cx);
            buffer.set_language_registry(languages);
            buffer.set_language(markdown, cx);
            buffer
        });
        let editor = cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project), window, cx));
        // Without this the pane never hears about edits: no dirty dot, no
        // save prompt.
        let _editor_event_subscription =
            cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| cx.emit(event.clone()));

        // Relative image references (`images/x.png`) resolve from the doc's
        // own folder, `parent_id`.
        let images: ImageCache = Default::default();
        let buffer_id = buffer.entity_id();
        markdown_preview::set_buffer_image_resolver(
            cx,
            buffer_id,
            image_resolver(project_root.clone(), parent_id, images.clone()),
        );
        cx.on_release(move |_, cx| markdown_preview::remove_buffer_image_resolver(cx, buffer_id))
            .detach();

        Self {
            editor,
            buffer,
            doc_id,
            name,
            project_root,
            images,
            _editor_event_subscription,
        }
    }

    pub fn doc_id(&self) -> i64 {
        self.doc_id
    }

    pub fn set_name(&mut self, name: SharedString, cx: &mut Context<Self>) {
        self.name = name;
        cx.emit(EditorEvent::TitleChanged);
    }

    /// Forget resolved preview images so a re-imported file shows fresh.
    pub fn clear_image_cache(&mut self, cx: &mut Context<Self>) {
        if let Ok(mut images) = self.images.lock() {
            images.clear();
        }
        cx.notify();
    }

    fn set_body_from_db(&mut self, body: String, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_text(body, cx);
            let version = buffer.version();
            buffer.did_save(version, None, cx);
        });
    }
}

/// The preview's image resolver for one doc: `images/x.png` is looked up
/// in the design tree relative to the doc's folder and the blob decoded.
///
/// ponytail: synchronous sqlite read on the UI thread, once per reference
/// per view (cached, including misses). Move to a background prefetch if
/// large images ever stall the preview.
fn image_resolver(
    project_root: PathBuf,
    parent_id: i64,
    cache: ImageCache,
) -> markdown_preview::BufferImageResolver {
    Arc::new(move |reference: &str| {
        if reference.contains("://") || reference.starts_with("data:") {
            return None;
        }
        if let Ok(cache) = cache.lock()
            && let Some(hit) = cache.get(reference)
        {
            return hit.clone().map(ImageSource::Image);
        }
        let image = load_image(&project_root, parent_id, reference)
            .ok()
            .flatten();
        if let Ok(mut cache) = cache.lock() {
            cache.insert(reference.to_string(), image.clone());
        }
        image.map(ImageSource::Image)
    })
}

fn load_image(project_root: &Path, parent_id: i64, reference: &str) -> Result<Option<Arc<Image>>> {
    let connection = open_db(project_root)?;
    let Some(node) = design_docs::resolve_path(&connection, parent_id, reference)? else {
        return Ok(None);
    };
    if node.kind != NodeKind::File {
        return Ok(None);
    }
    let Some(format) = image_format(&node.name) else {
        return Ok(None);
    };
    let bytes = design_docs::load_file(&connection, node.id)?;
    Ok(Some(Arc::new(Image::from_bytes(format, bytes))))
}

fn image_format(name: &str) -> Option<ImageFormat> {
    let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        _ => return None,
    })
}

impl EventEmitter<EditorEvent> for DesignDocView {}

impl Focusable for DesignDocView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for DesignDocView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.editor.clone())
    }
}

impl Item for DesignDocView {
    type Event = EditorEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.name.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDoc))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("Design doc: {}", self.name).into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer.read(cx).is_dirty()
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn can_save_as(&self, _cx: &App) -> bool {
        false
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let (text, version) = {
            let buffer = self.buffer.read(cx);
            (buffer.text(), buffer.version())
        };
        let write = cx.background_spawn({
            let root = self.project_root.clone();
            let id = self.doc_id;
            async move { design_docs::save_body(&open_db(&root)?, id, &text) }
        });
        cx.spawn(async move |this, cx| {
            write.await?;
            // Only edits up to `version` are saved; anything typed during
            // the write keeps the tab dirty.
            this.update(cx, |this, cx| {
                this.buffer
                    .update(cx, |buffer, cx| buffer.did_save(version, None, cx));
            })
        })
    }

    fn reload(
        &mut self,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let read = cx.background_spawn({
            let root = self.project_root.clone();
            let id = self.doc_id;
            async move { design_docs::load_body(&open_db(&root)?, id) }
        });
        cx.spawn(async move |this, cx| {
            let body = read.await?;
            this.update(cx, |this, cx| this.set_body_from_db(body, cx))
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl DesignDocView {
    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }
}
