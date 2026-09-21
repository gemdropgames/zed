//! The "Bind tileset…" card: a centred workspace modal holding a
//! searchable list of the project's `.til`s and a live preview of the
//! highlighted one.
//!
//! **Why it left the popover.** The question it asks -- *which tileset do
//! this map's cell indices mean?* -- is answered by looking at the art,
//! not by reading a path. A rebind re-points every cell in the document
//! at a different sheet, and the popover this replaced offered that as a
//! menu of one-line rels with nothing to judge them by. It is the same
//! trade `ggo_sprite_panel::new_sprite_modal` made for the same question,
//! and this card is modelled on it.
//!
//! **This view binds nothing.** Confirm calls the `bind` callback it was
//! constructed with, which is
//! [`PaintHost::bind_paint_tileset`](crate::paint_ui::PaintHost::bind_paint_tileset)
//! -- the resolve-then-apply rule stays in one place, and a binding that
//! will not open still leaves the document exactly as it was.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
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

use ggo_worldlib::sprites::tileset_doc::TILE_PX;

use crate::loader;

/// The results column's width. Rems rather than pixels: `Picker` derives
/// its own minimum width from a rems-based initial width and silently
/// keeps its default from a pixel one.
const PICKER_WIDTH: gpui::Rems = gpui::rems(20.);

/// How tall the picker's list gets before it scrolls.
const PICKER_MAX_HEIGHT: gpui::Rems = gpui::rems(20.);

/// The card's overall width: the picker column plus a preview column wide
/// enough for a sheet a few tiles across before it has to scroll.
const CARD_WIDTH: gpui::Pixels = px(760.);

/// How tall the preview gets, as a fraction of the window. Half, not the
/// usual 70%: the modal layer parks the card 80px from the top, so a
/// taller body pushes the footer off a short window.
const PREVIEW_MAX_VH: f32 = 0.5;

/// Preview tiles are drawn at this many pixels a side, so a 16px tile
/// reads at a glance instead of at native size.
const PREVIEW_CELL_PX: f32 = 32.;

/// What the card does with the pick. Takes `&mut App` because it runs
/// from the card's own update, where the host is not leased.
pub type BindCallback = Rc<dyn Fn(String, &mut App)>;

pub struct BindTilesetModal {
    /// The asset root the rels are relative to -- the frame a `.map`
    /// stores its `til_path` in, and so the frame a preview composes in.
    root: PathBuf,
    picker: Entity<Picker<TilesetPickerDelegate>>,
    /// The highlighted rel, mirrored out of the delegate so confirm and
    /// the preview never have to READ the picker: both run while it is
    /// leased.
    selected: Option<String>,
    /// Composed previews by rel. `None` means "composed, and there is
    /// nothing to show", which has to be distinguishable from "not
    /// composed yet" or every render would respawn the compose.
    previews: HashMap<String, Option<Arc<RenderImage>>>,
    /// The in-flight compose, held so replacing it cancels the previous
    /// one -- that is what makes the latest highlight the one that wins.
    preview_task: Option<Task<()>>,
    bind: BindCallback,
    focus_handle: FocusHandle,
}

impl BindTilesetModal {
    pub fn new(
        root: PathBuf,
        choices: Vec<String>,
        selected: usize,
        bind: BindCallback,
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
        let mut this = BindTilesetModal {
            root,
            picker,
            selected: highlighted,
            previews: HashMap::new(),
            preview_task: None,
            bind,
            focus_handle: cx.focus_handle(),
        };
        this.refresh_preview(cx);
        this
    }

    /// The highlight moved. Called by the delegate, which passes the row
    /// in rather than letting this read the picker back: it is leased.
    fn highlight(&mut self, choice: Option<String>, cx: &mut Context<Self>) {
        if self.selected == choice {
            return;
        }
        self.selected = choice;
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
                    async move { loader::compose_preview(&root, &rel) }
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
        (self.bind)(rel, cx);
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
                // Auto margins rather than `justify_center`: a centred
                // child in a scroller starts at a negative offset once it
                // overflows and the scroll range can never walk back to
                // it. Margins collapse to zero the moment it stops
                // fitting.
                .my_auto()
                .debug_selector(|| "ggo-map-bind-preview".into())
                .child(img(image).nearest(true).w(width).h(height))
        });
        v_flex()
            .id("ggo-map-bind-preview-region")
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

impl EventEmitter<DismissEvent> for BindTilesetModal {}

impl Focusable for BindTilesetModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The picker's, so the card opens with the search field ready to
        // type into -- unless there is nothing to search, in which case
        // the picker is not rendered at all and its handle would never
        // receive the focus the modal layer hands it (leaving Escape with
        // nowhere to land).
        if self.selected.is_some() {
            self.picker.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl ModalView for BindTilesetModal {}

impl Render for BindTilesetModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_tilesets = self.selected.is_some();
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
                .debug_selector(|| "ggo-map-bind-empty".into())
                .child(Label::new(
                    "No tileset in this project. Import one first.",
                ))
                .into_any_element()
        };
        let confirm = has_tilesets.then(|| {
            div()
                .debug_selector(|| "ggo-map-bind-confirm".into())
                .child(
                    Button::new("ggo-map-bind-confirm", "Bind")
                        .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                )
        });
        div()
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(CARD_WIDTH)
            .child(
                Modal::new("ggo-map-bind", None)
                    .header(
                        ModalHeader::new()
                            .headline("Bind tileset")
                            // A rebind re-points every painted cell, so
                            // the card says so where the choice is made.
                            .description("The map's cells are tile indices into this sheet"),
                    )
                    .section(Section::new().child(body))
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex().gap_1().children(confirm).child(
                                div()
                                    .debug_selector(|| "ggo-map-bind-cancel".into())
                                    .child(Button::new("ggo-map-bind-cancel", "Cancel").on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                    )),
                            ),
                        ),
                    ),
            )
    }
}

pub struct TilesetPickerDelegate {
    modal: WeakEntity<BindTilesetModal>,
    /// Every `.til` under the asset root, in listing order. Empty means
    /// the project has no tileset yet.
    choices: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl TilesetPickerDelegate {
    fn choice_at(&self, ix: usize) -> Option<String> {
        let candidate = self.matches.get(ix)?.candidate_id;
        self.choices.get(candidate).cloned()
    }

    fn publish_highlight(&self, cx: &mut App) {
        let choice = self.choice_at(self.selected_index);
        self.modal
            .update(cx, |modal, cx| modal.highlight(choice, cx))
            .ok();
    }
}

impl PickerDelegate for TilesetPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "map bind tileset"
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
            .map(|(id, rel)| StringMatchCandidate::new(id, rel))
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
                // the CURRENTLY BOUND row keeps the highlight it opened
                // on; any other query can drop it, and the top surviving
                // row takes over.
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
        let rel = self.choice_at(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(rel)),
        )
    }
}
