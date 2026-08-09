//! GGO Emulator panel (F3 task E1): an embedded `ggo-emu` -- cart picker,
//! Run/Stop, live 320x240 video, keyboard -> pad input. The emulation
//! itself is `ggo-emu-core` verbatim; [`drive`] ports the standalone
//! binary's drive loop (`ggo-emu/src/lib.rs::run_cart` +
//! `src/native.rs`) onto a background thread, and this module is the gpui
//! shell around it.
//!
//! Structural mirror of `ggo_world_panel`/`ggo_metasprite_panel`/
//! `ggo_charts_panel`: `Panel` impl, `ToggleFocus`, `observe_new`
//! registration into every new workspace, a `KeymapEventChannel` observer
//! that re-binds the panel's keys on every keymap reload, project-root
//! discovery off the workspace's first visible worktree with a
//! `root_override` test hook, and off-thread loading behind a
//! load-generation staleness guard.
//!
//! Audio is explicitly out of scope for F3 (constraints.md) -- see
//! [`drive`]'s module doc for exactly what else is not ported.
//!
//! # The atlas-release contract (the load-bearing part)
//!
//! gpui NEVER frees a `RenderImage`'s GPU atlas tiles on its own. Every
//! `RenderImage::new` takes a fresh process-global `ImageId`
//! (`gpui/src/assets.rs:59-68`), `img(..)` on an `ImageSource::Render`
//! bypasses every image/asset cache (`gpui/src/elements/img.rs:548`) and
//! uploads straight into the window's sprite atlas keyed by that id
//! (`gpui/src/window.rs:4479`), `RenderImage` has no `Drop` impl, and no
//! atlas backend has an LRU, an eviction pass or a per-frame sweep -- the
//! only thing that ever returns a tile's rect to the allocator is an
//! explicit `PlatformAtlas::remove`, which only
//! [`Window::drop_image`](gpui::Window::drop_image) calls
//! (`gpui/src/window.rs:4577-4589`). So a 60 Hz pane that builds a new
//! `RenderImage` per frame and never calls `drop_image` leaks ~300 KB of
//! atlas per frame, forever: at 320x240 BGRA that is ~18 MB/s of atlas
//! growth, spawning fresh 1024x1024 atlas textures continuously
//! (`gpui_wgpu/src/wgpu_atlas.rs:175`) that can never drop because their
//! live-key count never returns to zero.
//!
//! [`EmuPanel::retire_atlas_frames`] implements the release path. It is
//! the livekit video view's double buffer
//! (`livekit_client/src/remote_video_track_view.rs:88-99`), NOT
//! `svg_preview_view::set_current`'s immediate replace-and-drop: the atlas
//! hands the freed rect straight back to the allocator, so a frame that a
//! just-submitted scene still references must survive one more render.
//! Frame N-2 is dropped, N-1 is retained. [`EmuPanel::release_atlas_all`]
//! covers the two teardown paths (Stop, and panel release) where no
//! further render will come.

mod carts;
mod drive;
mod input;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding,
    KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, Pixels, Render, RenderImage, Styled,
    Subscription, Task, WeakEntity, Window, actions, div, img, px,
};
use ui::prelude::*;
use ui::{ContextMenu, DropdownMenu, Tooltip};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use drive::{EmuMsg, Session};
use input::InputState;

actions!(
    ggo_emu,
    [
        /// Toggles focus on the GGO emulator panel.
        ToggleFocus,
        /// Runs the selected cart in the emulator pane.
        Run,
        /// Stops the running cart.
        Stop
    ]
);

const GGO_EMU_PANEL_KEY: &str = "GGOEmuPanel";

/// The panel's key-dispatch context. Everything the pane binds is scoped
/// to it, so the pad keys and the transport bindings are inert unless the
/// pane itself has focus -- typing `z` into an editor must never reach a
/// cart. See [`bind_panel_keys`].
const KEY_CONTEXT: &str = "GgoEmuPanel";

/// Fixed default width until the panel grows real settings persistence
/// (the same call the other three GGO panels made). Wide enough for the
/// 320px screen plus the dock's padding.
const DEFAULT_WIDTH: Pixels = px(360.);

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as the other GGO panels' `init`: `zed::reload_keymaps`
    // clears and rebuilds ALL key bindings on every keymap/settings
    // change (including once at startup), and keymap assets are upstream
    // files this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` is what keeps Run/Stop alive across reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| EmuPanel::new(Some(weak_workspace), Some(window), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<EmuPanel>(window, cx);
        });
    })
    .detach();
}

/// Transport bindings only. The 18 pad keys are deliberately NOT
/// `KeyBinding`s: an action fires on press and has no release event, and
/// the pad mask is level-triggered (the cart asks "is A held right now"),
/// so they go through `on_key_down`/`on_key_up`/`on_modifiers_changed`
/// listeners on the focus-tracked root instead -- see [`EmuPanel::render`]
/// and [`input`].
///
/// `ctrl-alt-` rather than anything shorter, for two reasons. Every bare
/// letter is a pad key while the pane is focused, and a binding would
/// swallow the keystroke before the pad listener saw it; and shift is
/// SELECT, so any shift chord would latch a button as a side effect of
/// running the cart. `ToggleFocus` stays unbound, dispatched via
/// `Panel::toggle_action` / the command palette, exactly as the other
/// three GGO panels leave it.
fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-alt-r", Run, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-alt-s", Stop, Some(KEY_CONTEXT)),
    ]);
}

// ------------------------------------------------------------- view state

enum LoadState {
    /// Nothing enumerated yet -- before the panel's first activation.
    Empty,
    Loading,
    Ready(Vec<String>),
}

pub struct EmuPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery
    /// (`ggo_world_panel::root_override`'s analog).
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    state: LoadState,
    /// Index into the `LoadState::Ready` list. Held as a rel path rather
    /// than an index so a refresh that reorders or shortens the list
    /// can't silently repoint the selection at a different cart.
    selected: Option<String>,
    load_generation: u64,
    _load_task: Option<Task<()>>,
    /// The cart dropdown's menu, keyed by the `load_generation` it was
    /// built from -- see [`EmuPanel::cart_menu`].
    cart_menu: Option<(u64, Entity<ContextMenu>)>,

    /// The running emulator, if any. Dropping it signals the thread to
    /// stop (see [`Session::stop`]).
    session: Option<Session>,
    /// Pumps [`EmuMsg`]s from the emulator thread onto the UI thread.
    /// Dropped together with `session`.
    _pump_task: Option<Task<()>>,
    /// Last run's exit/error line, shown under the transport.
    status: Option<String>,
    /// The cart-visible frame number of the last frame received -- the
    /// pane's "is it actually running" readout.
    frame: u32,
    /// Latched pad mask, published into the session on every change.
    input: InputState,

    /// The frame to paint. `None` before the first frame of a run.
    latest_frame: Option<Arc<RenderImage>>,
    /// Atlas double buffer -- see the module doc. `current` is the frame
    /// the last render painted; `previous` is the one before it, which is
    /// what actually gets `drop_image`d.
    current_rendered_frame: Option<Arc<RenderImage>>,
    previous_rendered_frame: Option<Arc<RenderImage>>,
    /// Clears the pad mask when the pane loses focus, so a key held while
    /// the user clicks away doesn't stay latched forever.
    _focus_out: Option<Subscription>,
}

impl EmuPanel {
    pub fn new(
        workspace: Option<WeakEntity<Workspace>>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let _focus_out = window.map(|window| {
            cx.on_focus_out(&focus_handle, window, |this, _event, _window, cx| {
                this.release_all_buttons(cx);
            })
        });
        // Teardown: the panel's last one or two frames are still in the
        // window atlas when the panel is released, and no further render
        // will come to retire them. `on_release` is the only hook left --
        // the same one livekit's video view uses
        // (`remote_video_track_view.rs:32-44`).
        cx.on_release(|this, cx| {
            for image in [
                this.previous_rendered_frame.take(),
                this.current_rendered_frame.take(),
                this.latest_frame.take(),
            ]
            .into_iter()
            .flatten()
            {
                cx.drop_image(image, None);
            }
        })
        .detach();

        Self {
            focus_handle,
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            state: LoadState::Empty,
            selected: None,
            load_generation: 0,
            _load_task: None,
            cart_menu: None,
            session: None,
            _pump_task: None,
            status: None,
            frame: 0,
            input: InputState::default(),
            latest_frame: None,
            current_rendered_frame: None,
            previous_rendered_frame: None,
            _focus_out,
        }
    }

    /// Re-resolve the project root (first visible worktree) and
    /// re-enumerate its carts. Runs on every panel activation, the same
    /// trigger `ggo_world_panel::refresh_worlds` uses.
    fn refresh_carts(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        // The generation bump happens before the early return, not after
        // it: it is what invalidates the dropdown's menu cache, and a
        // refresh that finds no root still changes the list (to empty).
        self.load_generation += 1;
        let generation = self.load_generation;

        let Some(root) = self.project_root.clone() else {
            self.state = LoadState::Ready(Vec::new());
            self.selected = None;
            cx.notify();
            return;
        };

        self.state = LoadState::Loading;
        cx.notify();

        let load = cx.background_spawn(async move { carts::list_carts(&root) });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let found = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    // Superseded by a later refresh -- drop this stale
                    // result, same guard the other GGO panels use.
                    return;
                }
                // A selection that survived the refresh keeps pointing at
                // the same cart; one whose file vanished is dropped
                // rather than silently sliding onto a neighbour.
                if this.selected_still_present(&found).is_none() {
                    this.selected = None;
                }
                this.selected = this.selected.take().or_else(|| found.first().cloned());
                this.state = LoadState::Ready(found);
                cx.notify();
            })
            .ok();
        }));
    }

    /// `self.selected` if it is still in `found`. Split out only so
    /// `refresh_carts`'s closure reads as one thought.
    fn selected_still_present<'a>(&'a self, found: &[String]) -> Option<&'a String> {
        let selected = self.selected.as_ref()?;
        found.iter().any(|c| c == selected).then_some(selected)
    }

    fn select_cart(&mut self, cart: String, cx: &mut Context<Self>) {
        self.selected = Some(cart);
        cx.notify();
    }

    /// Start the selected cart. A run already in flight is stopped first,
    /// so Run is idempotent-ish (restart) rather than a way to end up
    /// with two emulator threads fighting over one pane.
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(cart)) = (self.project_root.clone(), self.selected.clone()) else {
            self.status = Some("no cart selected".to_string());
            cx.notify();
            return;
        };
        self.stop(window, cx);

        let (session, rx) = drive::start(root.join(&cart), cart);
        self.session = Some(session);
        self.status = None;
        self.frame = 0;
        self._pump_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.on_emu_msg(msg, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn on_emu_msg(&mut self, msg: EmuMsg, cx: &mut Context<Self>) {
        match msg {
            EmuMsg::Frame { bgra, frame } => {
                self.frame = frame;
                // `from_raw` takes the Vec by value: the emulator thread
                // already produced BGRA, so this is a move, not a copy.
                let Some(buffer) = image::ImageBuffer::from_raw(drive::WIDTH, drive::HEIGHT, bgra)
                else {
                    return;
                };
                self.latest_frame =
                    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])));
            }
            EmuMsg::Ended(reason) => {
                self.status = Some(reason);
                // The thread is already gone; just drop the handles so
                // the transport flips back to Run. The last frame stays
                // on screen (and stays retired normally by
                // `retire_atlas_frames`) -- a run that ends shouldn't
                // blank the pane.
                self.session = None;
                self._pump_task = None;
                self.input.clear();
            }
        }
        cx.notify();
    }

    /// Tear the run down: signal the thread (which drops the core on its
    /// way out), drop the pump task, release the pad, and hand every
    /// atlas tile the pane still owns back to the window -- no further
    /// render will come to retire them through the double buffer.
    fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() && self.latest_frame.is_none() {
            return;
        }
        self.session = None;
        self._pump_task = None;
        self.input.clear();
        self.release_atlas_all(window);
        cx.notify();
    }

    fn is_running(&self) -> bool {
        self.session.is_some()
    }

    // ----------------------------------------------------------- input

    /// Publish the latched mask into the running session. A no-op when
    /// nothing is running, so key handling stays unconditional.
    fn publish_input(&self) {
        if let Some(session) = &self.session {
            session.set_input(self.input.mask());
        }
    }

    fn on_key(&mut self, key: &str, down: bool) {
        if self.input.key(key, down) {
            self.publish_input();
        }
    }

    fn on_shift(&mut self, held: bool) {
        if self.input.set_select(held) {
            self.publish_input();
        }
    }

    fn release_all_buttons(&mut self, _cx: &mut Context<Self>) {
        if self.input.clear() {
            self.publish_input();
        }
    }

    // ----------------------------------------------------------- atlas

    /// The per-render half of the release contract (see the module doc):
    /// retire frame N-2's atlas tiles, keep N-1's alive one more render.
    /// Copied from `livekit_client`'s `RemoteVideoTrackView::render`,
    /// including the `id` guard -- a run that ends leaves the same image
    /// as both current and latest, and dropping it would blank the pane.
    fn retire_atlas_frames(&mut self, window: &mut Window) {
        let Some(latest) = self.latest_frame.clone() else {
            return;
        };
        if let Some(current) = self.current_rendered_frame.take() {
            if let Some(previous) = self.previous_rendered_frame.take()
                && previous.id != current.id
            {
                window.drop_image(previous).log_err();
            }
            self.previous_rendered_frame = Some(current);
        }
        self.current_rendered_frame = Some(latest);
    }

    /// The teardown half: every tile this pane still owns, at once.
    /// `drop_image` on a key that is already gone is a documented no-op
    /// (`gpui_wgpu/src/wgpu_atlas.rs:133`), so the overlap between the
    /// three slots is harmless.
    fn release_atlas_all(&mut self, window: &mut Window) {
        for image in [
            self.previous_rendered_frame.take(),
            self.current_rendered_frame.take(),
            self.latest_frame.take(),
        ]
        .into_iter()
        .flatten()
        {
            window.drop_image(image).log_err();
        }
    }

    // ---------------------------------------------------------- render

    /// The cart dropdown's menu entity, rebuilt only when the enumeration
    /// changes.
    ///
    /// This cache is not a micro-optimisation. `render` runs on every
    /// `cx.notify`, and a running cart notifies 60 times a second, so
    /// building the menu inline (which is what the other GGO panels do --
    /// they only re-render on interaction) would allocate a fresh gpui
    /// entity plus one boxed closure per cart, sixty times a second, for
    /// a widget that is disabled while the cart runs.
    fn cart_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        if let Some((generation, menu)) = &self.cart_menu
            && *generation == self.load_generation
        {
            return menu.clone();
        }
        let found = match &self.state {
            LoadState::Ready(carts) => carts.clone(),
            _ => Vec::new(),
        };
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for cart in found {
                let weak = weak.clone();
                menu = menu.entry(
                    SharedString::from(cart.clone()),
                    None,
                    move |_window, cx| {
                        weak.update(cx, |this, cx| this.select_cart(cart.clone(), cx))
                            .ok();
                    },
                );
            }
            menu
        });
        self.cart_menu = Some((self.load_generation, menu.clone()));
        menu
    }

    fn render_transport(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let running = self.is_running();
        let label: SharedString = match (&self.state, &self.selected) {
            (_, Some(cart)) => cart.clone().into(),
            (LoadState::Loading, _) => "Scanning…".into(),
            (LoadState::Empty, _) => "Select the panel".into(),
            (LoadState::Ready(_), None) => "No .cart files".into(),
        };
        let menu = self.cart_menu(window, cx);

        h_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("ggo-emu-run", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .disabled(self.selected.is_none())
                    .tooltip(Tooltip::text("Run cart"))
                    .on_click(cx.listener(|this, _event, window, cx| this.run(window, cx))),
            )
            .child(
                IconButton::new("ggo-emu-stop", IconName::Stop)
                    .icon_size(IconSize::Small)
                    .disabled(!running)
                    .tooltip(Tooltip::text("Stop"))
                    .on_click(cx.listener(|this, _event, window, cx| this.stop(window, cx))),
            )
            .child(DropdownMenu::new("ggo-emu-cart", label, menu).disabled(running))
            .child(div().flex_1())
            // The "is it actually running" readout: the cart's own frame
            // counter, straight off the last `EmuMsg::Frame`.
            .children(self.session.as_ref().map(|session| {
                Label::new(format!("{} · frame {}", session.cart, self.frame))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .into_any_element()
    }

    /// The pane itself: the framebuffer at whatever size the dock gives
    /// it, or a message. `w_full`/`h_full` on the img rather than a fixed
    /// 320x240 -- gpui scales the one image, which is what the standalone
    /// binary's `--scale` does with `scale_nearest`.
    fn render_screen(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(frame) = &self.latest_frame {
            return div()
                .size_full()
                .flex()
                .justify_center()
                .items_center()
                .bg(gpui::black())
                .child(img(frame.clone()).w_full().h_full())
                .into_any_element();
        }
        let message = match (&self.state, self.is_running(), &self.selected) {
            (_, true, _) => "Starting…".to_string(),
            (LoadState::Empty, _, _) => "Select the panel to find carts".to_string(),
            (LoadState::Loading, _, _) => "Scanning for carts…".to_string(),
            (LoadState::Ready(_), _, None) => "No .cart files under the project root".to_string(),
            (LoadState::Ready(_), _, Some(_)) => "Press Run".to_string(),
        };
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }
}

impl Render for EmuPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // BEFORE building this frame's elements: the image about to be
        // painted becomes N, so N-2 is now safe to hand back. See the
        // module doc.
        self.retire_atlas_frames(window);

        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &Run, window, cx| this.run(window, cx)))
            .on_action(cx.listener(|this, _: &Stop, window, cx| this.stop(window, cx)))
            // The pad. Scoped by focus, not by keymap: these fire only
            // while the pane's focus handle owns the keyboard, so typing
            // `z` anywhere else never reaches a cart.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, _cx| {
                this.on_key(event.keystroke.key.as_str(), true);
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _window, _cx| {
                this.on_key(event.keystroke.key.as_str(), false);
            }))
            // SELECT: a modifier-only press produces no keystroke, so the
            // shift state has to come from here. See `input`'s module doc
            // for why this is either shift rather than the right one.
            .on_modifiers_changed(cx.listener(
                |this, event: &ModifiersChangedEvent, _window, _cx| {
                    this.on_shift(event.modifiers.shift);
                },
            ))
            .child(self.render_transport(window, cx))
            .child(div().flex_1().min_h_0().child(self.render_screen(cx)))
            .children(self.status.as_ref().map(|status| {
                Label::new(status.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
    }
}

impl Focusable for EmuPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for EmuPanel {}

impl Panel for EmuPanel {
    fn persistent_name() -> &'static str {
        "GGO Emulator"
    }

    fn panel_key() -> &'static str {
        GGO_EMU_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call the other three GGO panels made: no settings
        // persistence yet, and Bottom would squash a 4:3 screen.
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
        // `PlayOutlined` over `PlayFilled`: dock icons in this fork are
        // outline glyphs (see `IconName::Debug`/`Terminal` usage), and
        // the filled play is already spoken for by this panel's own Run
        // button, where the weight difference reads as "the action" vs
        // "the panel". `DebugPause`/`Stop` are the other transport
        // glyphs available; neither names a panel.
        Some(IconName::PlayOutlined)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO Emulator")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8, `ggo_metasprite_panel` 9,
        // `ggo_charts_panel` 10 (grep activation_priority across
        // crates/).
        11
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred for the same reason `ggo_world_panel::set_active`
            // defers its own refresh: `set_active` fires inside the
            // workspace's own update (dock toggle), and `refresh_carts`
            // reads the project's worktrees -- a re-entrant read of the
            // entity currently being updated.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_carts(cx)).ok();
            });
        }
    }
}

impl EmuPanel {
    /// A workspace-less panel in a real test window -- the shape
    /// `TestAppContext::add_window_view` wants. Tests that don't need a
    /// window call `Self::new(None, None, cx)` directly.
    #[cfg(test)]
    fn test_new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(None, Some(window), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::dock::DockPosition;
    use workspace::{AppState, MultiWorkspace};

    /// A panel in a real (workspace-less) test window. `AppState::test`
    /// first: `add_window_view` renders immediately, and `render_screen`
    /// reads `cx.theme()`, which panics without the theme global.
    fn windowed_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::Entity<EmuPanel>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
        });
        cx.add_window_view(EmuPanel::test_new)
    }

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock. Goes through
    /// `MultiWorkspace::test_new` rather than a bare `Workspace::test_new`
    /// -- `register_action` handlers are only mounted into the dispatch
    /// tree once something renders `Workspace::actions`, which in
    /// production is `MultiWorkspace`'s render (the other three GGO
    /// panels' tests carry the same note).
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
                workspace.panel::<EmuPanel>(cx).is_some(),
                "EmuPanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<EmuPanel>(cx)
                .expect("EmuPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    /// `refresh_carts` walks the real filesystem off-thread and lands in
    /// `Ready` with the fixture carts listed, first one preselected.
    /// Calls `refresh_carts` directly rather than through `set_active`
    /// (which needs a live `Window`) -- the same shortcut
    /// `ggo_world_panel`'s test helper takes.
    #[gpui::test]
    async fn test_refresh_carts_enumerates_the_project_root(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/second.cart"), b"x").unwrap();
        std::fs::write(dir.path().join("first.cart"), b"x").unwrap();
        std::fs::write(dir.path().join("readme.md"), b"x").unwrap();

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = EmuPanel::new(None, None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });
        panel.update(cx, |panel, cx| panel.refresh_carts(cx));
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            match &panel.state {
                LoadState::Ready(carts) => assert_eq!(
                    carts,
                    &vec!["build/second.cart".to_string(), "first.cart".to_string()]
                ),
                _ => panic!("expected Ready"),
            }
            assert_eq!(
                panel.selected.as_deref(),
                Some("build/second.cart"),
                "the first cart in sorted order is preselected"
            );
        });
    }

    /// A selection whose file disappeared between refreshes is dropped
    /// rather than sliding onto whatever now occupies its index.
    #[gpui::test]
    async fn test_a_vanished_selection_is_not_silently_repointed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.cart"), b"x").unwrap();
        std::fs::write(dir.path().join("b.cart"), b"x").unwrap();

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = EmuPanel::new(None, None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_carts(cx);
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, cx| panel.select_cart("b.cart".to_string(), cx));

        std::fs::remove_file(dir.path().join("b.cart")).unwrap();
        panel.update(cx, |panel, cx| panel.refresh_carts(cx));
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.selected.as_deref(),
                Some("a.cart"),
                "the stale selection is dropped and the list's first entry takes over"
            );
        });
    }

    /// A stale enumeration must not clobber a newer one -- the
    /// load-generation guard every GGO panel carries.
    #[gpui::test]
    async fn test_a_stale_enumeration_is_dropped(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.cart"), b"x").unwrap();

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = EmuPanel::new(None, None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_carts(cx);
            let stale = panel.load_generation;
            // A second refresh before the first lands.
            panel.refresh_carts(cx);
            assert!(panel.load_generation > stale);
        });
        cx.executor().run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert!(matches!(&panel.state, LoadState::Ready(carts) if carts.len() == 1));
        });
    }

    /// Run with nothing selected reports rather than panicking, and
    /// starts no thread.
    #[gpui::test]
    async fn test_run_without_a_selection_is_reported(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.run(window, cx);
            assert!(!panel.is_running());
            assert_eq!(panel.status.as_deref(), Some("no cart selected"));
        });
    }

    /// Key events drive the pad mask through the panel's own handlers,
    /// including the shift-as-SELECT path, and losing focus releases
    /// everything.
    #[gpui::test]
    async fn test_key_handling_latches_and_releases_the_pad(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update(cx, |panel, cx| {
            panel.on_key("z", true);
            panel.on_key("left", true);
            panel.on_shift(true);
            assert_eq!(panel.input.mask(), (1 << 0) | (1 << 6) | input::SELECT_BIT);

            panel.on_key("z", false);
            assert_eq!(panel.input.mask(), (1 << 6) | input::SELECT_BIT);

            // Unmapped keys never touch the mask -- the pane is focused
            // while a user might still hit ctrl-alt-r.
            panel.on_key("r", true);
            assert_eq!(panel.input.mask(), (1 << 6) | input::SELECT_BIT | (1 << 11));
            panel.on_key("escape", true);
            assert_eq!(panel.input.mask(), (1 << 6) | input::SELECT_BIT | (1 << 11));

            panel.release_all_buttons(cx);
            assert_eq!(panel.input.mask(), 0);
        });
    }

    /// The `Ended` half of the pump: a run that dies clears the session
    /// (so Stop stops being offered) and surfaces its reason.
    ///
    /// Note what is NOT tested here: driving a real cart through
    /// `EmuPanel::run`'s real emulator thread. gpui's test scheduler
    /// panics with "Detected activity on thread ... your test is not
    /// deterministic" the moment a foreign thread wakes a task on it, and
    /// the whole point of `drive::start` is that it runs on a foreign
    /// thread. That path is covered instead by `drive`'s own plain
    /// `#[test]`s, which drive the hand-assembled cart end to end without
    /// a gpui window; what is left for this side is the message handling,
    /// which is exactly what this and the atlas tests below exercise.
    #[gpui::test]
    async fn test_an_ended_run_reports_and_clears_the_session(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        // A session whose thread has already failed and exited. Nothing
        // awaits the receiver, so no foreign-thread wake can reach the
        // test scheduler.
        let (session, rx) = drive::start("/definitely/not/here.cart".into(), "gone.cart".into());
        panel.update(cx, |panel, cx| {
            panel.session = Some(session);
            assert!(panel.is_running());

            panel.on_emu_msg(EmuMsg::Ended("cart exited with 0".to_string()), cx);
            assert!(!panel.is_running(), "an ended run clears the session");
            assert_eq!(panel.status.as_deref(), Some("cart exited with 0"));
        });
        drop(rx);
    }

    /// An ended run leaves its last frame on screen rather than blanking
    /// the pane -- the opposite of Stop.
    #[gpui::test]
    async fn test_an_ended_run_keeps_its_last_frame(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        push_frame_and_draw(&panel, cx, 1);
        panel.update(cx, |panel, cx| {
            panel.on_emu_msg(EmuMsg::Ended("cart exited with 0".to_string()), cx);
            assert!(
                panel.latest_frame.is_some(),
                "the final frame must stay on screen"
            );
        });
    }

    /// The dropdown menu entity is built once per enumeration, not once
    /// per render -- `render` runs 60x a second while a cart is running.
    #[gpui::test]
    async fn test_the_cart_menu_is_not_rebuilt_every_render(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        let first = panel.update_in(cx, |panel, window, cx| panel.cart_menu(window, cx));
        let second = panel.update_in(cx, |panel, window, cx| panel.cart_menu(window, cx));
        assert_eq!(first.entity_id(), second.entity_id());

        // A new enumeration invalidates it.
        panel.update(cx, |panel, cx| panel.refresh_carts(cx));
        cx.executor().run_until_parked();
        let third = panel.update_in(cx, |panel, window, cx| panel.cart_menu(window, cx));
        assert_ne!(first.entity_id(), third.entity_id());
    }

    // ------------------------------------------------- atlas retention

    /// Feed one synthetic frame and paint the panel, returning the
    /// `RenderImage` that frame produced so the test can ask the window
    /// whether its atlas tiles are still there.
    fn push_frame_and_draw(
        panel: &gpui::Entity<EmuPanel>,
        cx: &mut gpui::VisualTestContext,
        n: u32,
    ) -> Arc<RenderImage> {
        let bgra = vec![0x7Fu8; (drive::WIDTH * drive::HEIGHT * 4) as usize];
        let image = panel.update(cx, |panel, cx| {
            panel.on_emu_msg(EmuMsg::Frame { bgra, frame: n }, cx);
            panel
                .latest_frame
                .clone()
                .expect("a frame message must produce an image")
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(320.), px(240.)),
            |_window, _cx| panel.clone().into_any_element(),
        );
        image
    }

    /// THE prerequisite this task was gated on. A fresh `RenderImage`
    /// every frame means a fresh process-global `ImageId` and therefore a
    /// fresh atlas tile every frame; gpui frees none of them on its own
    /// (see the module doc). This drives twenty frames through the real
    /// render path and asserts the double buffer actually hands the old
    /// tiles back.
    ///
    /// The assertion is *bounded residency*, not "frame N-2 exactly": how
    /// many times `render` runs per delivered frame is the harness's (and
    /// in production, the compositor's) business, not this panel's --
    /// `push_frame_and_draw` provokes two passes per frame, since
    /// `on_emu_msg`'s `cx.notify` schedules one and `cx.draw` forces
    /// another. What the panel guarantees is the invariant that survives
    /// either cadence: at most two distinct frames are ever resident, so
    /// the atlas does not grow with run length, and the frame just
    /// painted is always among them.
    #[gpui::test]
    async fn test_per_frame_images_do_not_accumulate_atlas_tiles(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);

        let mut all: Vec<Arc<RenderImage>> = Vec::new();
        for n in 1..=20 {
            let image = push_frame_and_draw(&panel, cx, n);
            assert!(
                all.iter().all(|i| i.id != image.id),
                "every frame must be a distinct RenderImage/ImageId"
            );
            assert!(
                cx.update(|window, _| window.has_image_atlas_entry(&image)),
                "frame {n}: the frame being painted must be resident"
            );
            all.push(image);

            let resident = cx.update(|window, _| {
                all.iter()
                    .filter(|i| window.has_image_atlas_entry(i))
                    .count()
            });
            assert!(
                resident <= 2,
                "frame {n}: {resident} frames resident -- atlas residency must \
                 stay bounded, or a 60 Hz run leaks ~18 MB/s of atlas forever"
            );
        }

        // And the ones that were dropped really are gone -- not merely
        // uncounted because they were never uploaded in the first place.
        let released = cx.update(|window, _| {
            all.iter()
                .filter(|i| !window.has_image_atlas_entry(i))
                .count()
        });
        assert!(
            released >= all.len() - 2,
            "only {released} of {} frames released -- the rest leaked",
            all.len()
        );
    }

    /// Stop is a teardown path with no further render to retire through
    /// the double buffer, so it must release everything the pane still
    /// holds.
    #[gpui::test]
    async fn test_stop_releases_every_atlas_tile(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        let a = push_frame_and_draw(&panel, cx, 1);
        let b = push_frame_and_draw(&panel, cx, 2);

        panel.update_in(cx, |panel, window, cx| panel.stop(window, cx));

        assert!(
            cx.update(
                |window, _| !window.has_image_atlas_entry(&a) && !window.has_image_atlas_entry(&b)
            ),
            "Stop must hand every tile back"
        );
        panel.update(cx, |panel, _cx| {
            assert!(panel.latest_frame.is_none());
            assert!(panel.current_rendered_frame.is_none());
            assert!(panel.previous_rendered_frame.is_none());
        });
    }

    /// Stop with nothing running is a no-op that doesn't blank anything
    /// or notify pointlessly.
    #[gpui::test]
    async fn test_stop_without_a_run_is_a_noop(cx: &mut TestAppContext) {
        let (panel, cx) = windowed_panel(cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.stop(window, cx);
            assert!(!panel.is_running());
            assert!(panel.latest_frame.is_none());
        });
    }
}
