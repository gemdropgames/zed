//! The import wizard as a center-pane tab: one [`ImportItem`] per
//! workspace (the emulator's shape), wrapping the one [`ImportPanel`].
//! A pending import is not a document, so the tab is never dirty and
//! never saves.

use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, WeakEntity, Window};
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::{ImportPanel, ViewerState};

pub enum ImportItemEvent {
    UpdateTab,
}

pub struct ImportItem {
    panel: Entity<ImportPanel>,
}

impl ImportItem {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let panel = cx.new(|cx| ImportPanel::new(Some(workspace), cx));
        cx.observe(&panel, |_, _, cx| cx.emit(ImportItemEvent::UpdateTab))
            .detach();
        Self { panel }
    }

    pub fn panel(&self) -> &Entity<ImportPanel> {
        &self.panel
    }
}

impl EventEmitter<ImportItemEvent> for ImportItem {}

impl Focusable for ImportItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}

impl Render for ImportItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.clone())
    }
}

impl Item for ImportItem {
    type Event = ImportItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            ImportItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        let source = match &self.panel.read(cx).state {
            ViewerState::Ready(open) => open.source_rel.rsplit('/').next().map(str::to_string),
            ViewerState::Loading { rel_path } => rel_path.rsplit('/').next().map(str::to_string),
            _ => None,
        };
        match source {
            Some(name) if !name.is_empty() => format!("Import · {name}").into(),
            _ => "Import".into(),
        }
    }
}

/// Activate the workspace's import tab, or open it, then run `f` on its
/// panel. Runs inside the workspace update: `f` must not read the
/// workspace back (use `adopt_root`, not `refresh_root`).
pub fn open_import_item(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut ImportPanel, &mut Window, &mut Context<ImportPanel>),
) -> Entity<ImportPanel> {
    let existing = workspace.items_of_type::<ImportItem>(cx).next();
    let item = match existing {
        Some(item) => {
            workspace.activate_item(&item, true, true, window, cx);
            item
        }
        None => {
            let weak = workspace.weak_handle();
            let item = cx.new(|cx| ImportItem::new(weak, cx));
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            item
        }
    };
    let panel = item.read(cx).panel().clone();
    panel.update(cx, |panel, cx| f(panel, window, cx));
    panel
}
