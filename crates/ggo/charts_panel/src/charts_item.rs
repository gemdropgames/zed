//! The reports/charts view as a center-pane tab: a workspace [`Item`]
//! wrapping the ONE [`ChartsPanel`] entity, so hopping from a finished
//! emulator run to its report stays in the center area with real screen
//! space (the dock could never give a chart more than a sidebar's
//! width). Singleton by construction -- [`crate::open_charts_item`]
//! activates the existing tab rather than opening a second reports view.
//! The panel type keeps ALL loading/chart logic; this file only adapts
//! it to the workspace's tab machinery. No dirty/save: reports are
//! read-only views over the runs database.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, WeakEntity, Window,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::ChartsPanel;

pub enum ChartsItemEvent {
    UpdateTab,
}

pub struct ChartsItem {
    panel: Entity<ChartsPanel>,
}

impl ChartsItem {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let panel = cx.new(|cx| ChartsPanel::new(Some(workspace), cx));
        cx.observe(&panel, |_, _, cx| cx.emit(ChartsItemEvent::UpdateTab))
            .detach();
        Self { panel }
    }

    pub fn panel(&self) -> &Entity<ChartsPanel> {
        &self.panel
    }
}

impl EventEmitter<ChartsItemEvent> for ChartsItem {}

impl Focusable for ChartsItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Render for ChartsItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for ChartsItem {
    type Event = ChartsItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            ChartsItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Reports".into()
    }
}
