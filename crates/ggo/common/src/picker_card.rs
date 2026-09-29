//! The chrome and the matching policy every GGO "pick one thing, see it
//! before you commit" card shares: the world panel's add-instance,
//! add-background, add-component and asset-stem cards, the map panel's
//! bind-tileset card, the sprite panel's new-sprite card and the emerald
//! panel's add-system card.
//!
//! **Why it is here and not copied.** Those seven cards were written one
//! after another, each modelled on the last, and the copies drifted:
//! three different card widths for the same two-column layout, two
//! different answers to "where does the highlight go when the query
//! changes", and four transcriptions of the same empty-query pass
//! through `fuzzy`. A user who learns one card's behaviour should not be
//! wrong about the next one, so the behaviour lives once.
//!
//! This module owns no state and commits nothing. A card's confirm still
//! runs through its own panel's apply path, so every guard those paths
//! carry stays exactly where it was.

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, EventEmitter, FocusHandle, IntoElement,
    SharedString, Styled, Window, div, px,
};
use ui::prelude::*;
use ui::{Modal, ModalFooter, ModalHeader, Section};

/// The results column's width. Rems rather than pixels: `Picker` derives
/// its own minimum width from a rems-based initial width and silently
/// keeps its default from a pixel one.
pub const PICKER_WIDTH: gpui::Rems = gpui::rems(20.);

/// How tall a picker's list gets before it scrolls.
pub const PICKER_MAX_HEIGHT: gpui::Rems = gpui::rems(20.);

/// A card's overall width: the picker column plus a preview column wide
/// enough for a sheet a few tiles across before it has to scroll.
pub const CARD_WIDTH: gpui::Pixels = px(760.);

/// How tall a preview gets, as a fraction of the window. Half, not the
/// usual 70%: the modal layer parks a card 80px from the top, so a
/// taller body pushes the footer off a short window.
pub const PREVIEW_MAX_VH: f32 = 0.5;

/// An empty-query pass through `fuzzy`: every candidate, in listing
/// order, so a card that has never been typed into shows the same list
/// the menu it replaced did.
pub async fn matches_for(
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

/// Where the highlight lands once `query`'s matches are in.
///
/// An empty query lists every candidate in listing order, so a highlight
/// the user moved -- or one the card opened on -- survives it. Any other
/// query can drop the highlighted row entirely, and the top surviving
/// row takes over. Clamped, because the previous index can outrun a
/// shorter list.
pub fn reselect_index(previous: usize, query: &str, match_count: usize) -> usize {
    if query.is_empty() {
        previous.min(match_count.saturating_sub(1))
    } else {
        0
    }
}

/// The scrolling region a card's preview sits in: both axes, because a
/// sheet can outgrow the column either way, and a scroll container's
/// automatic minimum size is zero, which is what stops the card growing
/// to fit it.
pub fn preview_region(id: &'static str, window: &mut Window) -> gpui::Stateful<gpui::Div> {
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
/// back to it. Auto margins centre while it fits and collapse to zero
/// the moment it does not.
pub fn preview_placeholder(message: impl Into<SharedString>) -> gpui::Div {
    div().m_auto().child(
        Label::new(message.into())
            .size(LabelSize::Small)
            .color(Color::Muted),
    )
}

/// The card shell: an elevated surface holding a header, one section of
/// body, and a footer of confirm + Cancel.
///
/// The confirm button is absent rather than disabled when there is
/// nothing to confirm -- a card with no candidates offers no dead
/// control to click.
pub struct PickerCard {
    id: SharedString,
    headline: SharedString,
    description: SharedString,
    width: gpui::Pixels,
    body: Option<AnyElement>,
    confirm: Option<(SharedString, SharedString)>,
    cancel_id: SharedString,
    focus_handle: Option<FocusHandle>,
}

/// So a card can be built with `.when(...)` like the elements around it.
/// gpui's blanket impl only covers `IntoElement`, and this is a builder.
impl gpui::prelude::FluentBuilder for PickerCard {}

impl PickerCard {
    pub fn new(
        id: impl Into<SharedString>,
        headline: impl Into<SharedString>,
        description: impl Into<SharedString>,
    ) -> Self {
        let id = id.into();
        let cancel_id = SharedString::from(format!("{id}-cancel"));
        PickerCard {
            id,
            headline: headline.into(),
            description: description.into(),
            width: CARD_WIDTH,
            body: None,
            confirm: None,
            cancel_id,
            focus_handle: None,
        }
    }

    pub fn width(mut self, width: gpui::Pixels) -> Self {
        self.width = width;
        self
    }

    pub fn body(mut self, body: impl IntoElement) -> Self {
        self.body = Some(body.into_any_element());
        self
    }

    /// The confirm button. Left unset, the footer carries Cancel alone.
    pub fn confirm(
        mut self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
    ) -> Self {
        self.confirm = Some((id.into(), label.into()));
        self
    }

    /// Override the Cancel button's id, which otherwise derives from the
    /// card's. Panel tests address these buttons by selector, so a card
    /// that already had a cancel id keeps it.
    pub fn cancel_id(mut self, id: impl Into<SharedString>) -> Self {
        self.cancel_id = id.into();
        self
    }

    /// Give the card its own focus handle, so Escape still lands when the
    /// picker is not rendered (nothing to search) and never took focus.
    pub fn track_focus(mut self, focus_handle: &FocusHandle) -> Self {
        self.focus_handle = Some(focus_handle.clone());
        self
    }

    pub fn render<V>(
        self,
        on_confirm: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<V>,
    ) -> gpui::Div
    where
        V: EventEmitter<DismissEvent> + 'static,
    {
        let PickerCard {
            id,
            headline,
            description,
            width,
            body,
            confirm,
            cancel_id,
            focus_handle,
        } = self;
        let cancel_selector = cancel_id.clone();
        div()
            .when_some(focus_handle, |this, handle| this.track_focus(&handle))
            .elevation_3(cx)
            .w(width)
            .child(
                Modal::new(id, None)
                    .header(
                        ModalHeader::new()
                            .headline(headline)
                            .description(description),
                    )
                    .section(Section::new().children(body))
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .children(confirm.map(|(button_id, label)| {
                                    let selector = button_id.clone();
                                    div()
                                        .debug_selector(move || selector.to_string())
                                        .child(
                                            Button::new(button_id, label).on_click(on_confirm),
                                        )
                                }))
                                .child(
                                    div()
                                        .debug_selector(move || cancel_selector.to_string())
                                        .child(
                                            Button::new(cancel_id, "Cancel").on_click(
                                                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                            ),
                                        ),
                                ),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_keeps_the_highlight_where_the_user_left_it() {
        assert_eq!(reselect_index(3, "", 10), 3);
    }

    #[test]
    fn an_empty_query_clamps_a_highlight_past_the_end() {
        assert_eq!(reselect_index(9, "", 4), 3);
    }

    #[test]
    fn an_empty_query_against_no_matches_lands_on_row_zero() {
        assert_eq!(reselect_index(9, "", 0), 0);
    }

    #[test]
    fn a_typed_query_takes_the_top_surviving_row() {
        assert_eq!(reselect_index(7, "hero", 3), 0);
    }
}
