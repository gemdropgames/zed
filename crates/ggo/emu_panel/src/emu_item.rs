//! The emulator as a center-pane tab: a workspace [`Item`] wrapping the
//! ONE [`EmuPanel`] entity, so a run takes over the center area and the
//! screen scales up to it (the dock could never give a 320x240 frame
//! more than a sidebar's width). Singleton by construction --
//! [`crate::open_emu_item`] activates the existing tab rather than
//! opening a second emulator. The panel type keeps ALL emulation logic;
//! this file only adapts it to the workspace's tab machinery. No
//! dirty/save: the emulator holds no document.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, WeakEntity, Window,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::EmuPanel;

pub enum EmuItemEvent {
    UpdateTab,
}

pub struct EmulatorItem {
    panel: Entity<EmuPanel>,
}

impl EmulatorItem {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| EmuPanel::new(Some(workspace), Some(window), cx));
        cx.observe(&panel, |_, _, cx| cx.emit(EmuItemEvent::UpdateTab))
            .detach();
        Self { panel }
    }

    pub fn panel(&self) -> &Entity<EmuPanel> {
        &self.panel
    }

    /// The wrapped panel, for the cross-crate journeys in `ggo_smoke` --
    /// the tab is how a `.cart` opens, so the panel is only reachable
    /// through the item.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_panel(&self) -> Entity<EmuPanel> {
        self.panel.clone()
    }
}

impl EventEmitter<EmuItemEvent> for EmulatorItem {}

impl Focusable for EmulatorItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Render for EmulatorItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for EmulatorItem {
    type Event = EmuItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            EmuItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    /// Another tab took the pane's place: pause the cart rather than let
    /// it run unseen. The panel resumes on its next render, which only
    /// happens once the tab is visible again.
    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.panel.update(cx, |panel, cx| panel.auto_pause(cx));
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        // The selected cart's stem names the tab once one is chosen; a
        // bare emulator reads as just "Emulator".
        self.panel
            .read(cx)
            .selected_cart_stem()
            .map(|stem| format!("Emulator · {stem}").into())
            .unwrap_or_else(|| "Emulator".into())
    }
}
