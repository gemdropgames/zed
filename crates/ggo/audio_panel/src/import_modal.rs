//! The Import card: the question "where does this bake land?" as a
//! centred workspace modal rather than a 280px editor wedged into the
//! transport row.
//!
//! **Why it left the transport.** The target path is asked once per
//! import and answered by typing, while the transport is a row of
//! always-live controls the user leaves alone. A fixed-width field in
//! there could not shrink with the pane, pushed the Import button off a
//! narrow tab, and gave the bake readout nowhere to sit next to the path
//! it describes. A card answers the question over the window and hands
//! the transport back its whole row.
//!
//! **The panel still owns the field and the write.** This view holds no
//! state: it renders [`AudioPanel`]'s own `import_target` editor and its
//! bake readout, and confirm calls [`AudioPanel::import_impl`] -- the
//! same overwrite confirm and the same daemon write the transport button
//! used to reach directly.

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Styled, Subscription, Window, div, px,
};
use ui::prelude::*;
use ui::{Modal, ModalFooter, ModalHeader, Section};
use workspace::ModalView;

use crate::AudioPanel;

/// How wide the card is: the target path is the widest thing in it, and
/// a bare `assets/<nested>/<name>.adp` wants more than the dock-sized
/// 480px the generate card uses.
const CARD_WIDTH: gpui::Pixels = px(560.);

pub(crate) struct ImportModal {
    panel: Entity<AudioPanel>,
    _redraw: Subscription,
}

impl ImportModal {
    pub(crate) fn new(panel: Entity<AudioPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            // The readout and the Import button's enabled state are the
            // PANEL's (a bake landing while the card is up enables it),
            // so a panel-side change is what this view redraws on.
            _redraw: cx.observe(&panel, |_, _, cx| cx.notify()),
            panel,
        }
    }

    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.import_impl(window, cx));
        cx.emit(DismissEvent);
    }
}

impl Focusable for ImportModal {
    /// The target field, so the card opens ready to be typed into --
    /// unconditionally, because the card can be raised from the project
    /// panel before the file has finished decoding, and a fallback
    /// handle there would leave the field unfocused for good (the modal
    /// layer focuses once, on open).
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.read(cx).import_target.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for ImportModal {}

impl ModalView for ImportModal {}

impl Render for ImportModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel.read(cx);
        let field = panel.import_target.clone();
        let source = panel.open_rel().unwrap_or_default();
        let readout = panel.readout();
        let can_import = panel.can_import();
        let error = panel.import_error();
        let border = cx.theme().colors().border_variant;
        let background = cx.theme().colors().editor_background;

        div()
            .debug_selector(|| "ggo-audio-import-card".into())
            // Enter in a single-line editor resolves to `menu::Confirm`
            // (nothing in the `Editor` context binds it), and the editor
            // registers no handler for it, so it arrives here.
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.import(window, cx)))
            // Escape inside a focused editor resolves to `editor::Cancel`
            // (the `Editor` context outranks the context-less
            // `menu::Cancel` binding), and a single-line editor with
            // nothing to dismiss propagates it -- so this, not
            // `menu::Cancel`, is the action that actually arrives here.
            .on_action(cx.listener(|_, _: &editor::actions::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .elevation_3(cx)
            .w(CARD_WIDTH)
            .child(
                Modal::new("ggo-audio-import-modal", None)
                    .header(
                        ModalHeader::new()
                            .headline("Import as .adp")
                            .description(format!("from {source}")),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new("Target")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "ggo-audio-import-target".into())
                                        .w_full()
                                        .min_w_0()
                                        .px_1()
                                        .border_1()
                                        .border_color(border)
                                        .rounded_sm()
                                        .bg(background)
                                        .child(field),
                                )
                                // What the import is about to write, in
                                // the panel's own words: blocks, bytes
                                // and the share of the sample region.
                                .child(
                                    div()
                                        .debug_selector(|| "ggo-audio-import-preview".into())
                                        .child(
                                            Label::new(readout)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                )
                                .children(error.map(|error| {
                                    ggo_common::CopyableText::new(
                                        "ggo-audio-import-error-copy",
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
                                .child(
                                    div()
                                        .debug_selector(|| "ggo-audio-import-confirm".into())
                                        .child(
                                            Button::new("ggo-audio-import-confirm", "Import")
                                                .disabled(!can_import)
                                                .tooltip(ui::Tooltip::text(if can_import {
                                                    "Write the .adp"
                                                } else {
                                                    "The bake has not landed yet"
                                                }))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.import(window, cx)
                                                })),
                                        ),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "ggo-audio-import-cancel".into())
                                        .child(
                                            Button::new("ggo-audio-import-cancel", "Cancel")
                                                .on_click(cx.listener(|_, _, _, cx| {
                                                    cx.emit(DismissEvent)
                                                })),
                                        ),
                                ),
                        ),
                    ),
            )
    }
}
