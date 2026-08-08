use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, actions, div,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::item::Item;

actions!(
    ggo,
    [
        /// Opens the GGO hello pane (fork-wiring smoke check).
        OpenHello
    ]
);

pub fn tab_title() -> &'static str {
    "GGO Hello"
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, _cx| {
        let Some(_window) = window else {
            return;
        };
        workspace.register_action(|workspace, _: &OpenHello, window, cx| {
            let view = cx.new(|cx| HelloView {
                focus_handle: cx.focus_handle(),
            });
            workspace.active_pane().update(cx, |pane, cx| {
                pane.add_item(Box::new(view), true, true, None, window, cx)
            });
        });
    })
    .detach();
}

pub struct HelloView {
    focus_handle: FocusHandle,
}

impl Render for HelloView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(cx.theme().colors().editor_background)
            .child("GGO pane wiring works")
    }
}

impl Focusable for HelloView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for HelloView {}

impl Item for HelloView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        tab_title().into()
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_title_names_the_pane() {
        assert_eq!(tab_title(), "GGO Hello");
    }
}
