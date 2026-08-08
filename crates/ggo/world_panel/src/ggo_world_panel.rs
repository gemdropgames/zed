//! GGO World panel: a dock panel that lists the project's `worlds/**.toml`
//! files and renders the selected world with real pixels (composed sprite/
//! map images via `ggo-worldlib`), pan (middle-mouse drag) and wheel zoom.
//!
//! Split: `loader` owns everything that runs off the UI thread (world
//! read, instance resolution, asset composition); `canvas` owns camera
//! math and painting; this module owns the panel entity, the picker, and
//! the state machine between them.

mod canvas;
mod loader;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Action, App, Bounds, Context, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, RenderImage,
    ScrollWheelEvent, Styled, Task, WeakEntity, Window, actions, div, px,
};
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_worldlib::backgrounds::MergedBackground;
use ggo_worldlib::render::{AssetLoads, active_camera_origin, build_draw_list};
use ggo_worldlib::world_doc::WorldDocStore;
use ggo_worldlib::world_files::WorldListing;

actions!(
    ggo_world,
    [
        /// Toggles focus on the GGO world panel.
        ToggleFocus
    ]
);

const GGO_WORLD_PANEL_KEY: &str = "GGOWorldPanel";

/// Fixed default width until the panel grows real settings persistence.
const DEFAULT_WIDTH: Pixels = px(360.);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| WorldPanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorldPanel>(window, cx);
        });
    })
    .detach();
}

// ------------------------------------------------------------- view state

/// Pan/zoom + gesture state shared (via `Rc<RefCell>`) between the panel's
/// event listeners and the canvas element's prepaint/paint closures --
/// prepaint is where the canvas learns its bounds, which both the lazy
/// initial camera centering and cursor-anchored zoom need.
struct ViewShared {
    zoom: f64,
    /// `None` until the first prepaint centers the camera (canvas size is
    /// unknown before layout).
    pan: Option<[f64; 2]>,
    last_bounds: Option<Bounds<Pixels>>,
    drag: Option<Drag>,
}

/// An in-flight middle-mouse pan drag.
struct Drag {
    start_cursor: [f64; 2],
    start_pan: [f64; 2],
}

/// A loaded world plus its render-side caches.
struct OpenWorld {
    listing: WorldListing,
    store: WorldDocStore,
    sprite_loads: AssetLoads,
    map_loads: AssetLoads,
    meta_sprite_loads: AssetLoads,
    merged: Vec<MergedBackground>,
    /// One gpui `RenderImage` (BGRA) per composed worldlib image, built
    /// once at load time -- see `canvas::build_image_cache`.
    images: Arc<std::collections::HashMap<usize, Arc<RenderImage>>>,
    view: Rc<RefCell<ViewShared>>,
}

impl OpenWorld {
    fn new(
        listing: WorldListing,
        loaded: loader::LoadedWorld,
        images: std::collections::HashMap<usize, Arc<RenderImage>>,
    ) -> Self {
        OpenWorld {
            listing,
            store: loaded.store,
            sprite_loads: loaded.sprite_loads,
            map_loads: loaded.map_loads,
            meta_sprite_loads: loaded.meta_sprite_loads,
            merged: loaded.merged,
            images: Arc::new(images),
            view: Rc::new(RefCell::new(ViewShared {
                zoom: 1.0,
                pan: None,
                last_bounds: None,
                drag: None,
            })),
        }
    }
}

enum ViewerState {
    /// Nothing selected yet.
    Empty,
    Loading {
        stem: String,
    },
    Ready(Box<OpenWorld>),
    Error(String),
}

pub struct WorldPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    worlds: Vec<WorldListing>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl WorldPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            worlds: Vec::new(),
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree) and re-enumerate its worlds. Runs on every panel
    /// activation -- the walk only touches `<root>/worlds`, so it's cheap.
    fn refresh_worlds(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        self.worlds = match &self.project_root {
            Some(root) => loader::list_worlds(root),
            None => Vec::new(),
        };
        cx.notify();
    }

    /// Kick off the off-thread load of `self.worlds[ix]`. A stale result
    /// (superseded by a later selection) is dropped by generation check.
    fn select_world(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(listing) = self.worlds.get(ix).cloned() else {
            return;
        };
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            stem: listing.stem.clone(),
        };
        cx.notify();

        let rel = listing.rel_path.clone();
        let load = cx.background_spawn(async move {
            loader::load_world(&root, &rel).map(|loaded| {
                // Build the BGRA `RenderImage`s off-thread too -- one per
                // stem, never per frame.
                let images = canvas::build_image_cache(&[
                    &loaded.sprite_loads,
                    &loaded.map_loads,
                    &loaded.meta_sprite_loads,
                ]);
                (loaded, images)
            })
        });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok((loaded, images)) => {
                        ViewerState::Ready(Box::new(OpenWorld::new(listing, loaded, images)))
                    }
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    // ------------------------------------------------------------- render

    fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_stem = match &self.state {
            ViewerState::Ready(open) => Some(open.listing.stem.clone()),
            ViewerState::Loading { stem } => Some(stem.clone()),
            _ => None,
        };
        h_flex()
            .flex_wrap()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when(self.worlds.is_empty(), |this| {
                let message = if self.project_root.is_some() {
                    "No worlds found"
                } else {
                    "No project open"
                };
                this.child(Label::new(message).color(Color::Muted))
            })
            .children(self.worlds.iter().enumerate().map(|(ix, listing)| {
                let selected = selected_stem.as_deref() == Some(listing.stem.as_str());
                Button::new(("ggo-world", ix), SharedString::from(listing.stem.clone()))
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_world(ix, cx)))
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

    fn render_viewer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_viewer is only called in the Ready state");
        };

        // Build the paint-ordered draw list from current state + loads --
        // per render (i.e. per notify), matching ggo-ide's
        // per-frame-of-change rebuild; images inside are `Arc` clones.
        let state = open.store.state();
        let items = build_draw_list(
            &state,
            &open.merged,
            None,
            &open.sprite_loads,
            &open.map_loads,
            &open.meta_sprite_loads,
        );
        let screen_origin = active_camera_origin(&state);
        let world_center = canvas::camera_center(screen_origin);
        let images = open.images.clone();
        let view = open.view.clone();
        let background = cx.theme().colors().editor_background;
        let text_color = cx.theme().colors().text;

        let element = gpui::canvas(
            move |canvas_bounds, _window, _cx| {
                let mut v = view.borrow_mut();
                v.last_bounds = Some(canvas_bounds);
                if v.pan.is_none() {
                    // First layout: center the active camera's view.
                    v.pan = Some(canvas::centering_pan(
                        f64::from(canvas_bounds.size.width),
                        f64::from(canvas_bounds.size.height),
                        v.zoom,
                        world_center,
                    ));
                }
                canvas::Scene {
                    items,
                    images,
                    zoom: v.zoom,
                    pan: v.pan.expect("initialized above"),
                    screen_origin,
                    background,
                    text_color,
                }
            },
            move |canvas_bounds, scene, window, cx| {
                canvas::paint_scene(&scene, canvas_bounds, window, cx)
            },
        )
        .size_full();

        div()
            .id("ggo-world-canvas")
            .size_full()
            .overflow_hidden()
            .child(element)
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    let ViewerState::Ready(open) = &this.state else {
                        return;
                    };
                    let mut v = open.view.borrow_mut();
                    if let Some(pan) = v.pan {
                        v.drag = Some(Drag {
                            start_cursor: [
                                f64::from(event.position.x),
                                f64::from(event.position.y),
                            ],
                            start_pan: pan,
                        });
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                let ViewerState::Ready(open) = &this.state else {
                    return;
                };
                let mut v = open.view.borrow_mut();
                let Some(drag) = &v.drag else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Middle) {
                    v.drag = None;
                    return;
                }
                let dx = f64::from(event.position.x) - drag.start_cursor[0];
                let dy = f64::from(event.position.y) - drag.start_cursor[1];
                v.pan = Some([drag.start_pan[0] + dx, drag.start_pan[1] + dy]);
                drop(v);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                    if let ViewerState::Ready(open) = &this.state {
                        open.view.borrow_mut().drag = None;
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                let ViewerState::Ready(open) = &this.state else {
                    return;
                };
                let mut v = open.view.borrow_mut();
                let (Some(pan), Some(canvas_bounds)) = (v.pan, v.last_bounds) else {
                    return;
                };
                let dy = f32::from(event.delta.pixel_delta(px(20.)).y);
                if dy == 0.0 {
                    return;
                }
                let dir = if dy > 0.0 { 1 } else { -1 };
                let new_zoom = canvas::zoom_step(v.zoom, dir);
                if new_zoom == v.zoom {
                    return;
                }
                let cursor = [
                    f64::from(event.position.x - canvas_bounds.origin.x),
                    f64::from(event.position.y - canvas_bounds.origin.y),
                ];
                v.pan = Some(canvas::zoom_at(pan, v.zoom, cursor, new_zoom));
                v.zoom = new_zoom;
                drop(v);
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Render for WorldPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            ViewerState::Empty => self.render_message("Select a world".to_string(), cx),
            ViewerState::Loading { stem } => self.render_message(format!("Loading {stem}…"), cx),
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_viewer(cx),
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(self.render_picker(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for WorldPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorldPanel {}

impl Panel for WorldPanel {
    fn persistent_name() -> &'static str {
        "GGO World"
    }

    fn panel_key() -> &'static str {
        GGO_WORLD_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // No settings persistence yet (see task-5-brief.md); Bottom isn't a
        // sensible spot for a world/map editor sidebar, so only Left/Right.
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
        Some(IconName::Public)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO World")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_worlds` needs to READ the
            // workspace to find the project root -- reading it re-entrantly
            // panics.
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_worlds(cx)).ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::render::DrawKind;
    use ggo_worldlib::world_file::{WorldEntity, WorldFile, WorldInstance, write_world};
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use workspace::dock::DockPosition;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock and focuses the panel.
    /// Goes through `MultiWorkspace::test_new` rather than a bare
    /// `Workspace::test_new`, because `register_action` handlers (like
    /// `ToggleFocus`) are only mounted into the dispatch tree once something
    /// renders `Workspace::actions`, which in production is
    /// `MultiWorkspace`'s render (same lesson as ggo_hello's F0 test).
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
                workspace.panel::<WorldPanel>(cx).is_some(),
                "WorldPanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<WorldPanel>(cx)
                .expect("WorldPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    fn entity(components: serde_json::Value) -> WorldEntity {
        WorldEntity {
            components: components.as_object().unwrap().clone(),
        }
    }

    /// Fixture note: the world is built from RectFill/Text/Camera entities
    /// plus a nested `[[instance]]` -- deliberately NO image assets
    /// (authoring a valid `.spr`/`.til`/`.pal`/`.map` set through
    /// `sprites::io` save fns would dwarf the test; the brief allows this
    /// choice). Real-image composition is worldlib-tested; the BGRA bridge
    /// has its own unit test in `canvas`.
    fn write_fixture(root: &std::path::Path) {
        let sub = WorldFile {
            entities: vec![entity(json!({
                "Transform": { "pos": [0.0, 0.0], "z": 1.0 },
                "RectFill": { "w": 8.0, "h": 8.0, "color": 2016.0 }
            }))],
            instances: vec![],
            backgrounds: vec![],
        };
        write_world(root, "worlds/sub.toml", &sub).unwrap();

        let main = WorldFile {
            entities: vec![
                entity(json!({
                    "Transform": { "pos": [4.0, 4.0], "z": 0.0 },
                    "RectFill": { "w": 16.0, "h": 12.0, "color": 63488.0 }
                })),
                entity(json!({
                    "Transform": { "pos": [40.0, 8.0], "z": 2.0 },
                    "Text": { "content": "hello", "max_width": 40.0, "max_height": 12.0 }
                })),
                entity(json!({
                    "Transform": { "pos": [0.0, 0.0], "z": 0.0 },
                    "Camera": { "is_active": true }
                })),
            ],
            instances: vec![WorldInstance {
                world: "worlds/sub".to_string(),
                pos: [32.0, 16.0],
                background_priority: false,
            }],
            backgrounds: vec![],
        };
        write_world(root, "worlds/test.toml", &main).unwrap();
    }

    /// End-to-end viewer load against a real-fs temp project: picker
    /// enumerates both worlds, selecting `worlds/test` runs the off-thread
    /// loader (including recursive instance resolution of `worlds/sub`),
    /// and the panel reaches Ready with a non-empty draw list containing
    /// both the top-level and the instance-resolved RectFill.
    #[gpui::test]
    async fn test_select_world_reaches_ready_with_draw_items(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path());

        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = WorldPanel::new(None, cx);
                panel.root_override = Some(dir.path().to_path_buf());
                panel
            })
        });

        panel.update(cx, |panel, cx| {
            panel.refresh_worlds(cx);
            let stems: Vec<&str> = panel.worlds.iter().map(|w| w.stem.as_str()).collect();
            assert_eq!(stems, ["worlds/sub", "worlds/test"]);
            let ix = panel
                .worlds
                .iter()
                .position(|w| w.stem == "worlds/test")
                .unwrap();
            panel.select_world(ix, cx);
            assert!(matches!(panel.state, ViewerState::Loading { .. }));
        });

        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready state after load");
            };
            let state = open.store.state();
            let items = build_draw_list(
                &state,
                &open.merged,
                None,
                &open.sprite_loads,
                &open.map_loads,
                &open.meta_sprite_loads,
            );
            assert!(!items.is_empty(), "draw list should not be empty");

            let rects = items
                .iter()
                .filter(|i| matches!(i.kind, DrawKind::Rect { .. }))
                .count();
            assert_eq!(
                rects, 2,
                "one top-level RectFill + one from the resolved worlds/sub instance"
            );
            assert!(
                items
                    .iter()
                    .any(|i| matches!(&i.kind, DrawKind::Text { content } if content == "hello")),
                "Text entity should be in the draw list"
            );
            assert!(
                items
                    .iter()
                    .any(|i| matches!(i.kind, DrawKind::InstanceOrigin)),
                "instance origin gizmo should be in the draw list"
            );
        });
    }
}
