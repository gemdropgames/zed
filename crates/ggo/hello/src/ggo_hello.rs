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
        if window.is_none() {
            return;
        }
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
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    #[test]
    fn tab_title_names_the_pane() {
        assert_eq!(tab_title(), "GGO Hello");
    }

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        // Smaller companion assertion: init() must not panic when run in a
        // bare test App, independent of whether a workspace ever observes it.
        init(cx);
    }

    /// Proves the OpenHello action wiring end-to-end: dispatches the action
    /// through gpui's real action-dispatch machinery (not a direct method
    /// call) against a live Workspace, and asserts the resulting pane item's
    /// tab text. This is the action-dispatch variant called for in the
    /// review; the fallback (direct HelloView construction) wasn't needed.
    #[gpui::test]
    async fn test_open_hello_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        // `Workspace` only wires up `register_action`-installed handlers
        // (like `OpenHello`) when something calls `Workspace::actions`, which
        // in production is `MultiWorkspace`'s render. Rendering a bare
        // `Workspace` as the window root (as `Workspace::test_new` alone
        // would) never mounts those listeners into the dispatch tree, so the
        // action silently no-ops. Go through `MultiWorkspace::test_new`
        // instead to match how the app actually renders workspaces.
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.dispatch_action(OpenHello);

        workspace.update(cx, |workspace, cx| {
            let active_item = workspace
                .active_pane()
                .read(cx)
                .active_item()
                .expect("OpenHello should have added an item to the active pane");
            assert_eq!(
                active_item.tab_content_text(0, cx),
                SharedString::from(tab_title())
            );
        });
    }
}
