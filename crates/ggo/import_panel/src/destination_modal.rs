//! The destination form's host: a centred workspace modal rather than a
//! strip along the bottom of the import tab.
//!
//! **Why it moved out of the footer.** The destination is a transient
//! question ("where does this land, and under what name?"), while the
//! tab itself is a viewer over the source art -- the crop canvas, the
//! quantized preview band and the palette. Six controls and a commit
//! button nailed under that viewer took a fixed slice of every pane
//! height whether or not anyone was about to answer them, and in a short
//! pane they were the chrome that pushed the canvas onto its floor. A
//! card answers the question over the whole window and gives the viewer
//! its height back.
//!
//! **The panel still owns the form.** This view holds no form state: it
//! renders [`ImportPanel::render_destination_form`], whose `cx.listener`s
//! and weak-entity callbacks are bound to the PANEL entity, so every
//! control in there keeps driving the panel from inside this view's
//! element tree. That is also why the two directions of closing have to
//! be wired to each other: the panel closing its form dismisses this
//! modal ([`ImportPanelEvent`]), and this modal being dismissed cancels
//! the panel's form ([`DestinationModal::on_before_dismiss`]).

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, px,
};
use ui::prelude::*;
use ui::{Modal, ModalHeader, Section};
use workspace::{DismissDecision, ModalView};

use crate::{ClearCrop, Import, ImportPanel, ImportPanelEvent, KEY_CONTEXT};

/// How wide the card is: the Dir/Name row plus the written-file list are
/// the widest things in it, and both read better on one line than
/// wrapped.
const CARD_WIDTH: gpui::Pixels = px(440.);

/// How tall the scrolling body may get, as a fraction of the window --
/// the same half `ggo_emerald_panel`'s card uses, because the modal layer
/// parks the card 80px down and a taller body pushes its bottom edge off
/// a short window instead of scrolling.
const BODY_MAX_VH: f32 = 0.5;

pub struct DestinationModal {
    panel: Entity<ImportPanel>,
    /// Answered by [`Focusable`] when the panel has no fields to focus.
    /// The modal layer focuses whatever `focus_handle` returns the
    /// instant it opens the modal, so it must always have something real
    /// to hand back.
    fallback_focus: FocusHandle,
    _closed: Subscription,
    _redraw: Subscription,
}

impl DestinationModal {
    pub fn new(panel: Entity<ImportPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            fallback_focus: cx.focus_handle(),
            _closed: cx.subscribe(&panel, |_, _, event, cx| match event {
                ImportPanelEvent::FormClosed => cx.emit(DismissEvent),
            }),
            // The form lives in the panel, so a panel-side change (a
            // toggle flipped, a target list that moved because the dir
            // was retyped, a refused commit's status) is what this view
            // has to redraw on -- its own state never changes.
            _redraw: cx.observe(&panel, |_, _, cx| cx.notify()),
            panel,
        }
    }
}

impl Focusable for DestinationModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel
            .read(cx)
            .form_focus_handle(cx)
            .unwrap_or_else(|| self.fallback_focus.clone())
    }
}

impl EventEmitter<DismissEvent> for DestinationModal {}

impl ModalView for DestinationModal {
    /// A click outside reaches the PANEL, not just the layer: the open
    /// form is panel state, and a dismissal that left it set would leave
    /// the panel believing a card is up with nothing on screen -- and the
    /// next "Import…" click would then find the form already open.
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.panel.update(cx, |panel, cx| panel.cancel_form(cx));
        DismissDecision::Dismiss(true)
    }
}

impl Render for DestinationModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self
            .panel
            .update(cx, |panel, cx| panel.render_destination_form(window, cx));

        div()
            .debug_selector(|| "ggo-import-card".into())
            // Matches the `GgoImportPanel` keymap block, which is what
            // binds Enter to `Import`. The form's editors are no longer
            // inside the panel's own element tree, so without this the
            // binding would not match.
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &Import, window, cx| {
                this.panel
                    .update(cx, |panel, cx| panel.import_impl(window, cx));
            }))
            // Escape inside a focused editor resolves to `editor::Cancel`
            // (the `Editor` context outranks the context-less
            // `menu::Cancel` binding), and a single-line editor with
            // nothing to dismiss propagates it -- so this, not
            // `menu::Cancel`, is the action that actually arrives here.
            .on_action(cx.listener(|this, _: &editor::actions::Cancel, _, cx| {
                this.panel.update(cx, |panel, cx| panel.cancel_form(cx));
            }))
            // Escape with focus anywhere but an editor resolves to the
            // panel's own `ClearCrop`, which this card's key context is
            // what puts in reach. Inside the card there is no crop to
            // clear and Escape can only mean "cancel the question".
            .on_action(cx.listener(|this, _: &ClearCrop, _, cx| {
                this.panel.update(cx, |panel, cx| panel.cancel_form(cx));
            }))
            .elevation_3(cx)
            .w(CARD_WIDTH)
            .child(
                Modal::new("ggo-import-destination", None)
                    .header(ModalHeader::new().headline("Import"))
                    .section(
                        Section::new().child(
                            v_flex()
                                .id("ggo-import-card-body")
                                .max_h(vh(BODY_MAX_VH, window))
                                .min_h_0()
                                .overflow_y_scroll()
                                .child(form),
                        ),
                    ),
            )
    }
}
