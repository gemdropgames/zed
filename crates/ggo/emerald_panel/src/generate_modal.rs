//! The generate form's host: a centred workspace modal rather than the
//! emerald dock.
//!
//! **Why it moved out of the dock.** The form is a transient question
//! ("what is this component called?"), and the dock is a persistent
//! browser over the three manifests. Rendering the form inside the dock
//! meant a right-click had to REVEAL the dock -- stealing the active
//! panel slot and the focus from whatever the user was looking at -- and
//! then the form competed with the browser and the run transcript for a
//! 420px-wide column. A modal answers the question over the whole window
//! and leaves the dock exactly as it was.
//!
//! **The panel still owns the form.** This view holds no form state: it
//! renders [`EmeraldPanel::render_generate_form`], whose `cx.listener`s
//! are bound to the PANEL entity, so every button in there keeps driving
//! the panel from inside this view's element tree. That is also why the
//! two directions of closing have to be wired to each other: the panel
//! closing its form dismisses this modal ([`EmeraldPanelEvent`]), and
//! this modal being dismissed cancels the panel's form
//! ([`GenerateModal::on_before_dismiss`]).

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Styled, Subscription, Window, div, px,
};
use ui::prelude::*;
use ui::{Modal, ModalHeader, Section};
use workspace::{DismissDecision, ModalView};

use crate::{EmeraldPanel, EmeraldPanelEvent, KEY_CONTEXT, PanelForm, Submit};

/// How wide the card is. Wider than the dock's [`crate::DEFAULT_WIDTH`]
/// because the field rows (name + kind dropdown + extension) are the
/// widest thing here and the column no longer has to fit a dock.
const CARD_WIDTH: gpui::Pixels = px(480.);

/// How tall the scrolling body is allowed to get, as a fraction of the
/// window. Half rather than the usual 70%: the modal layer parks the card
/// 80px from the top of the window, so a taller body would push the
/// card's bottom edge off a short window instead of scrolling.
const BODY_MAX_VH: f32 = 0.5;

pub struct GenerateModal {
    panel: Entity<EmeraldPanel>,
    /// Answered by [`Focusable`] when no form is open. The modal layer
    /// focuses whatever `focus_handle` returns the instant it opens the
    /// modal, so it must always have something real to hand back.
    fallback_focus: FocusHandle,
    _closed: Subscription,
    _redraw: Subscription,
}

impl GenerateModal {
    pub fn new(panel: Entity<EmeraldPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            fallback_focus: cx.focus_handle(),
            _closed: cx.subscribe(&panel, |_, _, event, cx| match event {
                EmeraldPanelEvent::FormClosed => cx.emit(DismissEvent),
            }),
            // The form lives in the panel, so a panel-side change (a
            // field row added, a kind switched, a run finishing) is what
            // this view has to redraw on -- its own state never changes.
            _redraw: cx.observe(&panel, |_, _, cx| cx.notify()),
            panel,
        }
    }
}

impl Focusable for GenerateModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.panel.read(cx).form {
            // The name editor, so the modal opens ready to be typed into.
            Some(PanelForm::Generate(form)) => form.name.read(cx).focus_handle(cx),
            None => self.fallback_focus.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for GenerateModal {}

impl ModalView for GenerateModal {
    /// Escape and a click outside reach the PANEL, not just the layer:
    /// the form is panel state, and a dismissal that left it set would
    /// leave the panel believing a form is open with nothing on screen
    /// -- and the next right-click would then toggle this modal shut
    /// instead of opening it.
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.panel.update(cx, |panel, cx| panel.cancel_form(cx));
        DismissDecision::Dismiss(true)
    }
}

impl Render for GenerateModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(kind) = self.panel.read(cx).form_kind() else {
            return div().into_any_element();
        };
        let form = self
            .panel
            .update(cx, |panel, cx| panel.render_generate_form(window, cx));
        // The run the form started reports HERE while the form is still
        // open: a failed `emd` keeps the form, and its message has to be
        // readable next to the name that caused it rather than behind
        // the modal in a dock the user may never have opened.
        let run_state = self.panel.read(cx).render_run_state(false);

        div()
            .debug_selector(|| "ggo-emerald-modal".into())
            // Matches the `GgoEmeraldPanel > Editor` keymap block, which
            // is what binds Enter in the form's editors to `Submit`. The
            // form's editors are no longer inside the panel's own element
            // tree, so without this the binding would not match.
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &Submit, window, cx| {
                this.panel.update(cx, |panel, cx| panel.submit(window, cx));
            }))
            // Escape inside a focused editor resolves to `editor::Cancel`
            // (the `Editor` context outranks the context-less
            // `menu::Cancel` binding), and a single-line editor with
            // nothing to dismiss propagates it -- so this, not
            // `menu::Cancel`, is the action that actually arrives here.
            .on_action(cx.listener(|this, _: &editor::actions::Cancel, _, cx| {
                this.panel.update(cx, |panel, cx| panel.cancel_form(cx));
            }))
            .elevation_3(cx)
            .w(CARD_WIDTH)
            .child(
                Modal::new("ggo-emerald-generate", None)
                    .header(ModalHeader::new().headline(format!("New {}", kind.noun())))
                    .section(
                        Section::new().child(
                            v_flex()
                                .id("ggo-emerald-modal-body")
                                .max_h(vh(BODY_MAX_VH, window))
                                .min_h_0()
                                .overflow_y_scroll()
                                .child(form)
                                .children(run_state),
                        ),
                    ),
            )
            .into_any_element()
    }
}
