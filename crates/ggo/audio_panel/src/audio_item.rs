//! One center-pane tab per audio file: a workspace [`Item`] wrapping its
//! own [`AudioPanel`] -- the tileset tab's shape, minus dirty/save, because
//! the tab holds no document. Import writes a NEW file (the `.adp`) and
//! opens it in its own tab.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, WeakEntity, Window,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::AudioPanel;

pub enum AudioItemEvent {
    UpdateTab,
}

pub struct AudioItem {
    panel: Entity<AudioPanel>,
    rel: String,
}

impl AudioItem {
    pub fn new(
        rel: String,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| AudioPanel::new(Some(workspace), window, cx));
        Self::wrap(rel, panel, window, cx)
    }

    /// [`Self::new`] against a bare filesystem root instead of a live
    /// workspace -- the tests' `root_override` hook.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        rel: String,
        root: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.new(|cx| {
            let mut panel = AudioPanel::new(None, window, cx);
            panel.root_override = Some(root);
            panel
        });
        Self::wrap(rel, panel, window, cx)
    }

    fn wrap(
        rel: String,
        panel: Entity<AudioPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        panel.update(cx, |panel, cx| panel.open_rel_path(&rel, window, cx));
        cx.observe(&panel, |_, _, cx| cx.emit(AudioItemEvent::UpdateTab))
            .detach();
        Self { panel, rel }
    }

    pub fn rel(&self) -> &str {
        &self.rel
    }

    #[cfg(test)]
    pub(crate) fn panel(&self) -> &Entity<AudioPanel> {
        &self.panel
    }

    /// The wrapped panel, for the cross-crate journeys in `ggo_smoke` --
    /// the tab is how a `.wav` opens, so the panel is only reachable
    /// through the item.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_panel(&self) -> Entity<AudioPanel> {
        self.panel.clone()
    }
}

impl EventEmitter<AudioItemEvent> for AudioItem {}

impl Focusable for AudioItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Render for AudioItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for AudioItem {
    type Event = AudioItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            AudioItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        std::path::Path::new(&self.rel)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Audio".to_string())
            .into()
    }
}
