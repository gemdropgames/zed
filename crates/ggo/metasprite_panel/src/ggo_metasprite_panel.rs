//! GGO MetaSprite panel: dock-panel skeleton for the metasprite editor
//! (F2 task M3). Structural mirror of `ggo_world_panel`'s panel shell --
//! `Panel` impl, self-registration via `cx.observe_new`, and the
//! keybinding-reload observer pattern -- with a placeholder body; the
//! frame/hitbox editing UI lands in later F2 tasks.

use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Pixels,
    Render, Styled, Window, actions, div, px,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(
    ggo_metasprite,
    [
        /// Toggles focus on the GGO metasprite panel.
        ToggleFocus
    ]
);

const GGO_METASPRITE_PANEL_KEY: &str = "GGOMetaSpritePanel";

/// The panel's key-dispatch context (`.key_context`) -- scoped now, ready
/// for the field/canvas keybindings a later task adds, the same shape as
/// `ggo_world_panel::KEY_CONTEXT`.
const KEY_CONTEXT: &str = "GgoMetaSpritePanel";

/// Fixed default width until the panel grows real settings persistence.
const DEFAULT_WIDTH: Pixels = px(360.);

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as `ggo_world_panel::init`: `zed::reload_keymaps` clears
    // and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), and keymap assets are upstream files
    // this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` keeps any bindings added here alive across
    // reloads. `bind_panel_keys` binds nothing yet -- the skeleton's only
    // action, `ToggleFocus`, is dispatched via `Panel::toggle_action` /
    // the command palette, not a `KeyBinding` (matching how
    // `ggo_world::ToggleFocus` also ships unbound) -- but the reload
    // scaffold is wired now so a later task can add real `cx.bind_keys`
    // calls here without re-deriving this shape.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let panel = cx.new(MetaSpritePanel::new);
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<MetaSpritePanel>(window, cx);
        });
    })
    .detach();
}

/// No keybindings yet -- see the comment in `init`.
fn bind_panel_keys(_cx: &mut App) {}

pub struct MetaSpritePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
}

impl MetaSpritePanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
        }
    }
}

impl Render for MetaSpritePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .flex()
            .justify_center()
            .items_center()
            .bg(cx.theme().colors().panel_background)
            .child(Label::new("GGO MetaSprite Panel").color(Color::Muted))
    }
}

impl Focusable for MetaSpritePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for MetaSpritePanel {}

impl Panel for MetaSpritePanel {
    fn persistent_name() -> &'static str {
        "GGO MetaSprite"
    }

    fn panel_key() -> &'static str {
        GGO_METASPRITE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call as `ggo_world_panel`: no settings persistence yet, and
        // Bottom isn't a sensible spot for a sprite/frame editor sidebar.
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        DEFAULT_WIDTH
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        // `IconName` has no Film/animation glyph in this fork; `Image`
        // reads closest to a sprite/frame asset (checked against
        // crates/icons/src/icons.rs -- Sparkle was the other candidate but
        // reads as "AI/magic", not sprites).
        Some(IconName::Image)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO MetaSprite")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8 (grep activation_priority across
        // crates/).
        9
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock and focuses the
    /// panel. Goes through `MultiWorkspace::test_new` rather than a bare
    /// `Workspace::test_new`, because `register_action` handlers (like
    /// `ToggleFocus`) are only mounted into the dispatch tree once
    /// something renders `Workspace::actions`, which in production is
    /// `MultiWorkspace`'s render (same lesson as `ggo_world_panel`'s test).
    #[gpui::test]
    async fn test_toggle_focus_opens_panel(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.panel::<MetaSpritePanel>(cx).is_some(),
                "MetaSpritePanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<MetaSpritePanel>(cx)
                .expect("MetaSpritePanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }
}
