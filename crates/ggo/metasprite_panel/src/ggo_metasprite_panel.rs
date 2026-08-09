//! GGO MetaSprite panel (F2 task M4): sprite picker, frame strip, and
//! playback preview. Structural mirror of `ggo_world_panel` -- `Panel`
//! impl, keybinding-reload observer, off-thread loading with a
//! load-generation guard -- with the sprite-specific pieces split out:
//! `loader` owns everything off the UI thread (`.spr` enumeration +
//! open + per-frame compose), `playback` owns the pure range/loop/
//! offset/fit math; this module owns the panel entity, the picker, the
//! transport timer loop, and all gpui wiring. Frame/hitbox EDITING lands
//! in later F2 tasks (M5+) -- this panel is a viewer with a transport.

mod loader;
mod playback;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding,
    ParentElement, Pixels, Render, RenderImage, Styled, Task, WeakEntity, Window, actions, div,
    img, px,
};
use ui::prelude::*;
use ui::{ContextMenu, DropdownMenu};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_worldlib::sprites::cow::SpriteState;
use ggo_worldlib::sprites::timeline_ops::{playback_frame_at, playback_total_ms};

actions!(
    ggo_metasprite,
    [
        /// Toggles focus on the GGO metasprite panel.
        ToggleFocus,
        /// Toggles playback of the open sprite's active clip range.
        PlayPause
    ]
);

const GGO_METASPRITE_PANEL_KEY: &str = "GGOMetaSpritePanel";

/// The panel's key-dispatch context (`.key_context`), which the
/// [`bind_panel_keys`] bindings are scoped to -- same shape as
/// `ggo_world_panel::KEY_CONTEXT`.
const KEY_CONTEXT: &str = "GgoMetaSpritePanel";

/// Fixed default width until the panel grows real settings persistence.
const DEFAULT_WIDTH: Pixels = px(360.);

/// Frame-strip thumbnail box (px, square -- frames fit inside it via
/// `playback::fit_size`).
const THUMB_PX: f32 = 48.0;

/// Large center preview box (px, square).
const PREVIEW_PX: f32 = 240.0;

/// Playback timer cadence. 16ms tracks a 60Hz frame; the ACTUAL frame
/// shown each tick is recomputed from wall-clock elapsed time
/// (`playback_frame_at`), so a late tick skips ahead rather than
/// slowing playback down.
const TICK: Duration = Duration::from_millis(16);

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as `ggo_world_panel::init`: `zed::reload_keymaps` clears
    // and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), and keymap assets are upstream files
    // this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` keeps the panel's bindings alive across
    // reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| MetaSpritePanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<MetaSpritePanel>(window, cx);
        });
    })
    .detach();
}

fn bind_panel_keys(cx: &mut App) {
    // Scoped to the panel's own key context, so space only toggles
    // playback while the panel has focus. (`ToggleFocus` stays unbound,
    // dispatched via `Panel::toggle_action` / the command palette, same
    // as `ggo_world::ToggleFocus`.)
    cx.bind_keys([KeyBinding::new("space", PlayPause, Some(KEY_CONTEXT))]);
}

// ------------------------------------------------------------- view state

/// An in-flight playback: wall-clock anchored (the shown frame is
/// recomputed from `started.elapsed()` every tick, never incremented), so
/// tick jitter can't drift the transport.
struct Playing {
    started: Instant,
    /// Elapsed-ms seed so playback starts ON the selected frame
    /// (`playback::start_offset_ms`).
    start_offset_ms: i64,
    /// The frame the transport is currently showing.
    frame: usize,
}

/// A loaded sprite plus its per-frame image cache and transport state.
struct OpenSprite {
    rel_path: String,
    state: SpriteState,
    /// One composed BGRA image per frame index, built once at load (see
    /// `loader::LoadedSprite::frames` for the M5 invalidation hook).
    frames: Vec<Arc<RenderImage>>,
    selected_frame: usize,
    /// Index into `state.clips`; `None` = whole-sprite range.
    active_clip: Option<usize>,
    playing: Option<Playing>,
    /// The transport's timer loop -- dropping it (new sprite selected,
    /// panel dropped) cancels playback; a finished loop leaves a spent
    /// task behind, which the next play simply replaces.
    _tick_task: Option<Task<()>>,
}

impl OpenSprite {
    fn new(rel_path: String, loaded: loader::LoadedSprite) -> Self {
        OpenSprite {
            rel_path,
            state: loaded.state,
            frames: loaded.frames,
            selected_frame: 0,
            active_clip: None,
            playing: None,
            _tick_task: None,
        }
    }

    /// The frame the big preview shows: the transport's while playing,
    /// the selected frame otherwise (so stopping "resets" the preview to
    /// the selection).
    fn shown_frame(&self) -> usize {
        self.playing
            .as_ref()
            .map_or(self.selected_frame, |p| p.frame)
    }

    fn durations(&self) -> Vec<u16> {
        self.state.frames.iter().map(|f| f.duration_ms).collect()
    }
}

enum ViewerState {
    /// Nothing selected yet.
    Empty,
    Loading {
        rel_path: String,
    },
    Ready(Box<OpenSprite>),
    Error(String),
}

pub struct MetaSpritePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// Sorted `.spr` rel paths under the project root -- the picker feed.
    sprites: Vec<String>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl MetaSpritePanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            sprites: Vec::new(),
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree) and re-enumerate its sprites. Runs on every panel
    /// activation -- same discovery as `ggo_world_panel::refresh_worlds`.
    fn refresh_sprites(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        self.sprites = match &self.project_root {
            Some(root) => loader::list_sprites(root),
            None => Vec::new(),
        };
        cx.notify();
    }

    /// Kick off the off-thread load of `self.sprites[ix]`. A stale result
    /// (superseded by a later selection) is dropped by generation check.
    fn select_sprite(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(rel) = self.sprites.get(ix).cloned() else {
            return;
        };
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            rel_path: rel.clone(),
        };
        cx.notify();

        let load = {
            let rel = rel.clone();
            cx.background_spawn(async move { loader::load_sprite(&root, &rel) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => ViewerState::Ready(Box::new(OpenSprite::new(rel, loaded))),
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Click a strip thumbnail: select that frame (the transport, if
    /// running, keeps playing -- its own frame wins in the preview until
    /// it stops).
    fn select_frame(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && ix < open.state.frames.len()
        {
            open.selected_frame = ix;
            cx.notify();
        }
    }

    /// Pick the active clip (`None` = whole sprite). A running transport
    /// picks the new range up on its next tick (range/loop are re-read
    /// per tick, ggo-ide's mid-playback-edit rule).
    fn select_clip(&mut self, clip: Option<usize>, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.active_clip = clip;
            cx.notify();
        }
    }

    /// Play/pause toggle. Play anchors a wall clock, seeds the elapsed
    /// offset so playback starts on the selected frame (clamped into the
    /// active range), and spawns the tick loop; pause drops the loop and
    /// the transport state (the preview falls back to the selected
    /// frame).
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open.playing.is_some() {
            open.playing = None;
            open._tick_task = None;
            cx.notify();
            return;
        }
        if open.state.frames.is_empty() {
            return;
        }
        let durations = open.durations();
        let range =
            playback::play_range(&open.state.clips, open.active_clip, open.state.frames.len());
        let start_offset_ms = playback::start_offset_ms(&durations, range, open.selected_frame);
        open.playing = Some(Playing {
            started: Instant::now(),
            start_offset_ms,
            frame: open
                .selected_frame
                .clamp(range.0.min(range.1), range.0.max(range.1)),
        });
        // The timer loop: sleep a tick, recompute the shown frame from
        // wall-clock elapsed, stop when a non-looping range finishes (or
        // the panel/sprite goes away). `cx.spawn` + a
        // `background_executor().timer` await is this checkout's idiom
        // for periodic UI work (gpui has no retained per-entity
        // animation-frame callback; element-level `with_animation` is a
        // fixed-duration easing wrapper, wrong shape for a
        // duration-table-driven transport).
        open._tick_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let finished = this.update(cx, |this, cx| this.advance_playback(cx));
                match finished {
                    Ok(false) => {}
                    Ok(true) | Err(_) => break,
                }
            }
        }));
        cx.notify();
    }

    /// One transport tick: re-read durations/range/loop from the doc
    /// (mid-playback edits -- M5 -- get picked up immediately, ggo-ide's
    /// `tick` rule) and recompute the shown frame. Returns true when the
    /// loop should stop: playback was cancelled, or a non-looping range
    /// ran past its total (which also resets the preview to the selected
    /// frame).
    fn advance_playback(&mut self, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
            return true;
        };
        let Some(playing) = &mut open.playing else {
            return true;
        };
        let durations: Vec<u16> = open.state.frames.iter().map(|f| f.duration_ms).collect();
        let range =
            playback::play_range(&open.state.clips, open.active_clip, open.state.frames.len());
        let loop_ = playback::play_loop(&open.state.clips, open.active_clip);
        let t_ms = playing.start_offset_ms + playing.started.elapsed().as_millis() as i64;
        if !loop_ && t_ms >= playback_total_ms(&durations, range) {
            open.playing = None;
            cx.notify();
            return true;
        }
        playing.frame = playback_frame_at(&durations, range, t_ms, loop_);
        cx.notify();
        false
    }

    // ------------------------------------------------------------- render

    fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_rel = match &self.state {
            ViewerState::Ready(open) => Some(open.rel_path.clone()),
            ViewerState::Loading { rel_path } => Some(rel_path.clone()),
            _ => None,
        };
        h_flex()
            .flex_wrap()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when(self.sprites.is_empty(), |this| {
                let message = if self.project_root.is_some() {
                    "No sprites found"
                } else {
                    "No project open"
                };
                this.child(Label::new(message).color(Color::Muted))
            })
            .children(self.sprites.iter().enumerate().map(|(ix, rel)| {
                let selected = selected_rel.as_deref() == Some(rel.as_str());
                Button::new(("ggo-metasprite", ix), SharedString::from(rel.clone()))
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_sprite(ix, cx)))
            }))
    }

    fn render_message(&self, message: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }

    /// Transport row: play/pause, the clip selector, and the sprite name.
    fn render_transport(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_transport is only called in the Ready state");
        };
        let playing = open.playing.is_some();
        let clip_label: SharedString = match open.active_clip.and_then(|i| open.state.clips.get(i))
        {
            Some(c) => c.name.clone().into(),
            None => "All frames".into(),
        };
        let clip_names: Vec<String> = open.state.clips.iter().map(|c| c.name.clone()).collect();
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            {
                let weak = weak.clone();
                menu = menu.entry("All frames", None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_clip(None, cx)).ok();
                });
            }
            for (ix, name) in clip_names.into_iter().enumerate() {
                let weak = weak.clone();
                menu = menu.entry(SharedString::from(name), None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_clip(Some(ix), cx))
                        .ok();
                });
            }
            menu
        });
        h_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Button::new(
                    "ggo-metasprite-play",
                    if playing { "Pause" } else { "Play" },
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx))),
            )
            .child(DropdownMenu::new("ggo-metasprite-clip", clip_label, menu))
            .child(div().flex_1())
            .child(
                Label::new(SharedString::from(open.rel_path.clone()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// The big center preview: the shown frame fit into a
    /// [`PREVIEW_PX`] box.
    fn render_preview(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_preview is only called in the Ready state");
        };
        let shown = open.shown_frame();
        let mut preview = div()
            .flex_1()
            .min_h_0()
            .flex()
            .justify_center()
            .items_center()
            .bg(cx.theme().colors().editor_background);
        if let Some(image) = open.frames.get(shown) {
            let (w, h) = image_px_size(image);
            let (fit_w, fit_h) = playback::fit_size(w, h, PREVIEW_PX);
            preview = preview.child(img(image.clone()).w(px(fit_w)).h(px(fit_h)));
        }
        preview.into_any_element()
    }

    /// The bottom frame strip: one thumbnail + duration label per frame,
    /// click to select, selected frame outlined.
    fn render_strip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_strip is only called in the Ready state");
        };
        let selected = open.selected_frame;
        let border = cx.theme().colors().border;
        let accent = cx.theme().colors().border_focused;
        div()
            .id("ggo-metasprite-strip")
            .flex_none()
            .p_1()
            .border_t_1()
            .border_color(border)
            .overflow_x_scroll()
            .child(
                h_flex()
                    .gap_1()
                    .children(open.state.frames.iter().enumerate().map(|(ix, frame)| {
                        let thumb = open.frames.get(ix).map(|image| {
                            let (w, h) = image_px_size(image);
                            let (fit_w, fit_h) = playback::fit_size(w, h, THUMB_PX);
                            img(image.clone()).w(px(fit_w)).h(px(fit_h))
                        });
                        v_flex()
                            .id(("ggo-metasprite-frame", ix))
                            .items_center()
                            .gap_0p5()
                            .p_0p5()
                            .border_1()
                            .rounded_sm()
                            .border_color(if ix == selected { accent } else { border })
                            .child(
                                div()
                                    .w(px(THUMB_PX))
                                    .h(px(THUMB_PX))
                                    .flex()
                                    .justify_center()
                                    .items_center()
                                    .children(thumb),
                            )
                            .child(
                                Label::new(format!("{} ms", frame.duration_ms))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.select_frame(ix, cx)))
                    })),
            )
            .into_any_element()
    }

    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .child(self.render_transport(window, cx))
            .child(self.render_preview(cx))
            .child(self.render_strip(cx))
            .into_any_element()
    }
}

/// A `RenderImage`'s pixel size (frame 0 -- worldlib composes are always
/// single-frame).
fn image_px_size(image: &Arc<RenderImage>) -> (u32, u32) {
    let size = image.size(0);
    (size.width.0 as u32, size.height.0 as u32)
}

impl Render for MetaSpritePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            ViewerState::Empty => self.render_message("Select a sprite".to_string(), cx),
            ViewerState::Loading { rel_path } => {
                self.render_message(format!("Loading {rel_path}…"), cx)
            }
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(window, cx),
        };
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &PlayPause, _window, cx| this.toggle_play(cx)))
            .bg(cx.theme().colors().panel_background)
            .child(self.render_picker(cx))
            .child(div().flex_1().min_h_0().child(body))
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

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_sprites` needs to READ
            // the workspace to find the project root -- reading it
            // re-entrantly panics (same as `ggo_world_panel`).
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_sprites(cx)).ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::cow::{ClipEdit, Frame};
    use ggo_worldlib::sprites::hw::{TILE_BYTES, TILE_PX};
    use ggo_worldlib::sprites::io::save_sprite;
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

    /// Author a real 2-frame sprite trio (`.spr`/`.til`/`.pal`) via
    /// worldlib's own `save_sprite` -- one call persists all three files
    /// (the pool IS the tileset; `save_tileset` would only rewrite the
    /// same `.til`/`.pal` pair, so it isn't needed). Frame 0 shows the
    /// all-transparent tile 0; frame 1 shows tile 1, filled with palette
    /// index 1 (red) -- distinguishable thumbnails. One non-looping
    /// single-frame clip exercises the clip selector path.
    fn write_sprite_fixture(root: &std::path::Path) {
        let mut pool = vec![0u8; 2 * TILE_BYTES];
        for b in &mut pool[TILE_BYTES..] {
            *b = 0x11; // both nibbles = palette index 1
        }
        let mut palette = [0u16; 16];
        palette[1] = 0xF800; // pure 565 red
        let state = SpriteState {
            pool,
            tile_count: 2,
            session_tiles: std::collections::HashSet::new(),
            palette,
            frames: vec![
                Frame {
                    map: vec![0],
                    duration_ms: 100,
                },
                Frame {
                    map: vec![1],
                    duration_ms: 200,
                },
            ],
            clips: vec![ClipEdit {
                name: "walk".to_string(),
                from: 1,
                to: 1,
                loop_: false,
            }],
            w_tiles: 1,
            h_tiles: 1,
            pool_shared: false,
        };
        save_sprite(
            root,
            "sprites/hero.spr",
            &state,
            "sprites/hero.til",
            "sprites/hero.pal",
        )
        .unwrap();
    }

    /// End-to-end viewer load against a real-fs temp project: the picker
    /// enumerates the fixture `.spr`, selecting it runs the off-thread
    /// loader, and the panel reaches Ready with one composed thumbnail
    /// per frame (frame 0 all-transparent, frame 1 opaque red -- proving
    /// per-frame compose order survived the BGRA bridge).
    #[gpui::test]
    async fn test_select_sprite_reaches_ready_with_frame_thumbnails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture(dir.path());

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = MetaSpritePanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });

        panel.update(cx, |panel, cx| {
            panel.refresh_sprites(cx);
            assert_eq!(panel.sprites, ["sprites/hero.spr"]);
            panel.select_sprite(0, cx);
            assert!(matches!(panel.state, ViewerState::Loading { .. }));
        });

        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready state after load");
            };
            assert_eq!(open.rel_path, "sprites/hero.spr");
            assert_eq!(open.state.frames.len(), 2);
            assert_eq!(open.frames.len(), 2, "one thumbnail per frame");
            assert_eq!(open.state.frames[0].duration_ms, 100);
            assert_eq!(open.state.frames[1].duration_ms, 200);
            assert_eq!(open.state.clips.len(), 1);
            assert_eq!(open.shown_frame(), 0, "not playing => selected frame");

            let px_count = TILE_PX * TILE_PX;
            let f0 = open.frames[0].as_bytes(0).unwrap();
            assert_eq!(f0.len(), px_count * 4);
            assert!(
                f0.chunks_exact(4).all(|p| p[3] == 0),
                "frame 0 (tile 0, all index 0) must be fully transparent"
            );
            let f1 = open.frames[1].as_bytes(0).unwrap();
            assert!(
                f1.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                "frame 1 (tile 1, palette red) must be opaque red in BGRA"
            );
        });
    }

    /// Frame selection + clip selection state: strip clicks move the
    /// selected (and shown) frame, out-of-range clicks are ignored, and
    /// the clip selector round-trips through `active_clip`.
    #[gpui::test]
    async fn test_frame_and_clip_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_sprite_fixture(dir.path());

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = MetaSpritePanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_sprites(cx);
            panel.select_sprite(0, cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.select_frame(1, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected_frame, 1);
            assert_eq!(open.shown_frame(), 1);

            panel.select_frame(9, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected_frame, 1, "out-of-range click is ignored");

            panel.select_clip(Some(0), cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.active_clip, Some(0));
            assert_eq!(
                playback::play_range(&open.state.clips, open.active_clip, 2),
                (1, 1)
            );

            panel.select_clip(None, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.active_clip, None);
        });
    }
}
