//! The world panel's three creation cards (P1): "+ Instance", a layer's
//! "Add…" background, and the entity inspector's "Add component…".
//!
//! **Why they left their menus.** All three used to be one-line menus --
//! a `ContextMenu` of world stems, a `ContextMenu` of `.til` rels, a
//! `DropdownMenu` of schema names. Each asks a question the label alone
//! cannot answer: *how big is the world I am about to instance?*, *what
//! does that tileset look like, and how large should the new map be?*,
//! *which fields does this component seed?* A card can show that, and a
//! menu of twenty rels cannot even be searched.
//!
//! **The cards write nothing.** Each one confirms back through the
//! panel's existing apply path
//! ([`WorldPanel::add_instance_impl`](crate::WorldPanel),
//! `add_background_impl`, `WorldOp::AddComponent`), so every guard --
//! the cycle check, the corrupt-`.map` refusal, the Play gate -- stays
//! exactly where it was and a card that is confirmed twice is no
//! different from a menu that is clicked twice.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    RenderImage, Styled, Task, WeakEntity, Window, div, img, px,
};
use picker::{Picker, PickerDelegate};
use ui::prelude::*;
use ui::{ListItem, ListItemSpacing, Modal, ModalFooter, ModalHeader, Section};
use workspace::ModalView;

use ggo_worldlib::schemas::{ComponentSchema, defaults_for};
use ggo_worldlib::sprites::tileset_doc::TILE_PX;
use ggo_worldlib::world_file;

use crate::{NEW_BG_DIM, WorldPanel};

/// The results column's width. Rems rather than pixels: `Picker` derives
/// its own minimum width from a rems-based initial width and silently
/// keeps its default from a pixel one.
const PICKER_WIDTH: gpui::Rems = gpui::rems(18.);

/// How tall the picker's list gets before it scrolls.
const PICKER_MAX_HEIGHT: gpui::Rems = gpui::rems(20.);

/// The card's overall width: the picker column plus a preview column
/// wide enough for a sheet a few tiles across before it has to scroll.
const CARD_WIDTH: gpui::Pixels = px(720.);

/// How tall a preview gets, as a fraction of the window. Half, not the
/// usual 70%: the modal layer parks the card 80px from the top, so a
/// taller body pushes the footer off a short window.
const PREVIEW_MAX_VH: f32 = 0.5;

/// Preview tiles are drawn at this many pixels a side, so a 16px tile
/// reads at a glance instead of at native size.
const PREVIEW_CELL_PX: f32 = 32.;

/// The scrolling region every card's preview sits in: both axes, because
/// a sheet can outgrow the column either way, and a scroll container's
/// automatic minimum size is zero, which is what stops the card growing
/// to fit it.
fn preview_region(id: &'static str, window: &mut Window) -> gpui::Stateful<gpui::Div> {
    v_flex()
        .id(id)
        .flex_1()
        .min_w_0()
        .max_h(vh(PREVIEW_MAX_VH, window))
        .overflow_scroll()
}

/// The "nothing to show yet" filler, centred with auto margins rather
/// than `justify_center` -- a centred child in a scroller starts at a
/// negative offset once it overflows and the scroll range can never walk
/// back to it.
fn preview_placeholder(message: &'static str) -> gpui::Div {
    div().m_auto().child(
        Label::new(message)
            .size(LabelSize::Small)
            .color(Color::Muted),
    )
}

/// An empty-query pass through `fuzzy`: every candidate, in listing
/// order, so a card that has never been typed into shows the same list
/// the menu it replaced did.
async fn matches_for(
    candidates: Vec<StringMatchCandidate>,
    query: String,
    background: gpui::BackgroundExecutor,
) -> Vec<StringMatch> {
    if query.is_empty() {
        return candidates
            .into_iter()
            .map(|candidate| StringMatch {
                candidate_id: candidate.id,
                string: candidate.string,
                positions: Vec::new(),
                score: 0.0,
            })
            .collect();
    }
    match_strings(
        &candidates,
        &query,
        false,
        true,
        100,
        &Default::default(),
        background,
    )
    .await
}

// --------------------------------------------------------- add instance

/// What the add-instance card shows about the highlighted world: its own
/// name and how much of it this world would be taking on. A rendered
/// thumbnail is deliberately out of scope -- composing a candidate
/// world's draw list means resolving its whole asset set (and its own
/// instances) off-thread per highlight, which is the loader's job at
/// OPEN time, not a preview's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstancePreview {
    pub name: String,
    pub entities: usize,
    pub instances: usize,
    pub backgrounds: usize,
}

impl InstancePreview {
    /// Read `stem`'s world file under `root` and count what is in it.
    /// Runs off the UI thread.
    fn read(root: &std::path::Path, stem: &str) -> Option<Self> {
        let world = world_file::read_world(root, &format!("{stem}.wrld.toml")).ok()?;
        Some(InstancePreview {
            name: stem.to_string(),
            entities: world.entities.len(),
            instances: world.instances.len(),
            backgrounds: world.backgrounds.len(),
        })
    }
}

pub struct AddInstanceModal {
    panel: WeakEntity<WorldPanel>,
    /// The asset root the candidate stems resolve against -- the frame an
    /// `[[instance]] world` is written in.
    root: PathBuf,
    picker: Entity<Picker<StemPickerDelegate>>,
    /// The highlighted stem, mirrored out of the delegate so confirm and
    /// the preview never have to READ the picker: both run while it is
    /// leased.
    selected: Option<String>,
    /// Counts by stem. `None` means "read, and the file would not open",
    /// which has to be distinguishable from "not read yet" or every
    /// render would respawn the read.
    previews: HashMap<String, Option<InstancePreview>>,
    /// The in-flight read, held so replacing it cancels the previous one.
    preview_task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl AddInstanceModal {
    pub fn new(
        panel: WeakEntity<WorldPanel>,
        root: PathBuf,
        candidates: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let highlighted = candidates.first().cloned();
        let delegate = StemPickerDelegate {
            modal: cx.weak_entity(),
            candidates,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            // The card draws its own chrome, so the picker must not draw
            // a second elevated surface inside it -- nor dismiss itself
            // on blur, which is the modal layer's job here.
            Picker::uniform_list(delegate, window, cx)
                .embedded()
                .initial_width(PICKER_WIDTH)
                .max_height(PICKER_MAX_HEIGHT)
        });
        let mut this = AddInstanceModal {
            panel,
            root,
            picker,
            selected: highlighted,
            previews: HashMap::new(),
            preview_task: None,
            focus_handle: cx.focus_handle(),
        };
        this.refresh_preview(cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<StemPickerDelegate>> {
        &self.picker
    }

    #[cfg(test)]
    pub(crate) fn selected_stem(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    fn highlight(&mut self, stem: Option<String>, cx: &mut Context<Self>) {
        if self.selected == stem {
            return;
        }
        self.selected = stem;
        self.refresh_preview(cx);
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let Some(stem) = self.selected.clone() else {
            self.preview_task = None;
            return;
        };
        if self.previews.contains_key(&stem) {
            self.preview_task = None;
            return;
        }
        let root = self.root.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let read = cx
                .background_spawn({
                    let (root, stem) = (root.clone(), stem.clone());
                    async move { InstancePreview::read(&root, &stem) }
                })
                .await;
            this.update(cx, |this, cx| {
                this.previews.insert(stem, read);
                cx.notify();
            })
            .ok();
        }));
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(stem) = self.selected.clone() else {
            return;
        };
        self.panel
            .update(cx, |panel, cx| panel.add_instance_impl(stem, cx))
            .ok();
        cx.emit(DismissEvent);
    }

    fn render_preview(&self, window: &mut Window) -> impl IntoElement {
        let preview = self
            .selected
            .as_ref()
            .and_then(|stem| self.previews.get(stem))
            .cloned()
            .flatten();
        let body = preview.map(|preview| {
            v_flex()
                .m_auto()
                .gap_1()
                .debug_selector(|| "ggo-world-add-instance-preview".into())
                .child(Label::new(preview.name.clone()))
                .child(
                    Label::new(format!("{} entities", preview.entities))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(format!("{} instances", preview.instances))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(format!("{} backgrounds", preview.backgrounds))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
        });
        preview_region("ggo-world-add-instance-preview-region", window)
            .when(body.is_none(), |this| {
                this.child(preview_placeholder("No preview"))
            })
            .children(body)
    }
}

impl EventEmitter<DismissEvent> for AddInstanceModal {}

impl Focusable for AddInstanceModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The picker's, so the card opens with the search field ready to
        // type into -- unless there is nothing to search, in which case
        // the picker is not rendered at all and its handle would never
        // receive the focus the modal layer hands it (leaving Escape
        // with nowhere to land).
        if self.selected.is_some() {
            self.picker.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl ModalView for AddInstanceModal {}

impl Render for AddInstanceModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_candidates = self.selected.is_some();
        let body = if has_candidates {
            h_flex()
                .w_full()
                .gap_2()
                .items_start()
                .child(self.picker.clone())
                .child(self.render_preview(window))
                .into_any_element()
        } else {
            v_flex()
                .debug_selector(|| "ggo-world-add-instance-empty".into())
                .child(Label::new(
                    "No other world can be instanced here — every one would close a cycle.",
                ))
                .into_any_element()
        };
        let on_confirm = cx.listener(|this: &mut Self, _, _, cx| this.confirm(cx));
        let focus_handle = self.focus_handle.clone();
        card(
            "ggo-world-add-instance-card",
            "Add instance",
            "The picked world is flattened into this one at load",
            body,
            has_candidates.then_some(("ggo-world-add-instance-confirm", "Add")),
            on_confirm,
            &focus_handle,
            cx,
        )
    }
}

pub struct StemPickerDelegate {
    modal: WeakEntity<AddInstanceModal>,
    candidates: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl StemPickerDelegate {
    fn stem_at(&self, ix: usize) -> Option<String> {
        let candidate = self.matches.get(ix)?.candidate_id;
        self.candidates.get(candidate).cloned()
    }

    /// The stems the current query leaves offered, in row order.
    #[cfg(test)]
    pub(crate) fn match_stems(&self) -> Vec<String> {
        (0..self.matches.len())
            .filter_map(|ix| self.stem_at(ix))
            .collect()
    }

    fn publish_highlight(&self, cx: &mut App) {
        let stem = self.stem_at(self.selected_index);
        self.modal
            .update(cx, |modal, cx| modal.highlight(stem, cx))
            .ok();
    }
}

impl PickerDelegate for StemPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "world add instance"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Instance a world…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        self.publish_highlight(cx);
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .candidates
            .iter()
            .enumerate()
            .map(|(id, stem)| StringMatchCandidate::new(id, stem))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let matches = matches_for(candidates, query, background).await;
            this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = 0;
                this.delegate.publish_highlight(cx);
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |modal, cx| modal.confirm(cx)).ok();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let stem = self.stem_at(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(stem)),
        )
    }
}

// ------------------------------------------------------ add background

pub struct AddBackgroundModal {
    panel: WeakEntity<WorldPanel>,
    layer: u8,
    root: PathBuf,
    picker: Entity<Picker<BackgroundPickerDelegate>>,
    selected: Option<String>,
    previews: HashMap<String, Option<Arc<RenderImage>>>,
    preview_task: Option<Task<()>>,
    /// The generated map's side, in tiles. Typed into, so it is text
    /// until confirm; an unparseable or zero value falls back to
    /// [`NEW_BG_DIM`] rather than refusing the pick, which is what the
    /// menu this replaced did unconditionally.
    dimension: Entity<editor::Editor>,
    focus_handle: FocusHandle,
}

impl AddBackgroundModal {
    pub fn new(
        panel: WeakEntity<WorldPanel>,
        layer: u8,
        root: PathBuf,
        tilesets: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let highlighted = tilesets.first().cloned();
        let delegate = BackgroundPickerDelegate {
            modal: cx.weak_entity(),
            tilesets,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .embedded()
                .initial_width(PICKER_WIDTH)
                .max_height(PICKER_MAX_HEIGHT)
        });
        let dimension = cx.new(|cx| {
            let mut editor = editor::Editor::single_line(window, cx);
            editor.set_text(NEW_BG_DIM.to_string(), window, cx);
            editor
        });
        let mut this = AddBackgroundModal {
            panel,
            layer,
            root,
            picker,
            selected: highlighted,
            previews: HashMap::new(),
            preview_task: None,
            dimension,
            focus_handle: cx.focus_handle(),
        };
        this.refresh_preview(cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<BackgroundPickerDelegate>> {
        &self.picker
    }

    #[cfg(test)]
    pub(crate) fn dimension_editor(&self) -> &Entity<editor::Editor> {
        &self.dimension
    }

    fn highlight(&mut self, rel: Option<String>, cx: &mut Context<Self>) {
        if self.selected == rel {
            return;
        }
        self.selected = rel;
        self.refresh_preview(cx);
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let Some(rel) = self.selected.clone() else {
            self.preview_task = None;
            return;
        };
        if self.previews.contains_key(&rel) {
            self.preview_task = None;
            return;
        }
        let root = self.root.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let composed = cx
                .background_spawn({
                    let (root, rel) = (root.clone(), rel.clone());
                    // The map panel's own composer, so a world-hosted
                    // background and a standalone map preview the same
                    // sheet the same way.
                    async move { ggo_map_panel::loader::compose_preview(&root, &rel) }
                })
                .await;
            this.update(cx, |this, cx| {
                this.previews.insert(rel, composed);
                cx.notify();
            })
            .ok();
        }));
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(rel) = self.selected.clone() else {
            return;
        };
        let dimension = self
            .dimension
            .read(cx)
            .text(cx)
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|dimension| *dimension > 0)
            .unwrap_or(NEW_BG_DIM);
        let layer = self.layer;
        self.panel
            .update(cx, |panel, cx| {
                panel.add_background_impl(layer, rel, dimension, cx)
            })
            .ok();
        cx.emit(DismissEvent);
    }

    fn render_preview(&self, window: &mut Window) -> impl IntoElement {
        let image = self
            .selected
            .as_ref()
            .and_then(|rel| self.previews.get(rel))
            .cloned()
            .flatten();
        let sized = image.map(|image| {
            let size = image.size(0);
            let scale = PREVIEW_CELL_PX / TILE_PX as f32;
            let (width, height) = (
                px(size.width.0 as f32 * scale),
                px(size.height.0 as f32 * scale),
            );
            div()
                .flex_none()
                // Auto margins collapse to zero the moment the sheet
                // stops fitting, which `justify_center` does not -- see
                // [`preview_placeholder`].
                .my_auto()
                .debug_selector(|| "ggo-world-add-bg-preview".into())
                .child(img(image).nearest(true).w(width).h(height))
        });
        preview_region("ggo-world-add-bg-preview-region", window)
            .when(sized.is_none(), |this| {
                this.child(preview_placeholder("No preview"))
            })
            .children(sized)
    }
}

impl EventEmitter<DismissEvent> for AddBackgroundModal {}

impl Focusable for AddBackgroundModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.selected.is_some() {
            self.picker.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl ModalView for AddBackgroundModal {}

impl Render for AddBackgroundModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_tilesets = self.selected.is_some();
        let dimension_row = h_flex()
            .gap_1()
            .items_center()
            .debug_selector(|| "ggo-world-add-bg-dimension".into())
            .child(
                Label::new("Size (tiles)")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .w(px(64.))
                    .px_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .child(self.dimension.clone()),
            );
        let body = if has_tilesets {
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_start()
                        .child(self.picker.clone())
                        .child(self.render_preview(window)),
                )
                .child(dimension_row)
                .into_any_element()
        } else {
            v_flex()
                .debug_selector(|| "ggo-world-add-bg-empty".into())
                .child(Label::new("No tileset in this project. Import one first."))
                .into_any_element()
        };
        let on_confirm = cx.listener(|this: &mut Self, _, _, cx| this.confirm(cx));
        let focus_handle = self.focus_handle.clone();
        card(
            "ggo-world-add-bg-card",
            &format!("Add background to bg{}", self.layer),
            "A new map is generated bound to the picked tileset",
            body,
            has_tilesets.then_some(("ggo-world-add-bg-confirm", "Add")),
            on_confirm,
            &focus_handle,
            cx,
        )
    }
}

pub struct BackgroundPickerDelegate {
    modal: WeakEntity<AddBackgroundModal>,
    tilesets: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl BackgroundPickerDelegate {
    fn rel_at(&self, ix: usize) -> Option<String> {
        let candidate = self.matches.get(ix)?.candidate_id;
        self.tilesets.get(candidate).cloned()
    }

    #[cfg(test)]
    pub(crate) fn match_rels(&self) -> Vec<String> {
        (0..self.matches.len())
            .filter_map(|ix| self.rel_at(ix))
            .collect()
    }

    fn publish_highlight(&self, cx: &mut App) {
        let rel = self.rel_at(self.selected_index);
        self.modal
            .update(cx, |modal, cx| modal.highlight(rel, cx))
            .ok();
    }
}

impl PickerDelegate for BackgroundPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "world add background"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Pick a tileset…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        self.publish_highlight(cx);
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .tilesets
            .iter()
            .enumerate()
            .map(|(id, rel)| StringMatchCandidate::new(id, rel))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let matches = matches_for(candidates, query, background).await;
            this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = 0;
                this.delegate.publish_highlight(cx);
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |modal, cx| modal.confirm(cx)).ok();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let rel = self.rel_at(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(rel)),
        )
    }
}

// ------------------------------------------------------- add component

pub struct AddComponentModal {
    panel: WeakEntity<WorldPanel>,
    entity_ix: usize,
    /// Every schema the entity does not already carry, in listing order.
    schemas: Vec<ComponentSchema>,
    picker: Entity<Picker<SchemaPickerDelegate>>,
    /// The highlighted schema, mirrored out of the delegate so confirm
    /// and the preview never have to READ the picker: both run while it
    /// is leased.
    selected: Option<ComponentSchema>,
    focus_handle: FocusHandle,
}

impl AddComponentModal {
    pub fn new(
        panel: WeakEntity<WorldPanel>,
        entity_ix: usize,
        schemas: Vec<ComponentSchema>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let highlighted = schemas.first().cloned();
        let delegate = SchemaPickerDelegate {
            modal: cx.weak_entity(),
            schemas: schemas.clone(),
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .embedded()
                .initial_width(PICKER_WIDTH)
                .max_height(PICKER_MAX_HEIGHT)
        });
        AddComponentModal {
            panel,
            entity_ix,
            schemas,
            picker,
            selected: highlighted,
            focus_handle: cx.focus_handle(),
        }
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<SchemaPickerDelegate>> {
        &self.picker
    }

    #[cfg(test)]
    pub(crate) fn selected_name(&self) -> Option<&str> {
        self.selected.as_ref().map(|schema| schema.name.as_str())
    }

    fn highlight(&mut self, schema: Option<ComponentSchema>, cx: &mut Context<Self>) {
        if self.selected.as_ref().map(|s| &s.name) == schema.as_ref().map(|s| &s.name) {
            return;
        }
        self.selected = schema;
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(schema) = self.selected.clone() else {
            return;
        };
        let entity = self.entity_ix;
        let defaults = defaults_for(&schema);
        self.panel
            .update(cx, |panel, cx| {
                panel.apply_op(
                    ggo_worldlib::world_doc::WorldOp::AddComponent {
                        entity,
                        name: schema.name.clone(),
                        defaults,
                    },
                    cx,
                );
            })
            .ok();
        cx.emit(DismissEvent);
    }

    /// The fields the pick would seed, exactly as `defaults_for` writes
    /// them -- the one thing a schema NAME cannot tell the user.
    fn render_preview(&self, window: &mut Window) -> impl IntoElement {
        let seeded = self.selected.as_ref().map(|schema| {
            let defaults = defaults_for(schema);
            v_flex()
                .my_auto()
                .gap_1()
                .debug_selector(|| "ggo-world-add-component-preview".into())
                .child(Label::new(schema.name.clone()))
                .children(defaults.into_iter().map(|(field, value)| {
                    Label::new(format!("{field} = {value}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                }))
        });
        preview_region("ggo-world-add-component-preview-region", window)
            .when(seeded.is_none(), |this| {
                this.child(preview_placeholder("No preview"))
            })
            .children(seeded)
    }
}

impl EventEmitter<DismissEvent> for AddComponentModal {}

impl Focusable for AddComponentModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.selected.is_some() {
            self.picker.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl ModalView for AddComponentModal {}

impl Render for AddComponentModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_schemas = !self.schemas.is_empty();
        let body = if has_schemas {
            h_flex()
                .w_full()
                .gap_2()
                .items_start()
                .child(self.picker.clone())
                .child(self.render_preview(window))
                .into_any_element()
        } else {
            v_flex()
                .debug_selector(|| "ggo-world-add-component-empty".into())
                .child(Label::new("This entity already carries every component."))
                .into_any_element()
        };
        let on_confirm = cx.listener(|this: &mut Self, _, _, cx| this.confirm(cx));
        let focus_handle = self.focus_handle.clone();
        card(
            "ggo-world-add-component-card",
            &format!("Add component to entity #{}", self.entity_ix),
            "The picked schema's fields are seeded at their defaults",
            body,
            has_schemas.then_some(("ggo-world-add-component-confirm", "Add")),
            on_confirm,
            &focus_handle,
            cx,
        )
    }
}

pub struct SchemaPickerDelegate {
    modal: WeakEntity<AddComponentModal>,
    schemas: Vec<ComponentSchema>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl SchemaPickerDelegate {
    fn schema_at(&self, ix: usize) -> Option<ComponentSchema> {
        let candidate = self.matches.get(ix)?.candidate_id;
        self.schemas.get(candidate).cloned()
    }

    #[cfg(test)]
    pub(crate) fn match_names(&self) -> Vec<String> {
        (0..self.matches.len())
            .filter_map(|ix| self.schema_at(ix).map(|schema| schema.name))
            .collect()
    }

    fn publish_highlight(&self, cx: &mut App) {
        let schema = self.schema_at(self.selected_index);
        self.modal
            .update(cx, |modal, cx| modal.highlight(schema, cx))
            .ok();
    }
}

impl PickerDelegate for SchemaPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "world add component"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Add a component…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        self.publish_highlight(cx);
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .schemas
            .iter()
            .enumerate()
            .map(|(id, schema)| StringMatchCandidate::new(id, &schema.name))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let matches = matches_for(candidates, query, background).await;
            this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = 0;
                this.delegate.publish_highlight(cx);
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |modal, cx| modal.confirm(cx)).ok();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let schema = self.schema_at(ix)?;
        let fields = schema.fields.len();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex().child(Label::new(schema.name)).child(
                        Label::new(match fields {
                            1 => "1 field".to_string(),
                            n => format!("{n} fields"),
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                ),
        )
    }
}

// ------------------------------------------------------------- chrome

/// The shared card shell all three creation cards draw: header, one
/// section holding the body, and a footer of Confirm + Cancel. `confirm`
/// is `None` when the card has nothing to offer, which is what leaves a
/// dead button off the footer rather than greying one on.
#[allow(clippy::too_many_arguments)]
fn card<V>(
    id: &'static str,
    headline: &str,
    description: &str,
    body: gpui::AnyElement,
    confirm: Option<(&'static str, &'static str)>,
    on_confirm: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    focus_handle: &FocusHandle,
    cx: &mut Context<V>,
) -> gpui::Div
where
    V: EventEmitter<DismissEvent> + 'static,
{
    let cancel_id = "ggo-world-card-cancel";
    div()
        // The card's own handle, so Escape still lands when the picker
        // is not rendered (nothing to search) and never took focus.
        .track_focus(focus_handle)
        .elevation_3(cx)
        .w(CARD_WIDTH)
        .child(
            Modal::new(id, None)
                .header(
                    ModalHeader::new()
                        .headline(headline.to_string())
                        .description(description.to_string()),
                )
                .section(Section::new().child(body))
                .footer(
                    ModalFooter::new().end_slot(
                        h_flex()
                            .gap_1()
                            .children(confirm.map(|(button_id, label)| {
                                div()
                                    .debug_selector(move || button_id.into())
                                    .child(Button::new(button_id, label).on_click(on_confirm))
                            }))
                            .child(
                                div()
                                    .debug_selector(move || cancel_id.into())
                                    .child(Button::new(cancel_id, "Cancel").on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                    )),
                            ),
                    ),
                ),
        )
}
