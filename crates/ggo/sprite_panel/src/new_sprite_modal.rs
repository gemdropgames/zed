//! The "New Sprite…"/"New Metasprite…" binding card: a centred workspace
//! modal holding a searchable list of the project's tilesets and a live
//! preview of the highlighted one.
//!
//! **Why it left the tab.** The question the card asks -- *which `.til`
//! does this sprite bind to?* -- is answered once, before the document
//! exists, and the answer is consequential enough that it needs the
//! whole window: binding a tileset another sprite owns makes every later
//! save of either one rewrite the other's tiles and palette
//! ([`TilesetChoice`] spells the hazard out). The tab-hosted version put
//! that behind a dropdown whose rows were one line of text each, above a
//! viewer with nothing in it. A modal can show the art instead, which is
//! what a user actually picks by.
//!
//! **The panel still writes.** This view owns no document state: confirm
//! hands the collected `(kind, dir_rel, name, til_rel)` back to
//! [`SpritePanel::confirm_new_binding`], which keeps the unsaved-edits
//! guard and the single `create_sprite` call where they were.

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

use crate::{
    NewKind, NewSpriteOutcome, PICKER_CELL_PX, SpritePanel, TilesetChoice, compose_tileset_preview,
};
use ggo_worldlib::sprites::hw::TILE_PX;

/// The results column's width. `Picker` applies this to itself, so the
/// card's body row only has to leave the rest to the preview. Rems, not
/// pixels: `Picker` derives its own minimum width from a rems-based
/// initial width and silently keeps the default from a pixel one.
const PICKER_WIDTH: gpui::Rems = gpui::rems(20.);

/// How tall the picker's list is allowed to get before it scrolls.
const PICKER_MAX_HEIGHT: gpui::Rems = gpui::rems(20.);

/// The card's overall width: the picker column plus a preview column
/// wide enough for a sheet a few tiles across before it has to scroll.
const CARD_WIDTH: gpui::Pixels = px(760.);

/// How tall the preview is allowed to get, as a fraction of the window.
/// Half rather than the usual 70%: the modal layer parks the card 80px
/// from the top, so a taller body would push the card's footer off a
/// short window instead of scrolling.
const PREVIEW_MAX_VH: f32 = 0.5;

pub struct NewSpriteModal {
    panel: WeakEntity<SpritePanel>,
    kind: NewKind,
    /// The clicked directory, worktree-relative.
    dir_rel: String,
    /// The stem typed into the project panel's inline editor.
    name: String,
    /// The asset root the tileset rels are relative to -- the frame a
    /// `.spr` stores its `til_path` in.
    root: PathBuf,
    picker: Entity<Picker<TilesetPickerDelegate>>,
    /// The highlighted row, mirrored out of the delegate so confirm, the
    /// preview and the sharing warning never have to READ the picker --
    /// confirm and the highlight callback both run while it is leased.
    selected: Option<TilesetChoice>,
    /// Composed previews by tileset rel. `None` means "composed, and that
    /// tileset has nothing to show", which has to be distinguishable from
    /// "not composed yet" or every render would respawn the compose.
    previews: HashMap<String, Option<Arc<RenderImage>>>,
    /// The in-flight compose. Held in a field so replacing it cancels the
    /// previous one -- that, not a sequence number, is what makes the
    /// latest highlight the one that wins.
    preview_task: Option<Task<()>>,
    /// A failed `create_sprite`, shown on the card: the write is the last
    /// thing that happens, so the card stays up to be retried.
    error: Option<String>,
    focus_handle: FocusHandle,
}

impl NewSpriteModal {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        panel: WeakEntity<SpritePanel>,
        kind: NewKind,
        dir_rel: String,
        name: String,
        root: PathBuf,
        choices: Vec<TilesetChoice>,
        selected: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let highlighted = choices.get(selected).cloned();
        let delegate = TilesetPickerDelegate {
            modal: cx.weak_entity(),
            choices,
            matches: Vec::new(),
            selected_index: selected,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                // The card draws its own chrome, so the picker must not
                // draw a second elevated surface inside it -- nor dismiss
                // itself on blur, which is the modal layer's job here.
                .embedded()
                .initial_width(PICKER_WIDTH)
                .max_height(PICKER_MAX_HEIGHT)
        });
        let mut this = Self {
            panel,
            kind,
            dir_rel,
            name,
            root,
            picker,
            selected: highlighted,
            previews: HashMap::new(),
            preview_task: None,
            error: None,
            focus_handle: cx.focus_handle(),
        };
        this.refresh_preview(cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<TilesetPickerDelegate>> {
        &self.picker
    }

    #[cfg(test)]
    pub(crate) fn kind(&self) -> NewKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The highlighted tileset's composed preview, once it has been.
    pub(crate) fn preview_image(&self) -> Option<Arc<RenderImage>> {
        let rel = &self.selected.as_ref()?.rel;
        self.previews.get(rel)?.clone()
    }

    /// The highlight moved. Called by the delegate, which passes the row
    /// in rather than letting this read the picker back: it is leased.
    fn highlight(&mut self, choice: Option<TilesetChoice>, cx: &mut Context<Self>) {
        if self.selected == choice {
            return;
        }
        self.selected = choice;
        self.refresh_preview(cx);
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let Some(rel) = self.selected.as_ref().map(|choice| choice.rel.clone()) else {
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
                    async move { compose_tileset_preview(&root, &rel) }
                })
                .await;
            this.update(cx, |this, cx| {
                this.previews.insert(rel, composed);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Write the sprite with the highlighted binding. A no-op when the
    /// project has no tileset -- there is nothing legal to bind, and a
    /// `.spr` has no unbound representation ([`crate::create_sprite`]).
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(choice), Some(panel)) = (self.selected.clone(), self.panel.upgrade()) else {
            return;
        };
        let til_rel = choice.rel;
        let (kind, dir_rel, name) = (self.kind, self.dir_rel.clone(), self.name.clone());
        let outcome = panel.update(cx, |panel, cx| {
            panel.confirm_new_binding(kind, dir_rel, name, til_rel, window, cx)
        });
        cx.spawn(async move |this, cx| {
            let outcome = outcome.await;
            this.update(cx, |this, cx| match outcome {
                NewSpriteOutcome::Created => cx.emit(DismissEvent),
                // The unsaved-edits prompt was answered "Cancel": the
                // card stays up with nothing written, so the user can
                // save the other document and try again.
                NewSpriteOutcome::Cancelled => {}
                NewSpriteOutcome::Failed(message) => {
                    this.error = Some(message);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_preview(&self, window: &mut Window) -> impl IntoElement {
        let image = self.preview_image();
        let sized = image.map(|image| {
            let size = image.size(0);
            let scale = PICKER_CELL_PX / TILE_PX as f32;
            let (width, height) = (
                px(size.width.0 as f32 * scale),
                px(size.height.0 as f32 * scale),
            );
            div()
                .flex_none()
                // Auto margins, not `justify_center`: a centred child in
                // a scroller starts at a negative offset once it
                // overflows, and the scroll range (`-overflow..=0`) can
                // never walk back to it. Auto margins centre while it
                // fits and collapse to zero the moment it does not --
                // but only on the flex MAIN axis, which is y here, so
                // the horizontal margins stay 0 and a wide sheet starts
                // flush against the region's left edge instead of half
                // off it.
                .my_auto()
                .debug_selector(|| "ggo-sprite-new-preview".into())
                .child(img(image).nearest(true).w(width).h(height))
        });
        v_flex()
            .id("ggo-sprite-new-preview-region")
            .flex_1()
            .min_w_0()
            .max_h(vh(PREVIEW_MAX_VH, window))
            // Both axes: a sheet can outgrow the column either way, and a
            // scroll container's automatic minimum size is zero, which is
            // what stops the card growing to fit it.
            .overflow_scroll()
            .when(sized.is_none(), |this| {
                this.child(
                    div().m_auto().child(
                        Label::new("No preview")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .children(sized)
    }
}

impl EventEmitter<DismissEvent> for NewSpriteModal {}

impl Focusable for NewSpriteModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The picker's, so the card opens with the search field ready to
        // type into. `focus_handle` on the picker resolves to its query
        // editor, which is what the modal layer focuses on open.
        if self.selected.is_some() {
            self.picker.focus_handle(cx)
        } else {
            // Nothing to search: the picker is not rendered at all, so
            // its handle would never receive the focus the layer hands
            // it and Escape would have nowhere to land.
            self.focus_handle.clone()
        }
    }
}

impl ModalView for NewSpriteModal {}

impl Render for NewSpriteModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_tilesets = self.selected.is_some();
        let share_warning = self
            .selected
            .as_ref()
            .and_then(TilesetChoice::share_warning);
        let body = if has_tilesets {
            h_flex()
                .w_full()
                .gap_2()
                .items_start()
                .child(self.picker.clone())
                .child(self.render_preview(window))
                .into_any_element()
        } else {
            v_flex()
                .debug_selector(|| "ggo-sprite-new-empty".into())
                .child(Label::new(
                    "No tileset in this project. Import one first.",
                ))
                .into_any_element()
        };
        let create = has_tilesets.then(|| {
            div()
                .debug_selector(|| "ggo-sprite-new-create".into())
                .child(
                    Button::new("ggo-sprite-new-create", "Create")
                        .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                )
        });
        div()
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(CARD_WIDTH)
            .child(
                Modal::new("ggo-sprite-new", None)
                    .header(
                        ModalHeader::new()
                            .headline(format!("{} {}", self.kind.label(), self.name))
                            // Where it lands. The tab-hosted form said
                            // this inline ("… in assets/sprites") and it
                            // is the only thing left that says WHICH
                            // right-click raised the card.
                            .description(format!("in {}", self.dir_rel)),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_1()
                                .child(body)
                                // A WARNING, not an error: binding a
                                // tileset another sprite owns is legal
                                // and sometimes wanted, it just has
                                // consequences for a file the user did
                                // not open.
                                .children(share_warning.map(|warning| {
                                    Label::new(warning)
                                        .size(LabelSize::Small)
                                        .color(Color::Warning)
                                }))
                                .children(self.error.clone().map(|error| {
                                    ggo_common::CopyableText::new(
                                        "ggo-sprite-new-error-copy",
                                        error,
                                    )
                                    .size(LabelSize::Small)
                                })),
                        ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .children(create)
                                .child(
                                    div()
                                        .debug_selector(|| "ggo-sprite-new-cancel".into())
                                        .child(Button::new("ggo-sprite-new-cancel", "Cancel").on_click(
                                            cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                        )),
                                ),
                        ),
                    ),
            )
    }
}

pub struct TilesetPickerDelegate {
    modal: WeakEntity<NewSpriteModal>,
    /// Every `.til` under the asset root with its existing sharers, in
    /// listing order. Empty means the project has no tileset yet.
    choices: Vec<TilesetChoice>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl TilesetPickerDelegate {
    /// The choice behind match row `ix`.
    pub(crate) fn choice_at(&self, ix: usize) -> Option<TilesetChoice> {
        let candidate = self.matches.get(ix)?.candidate_id;
        self.choices.get(candidate).cloned()
    }

    pub(crate) fn selected_choice(&self) -> Option<TilesetChoice> {
        self.choice_at(self.selected_index)
    }

    #[cfg(test)]
    pub(crate) fn selected_rel(&self) -> Option<String> {
        self.selected_choice().map(|choice| choice.rel)
    }

    /// The rels the current query leaves offered, in row order.
    #[cfg(test)]
    pub(crate) fn match_rels(&self) -> Vec<String> {
        (0..self.matches.len())
            .filter_map(|ix| self.choice_at(ix).map(|choice| choice.rel))
            .collect()
    }

    fn publish_highlight(&self, cx: &mut App) {
        let choice = self.selected_choice();
        self.modal
            .update(cx, |modal, cx| modal.highlight(choice, cx))
            .ok();
    }
}

impl PickerDelegate for TilesetPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "new sprite tileset"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Bind a tileset…".into()
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
            .choices
            .iter()
            .enumerate()
            .map(|(id, choice)| StringMatchCandidate::new(id, &choice.rel))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
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
            };
            this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                // An empty query lists every tileset in listing order, so
                // the default row ([`crate::default_tileset_choice`])
                // survives the initial pass unmoved; any other query can
                // drop it, and the top surviving row takes the highlight.
                let last = this.delegate.matches.len().saturating_sub(1);
                this.delegate.selected_index = if query.is_empty() {
                    this.delegate.selected_index.min(last)
                } else {
                    0
                };
                this.delegate.publish_highlight(cx);
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal
            .update(cx, |modal, cx| modal.create(window, cx))
            .ok();
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
        let choice = self.choice_at(ix)?;
        let sharers = match choice.sharers.len() {
            0 => None,
            1 => Some("shared by 1 sprite".to_string()),
            n => Some(format!("shared by {n} sprites")),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(choice.rel))
                        .children(sharers.map(|sharers| {
                            Label::new(sharers).size(LabelSize::Small).color(Color::Muted)
                        })),
                ),
        )
    }
}
