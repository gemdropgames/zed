//! "+ System": the card that adds a system to the selected schedule's
//! run list.
//!
//! **Why it left the dropdown.** The control it replaces was a
//! `DropdownMenu` of bare `module/name` refs. Two things were wrong with
//! that. A schedule's whole meaning is its ORDER, and a menu entry said
//! nothing about where the pick would land -- the user learned that only
//! after the `emd schedule set` had already run. And a menu is a flat
//! list: a project with thirty systems gave thirty rows and no way to
//! search them. The card answers both -- a fuzzy query over the
//! candidates, and the resulting run order beside it -- before anything
//! is committed.
//!
//! **The panel still commits.** This view owns no schedule state: confirm
//! hands the highlighted ref back to [`EmeraldPanel::add_system`], which
//! keeps the optimistic commit, the version-lock gate and the single
//! `emd schedule set` exactly where they were.

use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Styled, Task, WeakEntity, Window, div, px,
};
use picker::{Picker, PickerDelegate};
use ui::prelude::*;
use ui::{ListItem, ListItemSpacing, Modal, ModalFooter, ModalHeader, Section};
use workspace::ModalView;

use crate::EmeraldPanel;

/// The results column's width. Rems, not pixels: `Picker` derives its
/// own minimum width from a rems-based initial width and silently keeps
/// the default from a pixel one.
const PICKER_WIDTH: gpui::Rems = gpui::rems(18.);

/// How tall the picker's list is allowed to get before it scrolls.
const PICKER_MAX_HEIGHT: gpui::Rems = gpui::rems(20.);

/// The card's overall width: the picker column plus a preview column
/// wide enough for a qualified `module/name@N` ref.
const CARD_WIDTH: gpui::Pixels = px(640.);

/// How tall the preview column is allowed to get, as a fraction of the
/// window -- half, for the same reason the generate card's body is: the
/// modal layer parks the card 80px from the top, so a taller column
/// would push the footer off a short window instead of scrolling.
const PREVIEW_MAX_VH: f32 = 0.5;

pub struct AddSystemModal {
    panel: WeakEntity<EmeraldPanel>,
    /// The schedule being edited, for the header.
    schedule: String,
    /// The run list as it stands. The preview is this with the
    /// highlighted candidate appended -- which is exactly what
    /// `OrderEdit::Add` produces, so the card cannot promise an order the
    /// commit would not make.
    order: Vec<String>,
    picker: Entity<Picker<SystemPickerDelegate>>,
    /// The highlighted row's ref, mirrored out of the delegate so confirm
    /// and the preview never have to READ the picker -- both run while it
    /// is leased.
    selected: Option<String>,
}

impl AddSystemModal {
    pub(crate) fn new(
        panel: WeakEntity<EmeraldPanel>,
        schedule: String,
        order: Vec<String>,
        candidates: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected = candidates.first().cloned();
        let delegate = SystemPickerDelegate {
            modal: cx.weak_entity(),
            candidates,
            matches: Vec::new(),
            selected_index: 0,
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
        Self {
            panel,
            schedule,
            order,
            picker,
            selected,
        }
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<SystemPickerDelegate>> {
        &self.picker
    }

    /// The run list the confirm would leave behind: the current order
    /// with the highlighted candidate appended.
    pub(crate) fn preview(&self) -> Vec<String> {
        let mut rows = self.order.clone();
        rows.extend(self.selected.clone());
        rows
    }

    /// The highlight moved. Called by the delegate, which passes the row
    /// in rather than letting this read the picker back: it is leased.
    fn highlight(&mut self, system_ref: Option<String>, cx: &mut Context<Self>) {
        if self.selected == system_ref {
            return;
        }
        self.selected = system_ref;
        cx.notify();
    }

    /// Commit the highlighted candidate through the panel's own
    /// `schedule set` path, then get out of the way.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(system_ref), Some(panel)) = (self.selected.clone(), self.panel.upgrade()) else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.add_system(&system_ref, window, cx);
        });
        cx.emit(DismissEvent);
    }

    fn render_preview(&self, window: &mut Window, cx: &App) -> impl IntoElement {
        let rows = self.preview();
        let appended = self.order.len();
        v_flex()
            .id("ggo-emerald-add-system-preview")
            .debug_selector(|| "ggo-emerald-add-system-preview".into())
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .max_h(vh(PREVIEW_MAX_VH, window))
            .overflow_scroll()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .pl_2()
            .child(
                Label::new("Run order")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children(rows.into_iter().enumerate().map(|(ix, row)| {
                let new = ix == appended;
                Label::new(format!("{}. {row}", ix + 1))
                    .size(LabelSize::Small)
                    .color(if new { Color::Accent } else { Color::Muted })
            }))
    }
}

impl EventEmitter<DismissEvent> for AddSystemModal {}

impl Focusable for AddSystemModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The picker's, which resolves to its query editor -- the card
        // opens ready to be searched. It is never empty: the panel only
        // opens this card when `available_systems` found something.
        self.picker.focus_handle(cx)
    }
}

impl ModalView for AddSystemModal {}

impl Render for AddSystemModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.render_preview(window, cx);
        div().elevation_3(cx).w(CARD_WIDTH).child(
            Modal::new("ggo-emerald-add-system", None)
                .header(
                    ModalHeader::new()
                        .headline("Add a system")
                        .description(format!("to schedule {}", self.schedule)),
                )
                .section(
                    Section::new().child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_start()
                            .child(self.picker.clone())
                            .child(preview),
                    ),
                )
                .footer(
                    ModalFooter::new().end_slot(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .debug_selector(|| "ggo-emerald-add-system-confirm".into())
                                    .child(
                                        Button::new("ggo-emerald-add-system-confirm", "Add")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.confirm(window, cx)
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "ggo-emerald-add-system-cancel".into())
                                    .child(
                                        Button::new("ggo-emerald-add-system-cancel", "Cancel")
                                            .on_click(
                                                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                            ),
                                    ),
                            ),
                    ),
                ),
        )
    }
}

pub struct SystemPickerDelegate {
    modal: WeakEntity<AddSystemModal>,
    /// Every system NOT already in the schedule's order, as qualified
    /// refs, in manifest order.
    candidates: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl SystemPickerDelegate {
    fn ref_at(&self, ix: usize) -> Option<String> {
        let candidate = self.matches.get(ix)?.candidate_id;
        self.candidates.get(candidate).cloned()
    }

    /// The refs the current query leaves offered, in row order.
    #[cfg(test)]
    pub(crate) fn match_refs(&self) -> Vec<String> {
        (0..self.matches.len())
            .filter_map(|ix| self.ref_at(ix))
            .collect()
    }

    fn publish_highlight(&self, cx: &mut App) {
        let system_ref = self.ref_at(self.selected_index);
        self.modal
            .update(cx, |modal, cx| modal.highlight(system_ref, cx))
            .ok();
    }
}

impl PickerDelegate for SystemPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "emerald add system"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Add a system…".into()
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
            .map(|(id, system_ref)| StringMatchCandidate::new(id, system_ref))
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
                // An empty query lists every candidate in manifest order,
                // so a highlight the user moved survives it; any other
                // query can drop the highlighted row, and the top
                // surviving row takes over.
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
            .update(cx, |modal, cx| modal.confirm(window, cx))
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
        let system_ref = self.ref_at(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(system_ref)),
        )
    }
}
