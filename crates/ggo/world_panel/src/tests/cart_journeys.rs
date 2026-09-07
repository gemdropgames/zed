//! The cart-owned editing journeys: the panel's REAL mouse and keymap
//! handlers driving a REAL emerald editor runtime in-process
//! ([`super::cart_harness::CartHarness`]).
//!
//! Nothing here hand-rolls a datagram. A click goes in through
//! `canvas_button_down`, out over the ggo-wire link, through the cart's
//! `edit_builtin`, back as the rows and gestures the cart publishes, and
//! is asserted on where the user would see it: the overlay the Live canvas
//! outlines, the inspector's position field, the document, and the undo
//! stack.
//!
//! Every coordinate a journey names is a WORLD pixel, converted to the
//! canvas through the panel's own Live transform ([`Journey::pt`]). The
//! Live prepaint stamps the canvas bounds itself once the cart is drawing,
//! so where the device frame sits and what integer scale it is drawn at
//! are the tab's layout to decide -- and a journey that hard-coded either
//! would be asserting on the test window's size.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::cart_harness::CartHarness;
use super::*;

// ------------------------------------------------------------- fixtures

/// The cart's own edit system for the journeys that need one: it counts
/// the left presses it sees and claims the frame's pointer, which is how a
/// real tool takes the gesture away from the systems AFTER it.
static USER_TOOL_PRESSES: AtomicU32 = AtomicU32::new(0);
/// The same, for the edit system registered BEHIND `user_tool` -- it only
/// counts a press nothing has claimed, which is what `consumed` is for.
static LATE_TOOL_PRESSES: AtomicU32 = AtomicU32::new(0);

fn user_tool(world: &mut emerald_core::World) {
    let Some(input) = world
        .try_resource::<emerald_editor_runtime::EditorInput>()
        .cloned()
    else {
        return;
    };
    if !input.just_pressed(emerald_editor_runtime::Button::Left) {
        return;
    }
    USER_TOOL_PRESSES.fetch_add(1, Ordering::SeqCst);
    if let Some(input) = world.try_resource_mut::<emerald_editor_runtime::EditorInput>() {
        input.consumed = true;
    }
}

fn late_tool(world: &mut emerald_core::World) {
    let Some(input) = world
        .try_resource::<emerald_editor_runtime::EditorInput>()
        .cloned()
    else {
        return;
    };
    if input.consumed || !input.just_pressed(emerald_editor_runtime::Button::Left) {
        return;
    }
    LATE_TOOL_PRESSES.fetch_add(1, Ordering::SeqCst);
}

/// The cart's one game system: slides the entity at tracked index 0 a
/// pixel right per frame, so a Play frame is visible in the rows.
fn slide_first(world: &mut emerald_core::World) {
    let Some(entity) = world.iter_entities().next() else {
        return;
    };
    if let Some(transform) = world.get_mut::<emerald_core::Transform>(entity) {
        transform.pos = transform.pos.add(emerald_core::Vec2::int(1, 0));
    }
}

/// Journey 25's cart-side edit system: on the frame it is armed for, it
/// moves the fixture's first box by +5 px and reports the entity as
/// changed -- what a user tool does when it edits the world itself.
static NUDGE_ARMED: AtomicU32 = AtomicU32::new(0);

fn nudge_box_a(world: &mut emerald_core::World) {
    if NUDGE_ARMED.swap(0, Ordering::SeqCst) == 0 {
        return;
    }
    // The fixture authors the camera first, so the first box is the
    // SECOND entity -- `BOX_A`'s tracked index.
    let Some(entity) = world.iter_entities().nth(BOX_A as usize) else {
        return;
    };
    if let Some(transform) = world.get_mut::<emerald_core::Transform>(entity) {
        transform.pos = transform.pos.add(emerald_core::Vec2::int(5, 0));
    }
    emerald_editor_runtime::mark_dirty(world, entity);
}

const NO_SYSTEMS: emerald_editor_runtime::link::SystemTable = &[];
const NUDGE_TABLE: emerald_editor_runtime::link::SystemTable = &[("nudge", nudge_box_a)];
const USER_TOOL_TABLE: emerald_editor_runtime::link::SystemTable = &[("user", user_tool)];
/// Two tools in table order: the second sees a press only if the first
/// left it alone.
const TWO_TOOL_TABLE: emerald_editor_runtime::link::SystemTable =
    &[("user", user_tool), ("late", late_tool)];
const GAME_TABLE: emerald_editor_runtime::link::SystemTable = &[("slide", slide_first)];

/// An entity table with just a `Transform` -- no sprite, so the cart
/// outlines and hit-tests it as the 16x16 fallback box at `pos`
/// (`drawn_footprint`).
fn boxed_entity(pos: [f64; 2]) -> WorldEntity {
    boxed_entity_z(pos, 0.0)
}

/// [`boxed_entity`] at a chosen depth: the cart hit-tests topmost by `z`.
fn boxed_entity_z(pos: [f64; 2], z: f64) -> WorldEntity {
    WorldEntity {
        components: json!({ "Transform": { "pos": pos, "z": z } })
            .as_object()
            .expect("an object literal")
            .clone(),
    }
}

/// The camera every journey fixture carries: `is_centered = false` at the
/// origin, so the cart's `effective_camera` is `(0, 0)` and a device pixel
/// IS a world pixel. Without it the panel frames the document at
/// `(-160, -120)` and every coordinate in these tests would be written
/// against that offset instead of against the world file.
fn origin_camera() -> WorldEntity {
    WorldEntity {
        components: json!({
            "Transform": { "pos": [0.0, 0.0], "z": 0.0 },
            "Camera": { "is_active": true, "is_centered": false }
        })
        .as_object()
        .expect("an object literal")
        .clone(),
    }
}

/// Cart index of the fixture's first box: the camera entity is authored
/// first so that `active_camera_origin` (which scans in document order)
/// cannot pick up anything else, and it publishes a row of its own.
const BOX_A: u32 = 1;
const BOX_B: u32 = 2;
/// Where each fixture entity is authored, in world px.
const BOX_A_POS: [f64; 2] = [40.0, 50.0];
const BOX_B_POS: [f64; 2] = [100.0, 50.0];
const INSTANCE_POS: [f64; 2] = [100.0, 120.0];
/// The second member's offset inside `worlds/pair`.
const PAIR_SPREAD: f64 = 24.0;
/// `worlds/grid`'s one box, authored on the 16 px grid.
const GRID_BOX: u32 = 1;
const GRID_BOX_POS: [f64; 2] = [32.0, 48.0];
/// `worlds/stacked`'s two boxes: same place, `STACK_TOP` the higher `z`.
const STACK_POS: [f64; 2] = [152.0, 112.0];
const STACK_TOP: u32 = 2;
/// How many boxes `worlds/many` holds. With the camera that is more than
/// the 60 indices one `Selection` datagram carries.
const MANY_BOXES: u32 = 70;

/// The journey worlds, written over whatever `routed_project`'s own
/// fixture left behind.
///
/// * `worlds/journey` -- a camera at the origin and two 16x16 boxes.
/// * `worlds/instanced` -- the same camera, one box, and an `[[instance]]`
///   of `worlds/pair`, which is two boxes.
fn write_journey_fixture(root: &std::path::Path) {
    write_world(
        root,
        "worlds/pair.toml",
        &WorldFile {
            entities: vec![boxed_entity([0.0, 0.0]), boxed_entity([0.0, PAIR_SPREAD])],
            instances: vec![],
            backgrounds: vec![],
        },
    )
    .expect("the sub-world writes");
    write_world(
        root,
        "worlds/journey.toml",
        &WorldFile {
            entities: vec![
                origin_camera(),
                boxed_entity(BOX_A_POS),
                boxed_entity(BOX_B_POS),
            ],
            instances: vec![],
            backgrounds: vec![],
        },
    )
    .expect("the journey world writes");
    // No camera entity: the pan journey needs `effective_camera` to fall
    // back to the engine camera RESOURCE, which is the one the cart's
    // `camera_pan` moves.
    write_world(
        root,
        "worlds/nocam.toml",
        &WorldFile {
            entities: vec![boxed_entity(BOX_A_POS), boxed_entity(BOX_B_POS)],
            instances: vec![],
            backgrounds: vec![],
        },
    )
    .expect("the camera-less world writes");
    write_world(
        root,
        "worlds/instanced.toml",
        &WorldFile {
            entities: vec![origin_camera(), boxed_entity(BOX_A_POS)],
            instances: vec![WorldInstance {
                world: "worlds/pair".to_string(),
                pos: INSTANCE_POS,
                background_priority: false,
            }],
            backgrounds: vec![],
        },
    )
    .expect("the instanced world writes");
    // On the 16 px grid already, so a snapped drag lands on it exactly.
    write_world(
        root,
        "worlds/grid.toml",
        &WorldFile {
            entities: vec![origin_camera(), boxed_entity(GRID_BOX_POS)],
            instances: vec![],
            backgrounds: vec![],
        },
    )
    .expect("the grid world writes");
    // Two boxes in the same place at different depths, near the middle of
    // the device screen so they stay on the canvas at a stepped-up scale.
    write_world(
        root,
        "worlds/stacked.toml",
        &WorldFile {
            entities: vec![
                origin_camera(),
                boxed_entity_z(STACK_POS, 0.0),
                boxed_entity_z(STACK_POS, 5.0),
            ],
            instances: vec![],
            backgrounds: vec![],
        },
    )
    .expect("the stacked world writes");
    // More entities than one `Selection` datagram carries, so selecting
    // them all has to arrive in parts.
    let mut many = vec![origin_camera()];
    many.extend((0..MANY_BOXES).map(|index| {
        let index = f64::from(index);
        boxed_entity([(index % 20.0) * 16.0, (index / 20.0).floor() * 16.0 + 16.0])
    }));
    write_world(
        root,
        "worlds/many.toml",
        &WorldFile {
            entities: many,
            instances: vec![],
            backgrounds: vec![],
        },
    )
    .expect("the crowded world writes");
}

// ------------------------------------------------------- panel + harness

/// Everything a journey drives: the dock-hosted panel, the workspace the
/// tab lives in, the link endpoint, and the cart on the far end of it.
struct Journey<'a> {
    panel: Entity<WorldPanel>,
    workspace: Entity<Workspace>,
    endpoint: Arc<ggo_common::LinkEndpoint>,
    cart: CartHarness,
    cx: &'a mut gpui::VisualTestContext,
    dir: tempfile::TempDir,
}

/// A world tab open in Live with a real cart behind it, connected, its
/// world blob loaded, and the canvas laid out at exactly the device size.
///
/// Deliberately its own wiring rather than `connected_live_panel`'s: these
/// journeys need a fixture with a camera at the origin and a known
/// instance shape, and they need the workspace back so the closing journey
/// can close the tab.
async fn journey<'a>(
    cx: &'a mut TestAppContext,
    rel: &str,
    edit_systems: emerald_editor_runtime::link::SystemTable,
    game_systems: emerald_editor_runtime::link::SystemTable,
) -> Journey<'a> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let project = routed_project(cx, dir.path(), true).await;
    write_journey_fixture(dir.path());
    cx.update(|cx| ggo_common::register_viewer_booter(cx, fake_booter));
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let dock = workspace.read_with(cx, |workspace, cx| {
        workspace
            .panel::<WorldDock>(cx)
            .expect("init() adds the dock")
    });
    dock.update(cx, |dock, _| {
        dock.test_root_override(dir.path().to_path_buf())
    });
    let rel = rel.to_string();
    workspace.update_in(cx, |workspace, window, cx| {
        ggo_common::open_in_panel(workspace, window, cx, |dock: &mut WorldDock, window, cx| {
            dock.open_world(&rel, window, cx);
        })
    });
    cx.run_until_parked();
    let panel = dock.read_with(cx, |dock, _| dock.active().expect("a world tab"));
    let endpoint = BOOTED
        .with(|booted| booted.borrow().last().map(|(_, e)| e.clone()))
        .expect("Live mode asked the booter for a viewer cart");

    let cart = CartHarness::new(endpoint.clone(), edit_systems, game_systems);
    endpoint.set_state(ggo_common::ViewerState::Running);
    // The Live geometry has to be defined before the first frame is
    // presented, or the boot screen stands in for the canvas and there is
    // no transform to place the greeting's camera through. The Live
    // prepaint replaces this with the tab's real bounds as soon as the
    // cart draws, which is what every coordinate here is taken through.
    panel.update(cx, |panel, _| {
        let mut view = open_of(panel).view.borrow_mut();
        view.zoom = 1.0;
        view.pan = Some([0.0, 0.0]);
        view.last_bounds = Some(gpui::bounds(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(320.), px(240.)),
        ));
    });
    let mut journey = Journey {
        panel,
        workspace,
        endpoint,
        cart,
        cx,
        dir,
    };
    journey.settle();
    journey
}

impl Journey<'_> {
    /// Frame the cart until the panel has nothing left to push: the
    /// greeting, the world blob and the four layer slots all have to land
    /// before the cart's rows describe the document the panel is showing.
    fn settle(&mut self) {
        for _ in 0..400 {
            let pending = self.panel.read_with(self.cx, |panel, _| {
                let live = live_of(panel);
                live.status != LiveStatus::Connected
                    || live.world_dirty
                    || live.layers_dirty.any()
                    || !live.layer_queue.is_empty()
                    || live.pending_camera.is_some()
                    || !live.pending_edits.is_empty()
                    || live.mailbox.busy()
                    || !live.loaded()
            });
            if !pending {
                return;
            }
            self.cart.frame(self.cx);
        }
        panic!("the live session never settled");
    }

    fn frames(&mut self, count: usize) {
        self.cart.frames(count, self.cx);
    }

    /// What the Live canvas would outline right now: one entry per
    /// published cart row that still resolves to something in the
    /// document, in the order the cart published them.
    fn overlay(&mut self) -> Vec<(Selection, [f64; 4], bool)> {
        self.panel.read_with(self.cx, |panel, _| overlay_of(panel))
    }

    /// The overlay entry for `selection`: its world rect and whether it is
    /// drawn selected.
    fn outline(&mut self, selection: Selection) -> ([f64; 4], bool) {
        self.overlay()
            .into_iter()
            .find(|(target, _, _)| *target == selection)
            .map(|(_, rect, selected)| (rect, selected))
            .unwrap_or_else(|| panic!("no overlay row for {selection:?}"))
    }

    /// The document selection the panel is showing.
    fn selected(&mut self) -> Vec<Selection> {
        self.panel
            .read_with(self.cx, |panel, _| open_of(panel).selected.clone())
    }

    /// [`Self::selected`] in a stable order: the cart publishes its
    /// selection in ITS order, which a marquee sweep does not promise to
    /// keep in document order.
    fn sorted_selection(&mut self) -> Vec<Selection> {
        let mut selected = self.selected();
        selected.sort_by_key(|selection| match selection {
            Selection::Entity(index) => (0, *index),
            Selection::Instance(index) => (1, *index),
        });
        selected
    }

    /// The integer scale the picture is drawn at: one world pixel is this
    /// many canvas pixels, so a gesture measured in DEVICE px (the
    /// camera pan) has to be scaled up to reach the canvas.
    fn scale(&mut self) -> f64 {
        self.panel
            .read_with(self.cx, |panel, _| {
                let size = panel.live_canvas_size()?;
                panel.live_camera_for(size).map(|(_, _, scale)| scale)
            })
            .map(f64::from)
            .expect("a laid-out live canvas")
    }

    /// The camera the cart last reported, in world px.
    fn camera(&mut self) -> [f64; 2] {
        self.panel
            .read_with(self.cx, |panel, _| live_of(panel).overlay_camera())
    }

    /// What the inspector's `pos.x`/`pos.y` fields render for a document
    /// entity -- the real `display_text`, not a re-derivation of it.
    fn inspector_position(&mut self, entity: usize) -> [String; 2] {
        self.panel.read_with(self.cx, |panel, _| {
            let state = open_of(panel).store.state();
            [0usize, 1].map(|axis| {
                inspector::display_text(
                    &inspector::FieldTarget::EntityVec2Axis {
                        entity,
                        component: "Transform".to_string(),
                        field: "pos".to_string(),
                        axis,
                    },
                    &state,
                    &[],
                )
            })
        })
    }

    fn entity_pos(&mut self, index: usize) -> [f64; 2] {
        self.panel
            .read_with(self.cx, |panel, _| entity_pos_of(panel, index))
    }

    fn instance_pos(&mut self, index: usize) -> [f64; 2] {
        self.panel.read_with(self.cx, |panel, _| {
            open_of(panel).store.state().instances[index].pos
        })
    }

    fn entity_count(&mut self) -> usize {
        self.panel.read_with(self.cx, |panel, _| {
            open_of(panel).store.state().entities.len()
        })
    }

    /// How many rows the CART is publishing -- its own `EntityCount`.
    fn cart_rows(&mut self) -> usize {
        self.panel
            .read_with(self.cx, |panel, _| live_of(panel).rows.len())
    }

    /// Undo one document entry the way the panel's own action does --
    /// which also prunes the selection and re-sends the world, so the CART
    /// moves back too -- and report whether there was an entry to undo.
    /// Asserting on `store.undo()` alone would prove the document half and
    /// leave the cart showing the move.
    fn undo(&mut self) -> bool {
        let before = self.doc_generation();
        self.panel.update(self.cx, |panel, cx| panel.undo_impl(cx));
        self.settle();
        self.frames(2);
        self.doc_generation() != before
    }

    /// [`Self::undo`], reporting whether the panel armed a WHOLE-WORLD
    /// resend for the step. That flag is what tells a move replayed to the
    /// cart as `SetTransform`s from a structural undo, which nothing but
    /// the blob can describe.
    fn undo_resent_the_world(&mut self) -> bool {
        let before = self.doc_generation();
        self.panel.update(self.cx, |panel, cx| panel.undo_impl(cx));
        let resent = self
            .panel
            .read_with(self.cx, |panel, _| live_of(panel).world_dirty);
        assert_ne!(self.doc_generation(), before, "there was an entry to undo");
        self.settle();
        self.frames(2);
        resent
    }

    /// The document's change counter, which every applied op moves.
    fn doc_generation(&mut self) -> u64 {
        self.panel
            .read_with(self.cx, |panel, _| open_of(panel).doc_generation)
    }

    /// Zero the tool counters while the harness holds the runtime's
    /// mailbox lock: the statics are shared by every journey in the
    /// binary, and a reset taken BEFORE `journey()` (which is where the
    /// lock is taken) could clear a count another journey is mid-way
    /// through.
    fn reset_tool_counters(&mut self) {
        USER_TOOL_PRESSES.store(0, Ordering::SeqCst);
        LATE_TOOL_PRESSES.store(0, Ordering::SeqCst);
    }

    /// A world point in canvas-local px, through the panel's OWN Live
    /// transform. Not a constant: the Live prepaint stamps the canvas
    /// bounds itself as soon as the cart is drawing, so where the device
    /// frame sits is the tab's layout to decide, not the test's.
    fn pt(&mut self, world: [f64; 2]) -> [f64; 2] {
        self.panel
            .read_with(self.cx, |panel, _| live_screen_of(panel, world))
    }

    /// [`Self::pt`] at the centre of the 16x16 box a fixture entity is
    /// drawn as.
    fn on(&mut self, world: [f64; 2]) -> [f64; 2] {
        self.pt([world[0] + 8.0, world[1] + 8.0])
    }

    /// The screen rect the overlay outlines cart row `index` at, on the
    /// canvas as it is actually laid out.
    fn screen_rect(&mut self, index: u32) -> [f64; 4] {
        self.panel
            .read_with(self.cx, |panel, _| {
                let size = panel.live_canvas_size()?;
                panel.test_live_row_screen_rect(index, size)
            })
            .expect("a laid-out live canvas")
    }

    /// A move with nothing held, so the cart's `prev_device` is where the
    /// cursor actually is before a button goes down -- a pan measures its
    /// delta against that, and a press that teleports the pointer would
    /// otherwise pan by the whole jump.
    fn hover(&mut self, at: [f64; 2]) {
        self.panel.update(self.cx, |panel, _| {
            assert!(panel.canvas_pointer_move(at, None, &Modifiers::none()));
        });
    }

    fn press(&mut self, at: [f64; 2], modifiers: Modifiers) {
        self.panel.update(self.cx, |panel, _| {
            assert!(
                panel.canvas_button_down(at, MouseButton::Left, &modifiers),
                "the cart owns the left button in Live"
            );
        });
    }

    fn drag_to(&mut self, at: [f64; 2]) {
        self.panel.update(self.cx, |panel, _| {
            assert!(panel.canvas_pointer_move(at, Some(MouseButton::Left), &Modifiers::none()));
        });
    }

    fn release(&mut self, at: [f64; 2]) {
        self.panel.update(self.cx, |panel, _| {
            assert!(panel.canvas_button_up(at, MouseButton::Left, &Modifiers::none()));
        });
    }

    fn middle_press(&mut self, at: [f64; 2]) {
        self.panel.update(self.cx, |panel, _| {
            assert!(panel.canvas_button_down(at, MouseButton::Middle, &Modifiers::none()));
        });
    }

    fn middle_drag_to(&mut self, at: [f64; 2]) {
        self.panel.update(self.cx, |panel, _| {
            assert!(panel.canvas_pointer_move(at, Some(MouseButton::Middle), &Modifiers::none()));
        });
    }

    fn middle_release(&mut self, at: [f64; 2]) {
        self.panel.update(self.cx, |panel, _| {
            assert!(panel.canvas_button_up(at, MouseButton::Middle, &Modifiers::none()));
        });
    }

    /// A click: press and release on the same pixel, each on its own cart
    /// frame (the cart derives its edges from consecutive samples).
    fn click(&mut self, at: [f64; 2], modifiers: Modifiers) {
        self.press(at, modifiers);
        self.frames(2);
        self.release(at);
        self.frames(2);
    }

    /// Dispatch a panel action through the keymap, the way the key would.
    fn action(&mut self, action: &dyn gpui::Action) {
        let action = action.boxed_clone();
        self.cx
            .update(|window, cx| window.dispatch_action(action, cx));
        self.cx.run_until_parked();
    }

    /// Switch the cart's mode through the rail's own handler -- what the
    /// `Edit | Play` buttons call. Not a simulated click: a journey's
    /// coordinates are taken through the Live transform of a canvas laid
    /// out at the device size, and the rail's click wiring is asserted in
    /// the panel's own tests.
    fn set_mode(&mut self, mode: EditorMode) {
        self.panel
            .update(self.cx, |panel, cx| panel.set_live_mode(mode, cx));
    }

    /// Turn the snap toggle on or off, as the view row's checkbox does --
    /// it writes the same field, and every pointer sample carries it.
    fn set_snap(&mut self, on: bool) {
        self.panel.update(self.cx, |panel, cx| {
            let ViewerState::Ready(open) = &mut panel.state else {
                panic!("expected Ready");
            };
            open.snap = on;
            cx.notify();
        });
    }

    /// A release the canvas never sees: gpui delivers `on_mouse_up` only
    /// to the element under the cursor, so a drag that ends off the canvas
    /// comes back through the hover-out flush and the out-of-bounds
    /// release, in that order -- both of which the canvas div wires up.
    fn release_off_canvas(&mut self, at: [f64; 2]) -> (bool, bool) {
        self.panel.update(self.cx, |panel, _| {
            let flushed = panel.live_release_held();
            let held_after = panel.live_button_held(MouseButton::Left);
            panel.canvas_button_up_out(at, MouseButton::Left, &Modifiers::none());
            (flushed, held_after)
        })
    }

    /// One wheel notch through the panel's real handler.
    fn wheel(&mut self, dy: f32) {
        self.panel.update(self.cx, |panel, cx| {
            panel.wheel_zoom(&wheel_event(8.0, 8.0, dy), cx)
        });
    }

    /// What a rebuilt cart does to a live session: the run goes back to
    /// `Building` and comes up again, which is what makes the panel greet
    /// the cart it is now talking to. The harness cart on the far end is
    /// the same object -- a `Hello` resets its link state, which is the
    /// half under test.
    fn regreet(&mut self) {
        self.endpoint.set_state(ggo_common::ViewerState::Building);
        self.frames(1);
        self.endpoint.set_state(ggo_common::ViewerState::Running);
        self.settle();
    }

    /// How many undo entries the document has. Destructive (it unwinds the
    /// whole stack), so a journey asks last.
    fn undo_depth(&mut self) -> usize {
        undo_depth(&self.panel, self.cx)
    }

    /// Pick a tool through the rail's own handler, as the radio does.
    fn set_tool(&mut self, tool: u8) {
        self.panel
            .update(self.cx, |panel, cx| panel.set_live_tool(tool, cx));
    }

    /// The world root every fixture was written under.
    fn root(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Press Save through the keymap and frame the cart until the save has
    /// settled -- in Live it is the CART that answers, several frames
    /// later. `budget` is in cart frames, so a journey that expects the
    /// deadline to fire spends the whole 300 of it.
    fn save(&mut self, budget: usize) {
        self.action(&Save);
        for _ in 0..budget {
            if !self.save_pending() {
                return;
            }
            self.frames(1);
        }
        assert!(!self.save_pending(), "the save never settled");
    }

    fn save_pending(&mut self) -> bool {
        self.panel
            .read_with(self.cx, |panel, _| open_of(panel).save_pending())
    }

    fn save_error(&mut self) -> Option<String> {
        self.panel
            .read_with(self.cx, |panel, _| open_of(panel).save_error.clone())
    }

    /// Whether the tab is still offering to save something.
    fn dirty(&mut self) -> bool {
        self.panel
            .read_with(self.cx, |panel, _| panel.dirty_world_name().is_some())
    }

    /// The world as it is ON DISK, re-read rather than derived from the
    /// document: a save journey has to prove the bytes landed.
    fn on_disk(&mut self) -> WorldFile {
        world_file::read_world(self.root(), "worlds/journey.toml").expect("the world file")
    }

    /// Type `text` into the inspector's editor for one axis of a document
    /// entity's `Transform.pos` and commit it the way Enter does -- the
    /// panel's own commit path, not a `set_component` behind it.
    fn commit_pos(&mut self, entity: usize, axis: usize, text: &str) {
        let editor = self
            .panel
            .read_with(self.cx, |panel, _| {
                open_of(panel)
                    .inspector
                    .iter()
                    .find(|field| {
                        matches!(
                            &field.target,
                            inspector::FieldTarget::EntityVec2Axis {
                                entity: held,
                                component,
                                field: name,
                                axis: held_axis,
                            } if *held == entity
                                && component == "Transform"
                                && name == "pos"
                                && *held_axis == axis
                        )
                    })
                    .map(|field| field.editor.clone())
            })
            .expect("an inspector editor for Transform.pos");
        self.panel.update_in(self.cx, |panel, window, cx| {
            editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
            panel.commit_editor(editor.entity_id(), cx);
        });
        self.cx.run_until_parked();
    }

    /// Link background slot 0 to a real tileset, which is what gives paint
    /// mode a `.map` to open and the cart a layer to hold.
    fn add_background(&mut self) {
        write_test_tileset(self.root(), "tiles/bg.til");
        self.panel.update(self.cx, |panel, cx| {
            panel.add_background_impl(0, "tiles/bg.til".into(), cx)
        });
        self.cx.run_until_parked();
        self.settle();
    }

    /// Put the brush on background slot 0, as the layers rail does.
    fn enter_paint(&mut self) {
        self.panel.update(self.cx, |panel, cx| {
            assert!(
                panel.enter_paint_mode(PaintTarget::BgSlot(0), cx),
                "slot 0 has a map to paint"
            );
        });
        self.cx.run_until_parked();
    }

    /// One brush click at world `at`, through the canvas's own paint path.
    fn paint_at(&mut self, at: [f64; 2]) {
        let local = self.pt(at);
        self.panel.update_in(self.cx, |panel, _, cx| {
            panel.canvas_primary_down_with(local, false, cx);
            panel.canvas_primary_up(cx);
        });
        self.frames(2);
    }

    /// Slot 0's cells as they are ON DISK.
    fn map_cells_on_disk(&mut self) -> Vec<u16> {
        io::open_map(self.root(), "maps/journey.bg0.map")
            .expect("the linked map")
            .cells
    }
}

// -------------------------------------------------------------- journeys

/// 1. A click on the sprite selects it on the cart, and the panel outlines
///    the rect the cart published for it.
#[gpui::test]
async fn click_selects_and_outlines_the_sprite(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    assert!(
        journey.selected().is_empty(),
        "nothing is selected before the click"
    );

    let at = journey.on(BOX_A_POS);
    journey.press(at, Modifiers::none());
    journey.frames(2);

    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(BOX_A as usize)],
        "the cart hit-tested the click and the panel mirrored its selection"
    );
    let (rect, selected) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(rect, [BOX_A_POS[0], BOX_A_POS[1], 16.0, 16.0]);
    assert!(selected, "and outlines it selected");
    let (_, other) = journey.outline(Selection::Entity(BOX_B as usize));
    assert!(!other, "the box that was not clicked stays unselected");
}

/// 2. A drag moves the entity on the cart, the outline and the inspector
///    follow it every frame, and the whole drag lands as ONE undo entry.
#[gpui::test]
async fn drag_moves_the_entity_and_the_outline_follows(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let start = journey.on(BOX_A_POS);
    journey.press(start, Modifiers::none());
    journey.frames(2);

    for step in 1..=3 {
        let step = f64::from(step);
        let to = journey.on([BOX_A_POS[0] + step * 10.0, BOX_A_POS[1]]);
        journey.drag_to(to);
        journey.frames(2);
        let (rect, selected) = journey.outline(Selection::Entity(BOX_A as usize));
        assert!(selected, "the dragged entity stays selected");
        assert_eq!(
            rect[0],
            BOX_A_POS[0] + step * 10.0,
            "the outline advanced with the cart's row on step {step}"
        );
        assert_eq!(rect[1], BOX_A_POS[1], "and did not drift on the cross axis");
        assert_eq!(
            journey.inspector_position(BOX_A as usize)[0],
            (BOX_A_POS[0] + step * 10.0).to_string(),
            "and the inspector shows the new x"
        );
    }

    let end = journey.on([BOX_A_POS[0] + 30.0, BOX_A_POS[1]]);
    journey.release(end);
    journey.frames(3);
    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [BOX_A_POS[0] + 30.0, BOX_A_POS[1]],
        "the document took the finished move"
    );
    assert!(journey.undo(), "the whole drag is one entry");
    assert_eq!(journey.entity_pos(BOX_A as usize), BOX_A_POS);
    let (rect, _) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(
        [rect[0], rect[1]],
        BOX_A_POS,
        "and the CART moved back too: the undo was replayed to it"
    );
    assert!(!journey.undo(), "and there was only the one");
}

/// 3. Undoing a cart-side drag moves the entity back ON THE CART -- and
///    does it with a `SetTransform`, not a world reload: the cart is
///    holding the right world already, and blanking its rows for a blob
///    round trip is a visible blink for an undone drag.
#[gpui::test]
async fn undo_after_a_drag_moves_it_back_on_the_cart(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let start = journey.on(BOX_A_POS);
    let end = journey.on([BOX_A_POS[0] + 30.0, BOX_A_POS[1]]);
    journey.press(start, Modifiers::none());
    journey.frames(2);
    journey.drag_to(end);
    journey.frames(2);
    journey.release(end);
    journey.frames(3);
    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [BOX_A_POS[0] + 30.0, BOX_A_POS[1]]
    );

    assert!(
        !journey.undo_resent_the_world(),
        "an undone move is replayed to the cart, not re-sent as a world"
    );

    let (rect, _) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(
        [rect[0], rect[1]],
        BOX_A_POS,
        "and the cart put the row back where the transform said"
    );
}

/// 4. Shift-click toggles membership, and a rubber band drawn over both
///    boxes selects the pair -- both decided by the cart, mirrored back.
#[gpui::test]
async fn shift_click_toggles_and_marquee_selects_two(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let box_a = journey.on(BOX_A_POS);
    let box_b = journey.on(BOX_B_POS);
    journey.click(box_a, Modifiers::none());
    assert_eq!(journey.selected(), vec![Selection::Entity(BOX_A as usize)]);

    journey.click(box_b, Modifiers::shift());
    assert_eq!(
        journey.sorted_selection(),
        vec![
            Selection::Entity(BOX_A as usize),
            Selection::Entity(BOX_B as usize)
        ],
        "shift added the second box"
    );

    journey.click(box_b, Modifiers::shift());
    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(BOX_A as usize)],
        "and shift-clicking it again took it back out"
    );

    // A band anchored in empty space, dragged over both boxes.
    let from = journey.pt([20.0, 30.0]);
    let to = journey.pt([140.0, 80.0]);
    journey.press(from, Modifiers::none());
    journey.frames(2);
    journey.drag_to(to);
    journey.frames(2);
    journey.panel.read_with(journey.cx, |panel, _| {
        assert_eq!(
            live_of(panel).marquee,
            Some([20.0, 30.0, 140.0, 80.0]),
            "the panel draws the band the cart is dragging out, corner to corner"
        );
    });
    journey.release(to);
    journey.frames(3);
    assert_eq!(
        journey.sorted_selection(),
        vec![
            Selection::Entity(BOX_A as usize),
            Selection::Entity(BOX_B as usize)
        ],
        "the band swept up both boxes and nothing else"
    );
    journey.panel.read_with(journey.cx, |panel, _| {
        assert!(live_of(panel).marquee.is_none(), "and the band is gone");
    });
}

/// 5. A middle drag pans the CART's camera, and the outline stays on the
///    sprite: it moves in canvas space by exactly what the camera moved.
#[gpui::test]
async fn middle_drag_pans_and_the_outline_stays_on_the_sprite(cx: &mut TestAppContext) {
    // `worlds/nocam` authors no camera entity, so `effective_camera` falls
    // back to the engine camera RESOURCE -- the one the cart's
    // `camera_pan` moves. A scene-placed camera pins the view to itself.
    let mut journey = journey(cx, "worlds/nocam.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    const NOCAM_BOX_A: u32 = 0;
    let from = journey.on(BOX_A_POS);
    journey.hover(from);
    journey.frames(2);
    let camera_before = journey.camera();
    let screen_before = journey.screen_rect(NOCAM_BOX_A);

    // The pan is measured in DEVICE px, so the canvas move is scaled.
    let scale = journey.scale();
    let to = [from[0] + 30.0 * scale, from[1] + 20.0 * scale];
    journey.middle_press(from);
    journey.frames(2);
    journey.middle_drag_to(to);
    journey.frames(2);
    journey.middle_release(to);
    journey.frames(2);

    let camera_after = journey.camera();
    assert_eq!(
        [
            camera_after[0] - camera_before[0],
            camera_after[1] - camera_before[1]
        ],
        [-30.0, -20.0],
        "the cart panned its own camera by the drag"
    );
    let screen_after = journey.screen_rect(NOCAM_BOX_A);
    assert_eq!(
        [
            screen_after[0] - screen_before[0],
            screen_after[1] - screen_before[1]
        ],
        [30.0 * scale, 20.0 * scale],
        "and the outline moved with the picture, not against it"
    );
}

/// 6. The keymap's editing actions are the cart's: nudge by one and by a
///    tile, select everything, and delete it -- off the cart's table and
///    out of the document together.
#[gpui::test]
async fn nudge_delete_select_all_through_the_keymap(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());

    journey.action(&NudgeRight);
    journey.frames(3);
    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [BOX_A_POS[0] + 1.0, BOX_A_POS[1]],
        "one arrow is one pixel"
    );
    let (rect, _) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(rect[0], BOX_A_POS[0] + 1.0, "and the outline moved with it");

    journey.action(&NudgeRightTile);
    journey.frames(3);
    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [BOX_A_POS[0] + 17.0, BOX_A_POS[1]],
        "and the tile arrow is sixteen"
    );
    let (rect, _) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(rect[0], BOX_A_POS[0] + 17.0);

    journey.action(&SelectAll);
    journey.frames(3);
    assert_eq!(
        journey.selected().len(),
        3,
        "the cart selected its whole table (the camera entity counts)"
    );

    assert_eq!(
        journey.cart_rows(),
        3,
        "the cart is drawing all three before the delete"
    );
    journey.action(&DeleteSelected);
    journey.frames(4);
    assert_eq!(journey.cart_rows(), 0, "the cart has nothing left to draw");
    assert_eq!(journey.entity_count(), 0, "and neither has the document");
}

/// 7. An instance drags as a group: a press on one member moves both rows,
///    the document takes ONE `MoveInstance`, and one undo puts it back.
#[gpui::test]
async fn an_instance_drags_as_a_group_and_undoes_as_one(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/instanced.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let member_b = [INSTANCE_POS[0], INSTANCE_POS[1] + PAIR_SPREAD];
    let start = journey.on(INSTANCE_POS);
    journey.press(start, Modifiers::none());
    journey.frames(2);
    assert_eq!(
        journey.selected(),
        vec![Selection::Instance(0)],
        "clicking a member selects the whole instance"
    );

    let to = journey.on([INSTANCE_POS[0] + 20.0, INSTANCE_POS[1]]);
    journey.drag_to(to);
    journey.frames(2);
    let moved: Vec<[f64; 4]> = journey
        .overlay()
        .into_iter()
        .filter(|(selection, _, _)| *selection == Selection::Instance(0))
        .map(|(_, rect, _)| rect)
        .collect();
    assert_eq!(
        moved,
        vec![
            [INSTANCE_POS[0] + 20.0, INSTANCE_POS[1], 16.0, 16.0],
            [member_b[0] + 20.0, member_b[1], 16.0, 16.0],
        ],
        "both members moved: the cart drags the whole group"
    );

    journey.release(to);
    journey.frames(3);
    assert_eq!(
        journey.instance_pos(0),
        [INSTANCE_POS[0] + 20.0, INSTANCE_POS[1]],
        "the document moved the [[instance]], not its members"
    );
    assert!(
        !journey.undo_resent_the_world(),
        "an undone group move is replayed member by member, not re-sent"
    );
    assert_eq!(journey.instance_pos(0), INSTANCE_POS);
    let back: Vec<[f64; 4]> = journey
        .overlay()
        .into_iter()
        .filter(|(selection, _, _)| *selection == Selection::Instance(0))
        .map(|(_, rect, _)| rect)
        .collect();
    assert_eq!(
        back,
        vec![
            [INSTANCE_POS[0], INSTANCE_POS[1], 16.0, 16.0],
            [member_b[0], member_b[1], 16.0, 16.0],
        ],
        "and both members are back on the cart, not just in the document"
    );
    assert!(!journey.undo(), "and only the one");
}

/// 8. Play runs the GAME table and takes the canvas away from the editor:
///    the rows move on their own, a click changes nothing, and going back
///    to Edit stops the game and clears the selection.
#[gpui::test]
async fn play_mode_ignores_clicks_and_runs_the_game(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, GAME_TABLE).await;
    // The camera is authored first, so it is the entity the game system
    // slides -- which is exactly the row the panel must NOT fold back.
    let camera_before = journey.entity_pos(0);
    journey.set_mode(EditorMode::Play);
    journey.frames(2);
    let (rect_before, _) = journey.outline(Selection::Entity(0));

    journey.frames(5);
    let (rect_after, _) = journey.outline(Selection::Entity(0));
    assert_eq!(
        rect_after[0] - rect_before[0],
        5.0,
        "five frames of the game system moved the row five pixels"
    );
    assert_eq!(
        journey.entity_pos(0),
        camera_before,
        "and none of it was written back: a play-through is not an edit"
    );

    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());
    assert!(
        journey.selected().is_empty(),
        "the editor's built-ins do not run in Play"
    );

    journey.set_mode(EditorMode::Edit);
    // The whole world goes out again on the way back into Edit: every
    // report the cart made while the game ran was discarded, and the cart
    // republishes nothing on its own, so the blob is the only thing that
    // can put the two sides back in step.
    journey.settle();
    journey.frames(2);
    let (settled, _) = journey.outline(Selection::Entity(0));
    journey.frames(2);
    let (again, _) = journey.outline(Selection::Entity(0));
    assert_eq!(settled, again, "back in Edit the game system stopped");
    assert!(journey.selected().is_empty());
}

/// 9. A cart tool claims the frame's pointer: with the tool selected the
///    built-in select is inert, the tool sees the press, and `consumed`
///    keeps the edit system BEHIND it off the same press.
///
///    `consumed` gates only the systems that run later -- the runtime
///    schedules `edit_builtin` first (`sync::install`), so nothing can
///    take a press away from the built-in select by consuming it. Taking
///    the pointer off the built-ins is `SetTool`'s job.
#[gpui::test]
async fn a_cart_tool_claims_the_pointer_from_the_systems_behind_it(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", TWO_TOOL_TABLE, NO_SYSTEMS).await;
    journey.reset_tool_counters();
    journey.set_tool(1);
    journey.frames(2);

    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());

    assert_eq!(
        USER_TOOL_PRESSES.load(Ordering::SeqCst),
        1,
        "the cart's edit system got the press"
    );
    assert_eq!(
        LATE_TOOL_PRESSES.load(Ordering::SeqCst),
        0,
        "and consuming it kept the system behind it off the same press"
    );
    assert!(
        journey.selected().is_empty(),
        "the built-in select is not the active tool: nothing was selected"
    );
}

/// 10. Switching to a cart tool makes the built-in pointer systems inert,
///     and switching back turns them on again.
#[gpui::test]
async fn switching_the_tool_makes_the_builtins_inert(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", USER_TOOL_TABLE, NO_SYSTEMS).await;
    journey.reset_tool_counters();

    journey.set_tool(1);
    journey.frames(2);
    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());
    assert!(
        journey.selected().is_empty(),
        "tool 1 is the cart's own; the select tool is not running"
    );

    journey.set_tool(0);
    journey.frames(2);
    journey.click(at, Modifiers::none());
    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(BOX_A as usize)],
        "and tool 0 hands the pointer back to the built-in select"
    );
}

/// 11. Closing the world tab ends the run behind it: the cart is told to
///     stop rather than left drawing into a tab nobody is watching.
#[gpui::test]
async fn closing_the_tab_stops_the_cart(cx: &mut TestAppContext) {
    let Journey {
        panel,
        workspace,
        endpoint,
        cart,
        cx,
        dir: _dir,
    } = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    assert!(
        !endpoint.stop_requested(),
        "a live tab wants its cart running"
    );
    // The run is stopped from the panel's `on_release`, so every handle
    // to it has to go first -- the tab is what owns it, and a test that
    // kept one would prove nothing. The cart goes with it: it is the
    // thing that would keep drawing.
    let released = panel.downgrade();
    drop(panel);
    drop(cart);

    let item_id = workspace.read_with(cx, |workspace, cx| {
        workspace
            .items_of_type::<crate::WorldCanvasItem>(cx)
            .next()
            .expect("the world tab")
            .entity_id()
    });
    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(cx, |pane, window, cx| {
        pane.close_item_by_id(item_id, workspace::SaveIntent::Skip, window, cx)
    })
    .await
    .expect("the tab closes");
    cx.run_until_parked();

    assert!(released.upgrade().is_none(), "the tab owned the panel");
    assert!(
        endpoint.stop_requested(),
        "closing the tab asked the viewer run to end"
    );
}

/// 12. With snap on, the cart lands the drag on the 16 px grid rather than
///     where the cursor let go -- and the outline the panel draws is on
///     the grid with it.
#[gpui::test]
async fn snap_on_drags_land_on_the_16px_grid(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/grid.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    // Before the press: the toggle rides on every pointer sample, and the
    // cart reads it on the frame it moves.
    journey.set_snap(true);

    let start = journey.on(GRID_BOX_POS);
    journey.press(start, Modifiers::none());
    journey.frames(2);
    let to = journey.on([GRID_BOX_POS[0] + 20.0, GRID_BOX_POS[1]]);
    journey.drag_to(to);
    journey.frames(2);
    journey.release(to);
    journey.frames(3);

    assert_eq!(
        journey.entity_pos(GRID_BOX as usize),
        [48.0, 48.0],
        "20 px past a grid start snaps to the next cell, not to 52"
    );
    let (rect, _) = journey.outline(Selection::Entity(GRID_BOX as usize));
    assert_eq!(
        [rect[0], rect[1]],
        [48.0, 48.0],
        "and the outline is where the cart put the sprite"
    );
}

/// 13. Letting go outside the canvas ends the drag once: gpui delivers no
///     mouse-up to an element the cursor has left, so the hover-out flush
///     is what closes it -- and the out-of-bounds release behind it must
///     not send a second edge.
#[gpui::test]
async fn releasing_outside_the_canvas_ends_the_drag_once(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let start = journey.on(BOX_A_POS);
    journey.press(start, Modifiers::none());
    journey.frames(2);
    let to = journey.on([BOX_A_POS[0] + 30.0, BOX_A_POS[1]]);
    journey.drag_to(to);
    journey.frames(2);

    let (flushed, held_after) = journey.release_off_canvas([600.0, 600.0]);
    assert!(flushed, "the cursor left with the button down");
    assert!(
        !held_after,
        "and the flush is the ONE release: nothing is left held for the \
         out-of-bounds handler to send again"
    );
    journey.frames(3);

    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [BOX_A_POS[0] + 30.0, BOX_A_POS[1]],
        "the drag kept what it had moved"
    );

    // No stuck gesture: the next click is a plain selection, not a drag
    // resumed from wherever the cursor came back.
    let box_b = journey.on(BOX_B_POS);
    journey.click(box_b, Modifiers::none());
    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(BOX_B as usize)],
        "and the canvas takes clicks again"
    );
    assert_eq!(
        journey.entity_pos(BOX_B as usize),
        BOX_B_POS,
        "which moved nothing"
    );
    assert_eq!(journey.undo_depth(), 1, "one entry for the whole drag");
}

/// 14. Escape mid-marquee retires the band on BOTH sides: the panel stops
///     drawing it and the cart abandons it, so the release that follows
///     selects nothing.
#[gpui::test]
async fn escape_mid_marquee_retires_the_band_on_both_sides(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let from = journey.pt([20.0, 30.0]);
    let to = journey.pt([140.0, 80.0]);
    journey.press(from, Modifiers::none());
    journey.frames(2);
    journey.drag_to(to);
    journey.frames(2);
    journey.panel.read_with(journey.cx, |panel, _| {
        assert!(live_of(panel).marquee.is_some(), "a band is being dragged");
    });

    journey.action(&ClearSelection);
    journey.frames(3);
    journey.panel.read_with(journey.cx, |panel, _| {
        assert!(
            live_of(panel).marquee.is_none(),
            "Escape stopped the panel drawing it"
        );
    });

    journey.release(to);
    journey.frames(3);
    assert!(
        journey.selected().is_empty(),
        "and the cart abandoned the band rather than applying it on release"
    );
}

/// 15. A click on empty space clears the selection -- the cart's own miss
///     rule, mirrored back into the document.
#[gpui::test]
async fn click_on_empty_space_clears_the_selection(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());
    assert_eq!(journey.selected(), vec![Selection::Entity(BOX_A as usize)]);

    let empty = journey.pt([200.0, 200.0]);
    journey.click(empty, Modifiers::none());
    assert!(
        journey.selected().is_empty(),
        "a miss clears what the hit selected"
    );
    let (_, selected) = journey.outline(Selection::Entity(BOX_A as usize));
    assert!(!selected, "and the outline stops being drawn selected");
}

/// 16. Undoing a delete brings the row back ON THE CART: a restored entity
///     shifts every index above it, so this one goes out as a world.
#[gpui::test]
async fn undo_of_a_delete_brings_the_row_back_on_the_cart(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());
    assert_eq!(journey.cart_rows(), 3);

    journey.action(&DeleteSelected);
    journey.frames(4);
    assert_eq!(journey.entity_count(), 2, "the document lost it");
    assert_eq!(journey.cart_rows(), 2, "and so did the cart");

    assert!(
        journey.undo_resent_the_world(),
        "a restored entity is a world, not a transform"
    );
    assert_eq!(journey.entity_count(), 3);
    assert_eq!(journey.cart_rows(), 3, "the cart is drawing it again");
    let (rect, _) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(
        [rect[0], rect[1]],
        BOX_A_POS,
        "back where it was authored, outline and all"
    );
}

/// 17. Each nudge is its own gesture on the cart ("one gesture per
///     command", spec), so two arrows are two undo entries -- not one run
///     and not one per cart frame.
#[gpui::test]
async fn two_nudges_coalesce_into_undo_entries_per_spec(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());

    journey.action(&NudgeRight);
    journey.frames(3);
    journey.action(&NudgeRight);
    journey.frames(3);
    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [BOX_A_POS[0] + 2.0, BOX_A_POS[1]],
        "both arrows moved it"
    );
    assert_eq!(
        journey.undo_depth(),
        2,
        "one undo entry per nudge, and no extra for the frames between them"
    );
}

/// 18. Two sprites in the same place: the click takes the topmost by `z`.
#[gpui::test]
async fn overlapping_sprites_select_the_topmost_by_z(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/stacked.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let at = journey.on(STACK_POS);
    journey.click(at, Modifiers::none());
    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(STACK_TOP as usize)],
        "the one drawn on top is the one picked"
    );
}

/// 19. A wheel notch steps the picture scale, and the outline stays on the
///     sprite: it grows with the picture and the hit test still finds it
///     where it is drawn.
#[gpui::test]
async fn wheel_zoom_keeps_the_outline_on_the_sprite(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/stacked.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    // Whatever the tab's own layout fits: the Live prepaint stamps the
    // canvas bounds, so the starting scale is not the test's to name.
    let scale_before = journey.scale();
    let before = journey.screen_rect(STACK_TOP);

    journey.wheel(20.0);
    journey.frames(2);
    let scale_after = journey.scale();
    assert_eq!(
        scale_after,
        scale_before + 1.0,
        "one notch is one step of the integer scale"
    );

    let after = journey.screen_rect(STACK_TOP);
    let growth = scale_after / scale_before;
    assert_eq!(
        [after[2], after[3]],
        [before[2] * growth, before[3] * growth],
        "the outline grew with the picture"
    );
    let at = journey.pt(STACK_POS);
    assert_eq!(
        [after[0], after[1]],
        at,
        "and sits exactly where the panel's own transform puts that world point"
    );

    // The user's proof: the sprite is still clickable where it is drawn.
    let on = journey.on(STACK_POS);
    journey.click(on, Modifiers::none());
    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(STACK_TOP as usize)],
        "the hit test and the outline agree about the new scale"
    );
}

/// 20. A selection too big for one datagram arrives whole: the cart splits
///     it into parts and the panel reassembles every index before it maps
///     them back to the document.
#[gpui::test]
async fn a_selection_larger_than_one_datagram_arrives_whole(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/many.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let total = MANY_BOXES as usize + 1;
    assert_eq!(journey.cart_rows(), total, "the cart is drawing them all");

    journey.action(&SelectAll);
    journey.frames(4);

    journey.panel.read_with(journey.cx, |panel, _| {
        assert_eq!(
            live_of(panel).cart_selection.len(),
            total,
            "every index of a multi-part selection reached the mailbox"
        );
    });
    assert_eq!(
        journey.selected().len(),
        total,
        "and the document selection is the whole table"
    );
}

/// 21. A cart rebuilt mid-drag abandons it cleanly: the greeting resets
///     both sides, nothing moves by the gesture that was in flight, and
///     the canvas takes the next click.
#[gpui::test]
async fn a_regreet_mid_drag_abandons_the_drag_cleanly(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let start = journey.on(BOX_A_POS);
    journey.press(start, Modifiers::none());
    journey.frames(2);
    let to = journey.on([BOX_A_POS[0] + 30.0, BOX_A_POS[1]]);
    journey.drag_to(to);
    journey.frames(2);
    let mid_drag = journey.entity_pos(BOX_A as usize);
    assert_eq!(mid_drag, [BOX_A_POS[0] + 30.0, BOX_A_POS[1]]);

    journey.regreet();
    journey.frames(3);

    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        mid_drag,
        "the abandoned drag moved nothing further"
    );
    let (rect, _) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(
        [rect[0], rect[1]],
        mid_drag,
        "and the rebuilt cart was given the document back"
    );
    assert!(
        journey.selected().is_empty(),
        "a greeting resets the cart's selection"
    );

    let box_b = journey.on(BOX_B_POS);
    journey.press(box_b, Modifiers::none());
    journey.frames(2);
    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(BOX_B as usize)],
        "and the next press is a plain selection, not a resumed drag"
    );
    assert_eq!(journey.entity_pos(BOX_B as usize), BOX_B_POS);

    // The rebuilt cart's gesture ids start over at 1, and the drag it
    // abandoned was never closed: a stack (or a tag) carried across the
    // greeting would fold this drag into the abandoned one's undo entry.
    let onward = journey.on([BOX_B_POS[0] + 20.0, BOX_B_POS[1]]);
    journey.drag_to(onward);
    journey.frames(2);
    journey.release(onward);
    journey.frames(3);
    assert_eq!(
        journey.entity_pos(BOX_B as usize),
        [BOX_B_POS[0] + 20.0, BOX_B_POS[1]]
    );
    assert_eq!(
        journey.undo_depth(),
        2,
        "the new session's drag is its own undo entry"
    );
}

/// 23. An inspector edit is a cart edit like any other: typing a new x
///     into `Transform.pos` and committing it (Enter, or a blur) moves
///     the row ON THE CART, the overlay follows the row back, and the
///     document holds what was typed.
#[gpui::test]
async fn an_inspector_edit_round_trips_through_the_cart(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());
    assert_eq!(journey.selected(), vec![Selection::Entity(BOX_A as usize)]);
    // The inspector's editors are built while the panel renders, and the
    // fields it builds are the SELECTION's.
    show_panel(journey.cx);

    journey.commit_pos(BOX_A as usize, 0, "88");
    journey.settle();
    journey.frames(2);

    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [88.0, BOX_A_POS[1]],
        "the document took the typed value"
    );
    let (rect, _) = journey.outline(Selection::Entity(BOX_A as usize));
    assert_eq!(
        [rect[0], rect[1]],
        [88.0, BOX_A_POS[1]],
        "and the cart moved its row, which is what the overlay draws"
    );
    assert_eq!(
        journey.inspector_position(BOX_A as usize)[0],
        "88",
        "the field reads back what the round trip landed on"
    );
}

/// 24. A cell painted in Live reaches the CART, a cell at a time -- the
///     `.map` is not saved yet, so nothing else could have told it -- and
///     the save then puts that cell on disk.
#[gpui::test]
async fn a_cell_painted_in_live_reaches_the_cart_and_the_map(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    journey.add_background();
    journey.enter_paint();
    assert_eq!(
        journey.map_cells_on_disk().first().copied(),
        Some(live::BLANK_TILE),
        "the linked map starts blank"
    );

    journey.paint_at([4.0, 4.0]);
    let painted = journey.panel.read_with(journey.cx, |panel, _| {
        panel
            .test_paint_session("maps/journey.bg0.map")
            .and_then(|session| session.store.state().cells.first().copied())
            .expect("a session with cells")
    });
    assert_ne!(painted, live::BLANK_TILE, "the brush painted a tile");
    let (_, _, cells) = journey.cart.layer_cells(0).expect("the cart loaded slot 0");
    assert_eq!(
        cells.first().copied(),
        Some(painted),
        "the cart's own layer holds the painted cell, before any save"
    );

    journey.save(400);
    assert_eq!(journey.save_error(), None, "the save landed");
    assert_eq!(
        journey.map_cells_on_disk().first().copied(),
        Some(painted),
        "and the `.map` on disk has it"
    );
}

/// 25. An edit one of the CART's own systems makes survives a save: the
///     snapshot is what the save writes, so a position no host command
///     ever named still reaches the TOML. And Save in Play is refused --
///     the entities are the game's there, not the author's.
#[gpui::test]
async fn a_user_system_edit_survives_a_save(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NUDGE_TABLE, GAME_TABLE).await;

    journey.set_mode(EditorMode::Play);
    journey.frames(2);
    journey.action(&Save);
    journey.frames(2);
    assert_eq!(
        journey.save_error().as_deref(),
        Some("stop the game to save"),
        "Play is the game's world, not the author's"
    );
    journey.set_mode(EditorMode::Edit);
    journey.settle();

    NUDGE_ARMED.store(1, Ordering::SeqCst);
    journey.frames(4);
    assert_eq!(
        journey.entity_pos(BOX_A as usize),
        [BOX_A_POS[0] + 5.0, BOX_A_POS[1]],
        "the cart's own system moved it, and the mirror followed"
    );

    journey.save(400);
    assert_eq!(journey.save_error(), None);
    assert_eq!(
        journey.on_disk().entities[BOX_A as usize].components["Transform"]["pos"],
        json!([BOX_A_POS[0] as i64 + 5, BOX_A_POS[1] as i64]),
        "the TOML has the position the cart's system put it at"
    );
    assert!(!journey.dirty(), "and the tab is clean again");
}

/// 26. Adding and deleting entities in Live are the cart's: `+ Entity`
///     spawns a row on it, `DeleteSelected` despawns it, and the undo of
///     the delete puts the row back on the cart.
#[gpui::test]
async fn add_and_delete_entities_in_live(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    assert_eq!(journey.cart_rows(), 3);

    journey.panel.update(journey.cx, |panel, cx| panel.add_entity_impl(cx));
    journey.settle();
    journey.frames(4);
    assert_eq!(journey.entity_count(), 4, "the document gained one");
    assert_eq!(journey.cart_rows(), 4, "and the cart spawned it");
    assert_eq!(
        journey.selected(),
        vec![Selection::Entity(3)],
        "the new entity is what the DOCUMENT selects"
    );
    // ...but not what the cart does: the wire has no host -> cart
    // selection message, so a spawn the host asked for lands unselected
    // on the cart and `DeleteSelected` -- which despawns the CART's
    // selection -- would find nothing. A click is what selects it there.
    let spawned = journey.entity_pos(3);
    let at = journey.on(spawned);
    journey.click(at, Modifiers::none());
    assert_eq!(journey.selected(), vec![Selection::Entity(3)]);

    journey.action(&DeleteSelected);
    journey.frames(4);
    assert_eq!(journey.entity_count(), 3, "the cart despawned it");
    assert_eq!(journey.cart_rows(), 3);

    assert!(journey.undo(), "there was a delete to undo");
    assert_eq!(journey.entity_count(), 4);
    assert_eq!(journey.cart_rows(), 4, "and the cart is drawing it again");
}

/// 27. The cart never sends the bytes a save asked for: the save fails at
///     its deadline, the file is left byte for byte as it was, and the
///     tab stays dirty. A half-written world is worse than none.
#[gpui::test]
async fn a_snapshot_that_never_arrives_keeps_the_file(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let start = journey.on(BOX_A_POS);
    journey.press(start, Modifiers::none());
    journey.frames(2);
    let to = journey.on([BOX_A_POS[0] + 20.0, BOX_A_POS[1]]);
    journey.drag_to(to);
    journey.frames(2);
    journey.release(to);
    journey.frames(2);
    assert!(journey.dirty(), "the drag left something to save");
    let before = std::fs::read(journey.root().join("worlds/journey.toml")).expect("the world file");

    journey.cart.drop_cart_blobs(true);
    journey.save(live::SAVE_DEADLINE_FRAMES as usize + 8);

    assert_eq!(
        journey.save_error().as_deref(),
        Some("the cart did not answer in time")
    );
    assert!(journey.dirty(), "a failed save keeps the document dirty");
    assert_eq!(
        std::fs::read(journey.root().join("worlds/journey.toml")).expect("the world file"),
        before,
        "and the file is byte for byte what it was"
    );
}

/// 28. Duplicate is the cart's command: it copies the selection at +16 px
///     and selects the copies, the document gains them at the indices the
///     cart numbered them with, and the undo despawns them again.
#[gpui::test]
async fn duplicate_copies_the_selection_on_the_cart_and_in_the_document(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let at = journey.on(BOX_A_POS);
    journey.click(at, Modifiers::none());
    assert_eq!(journey.cart_rows(), 3);

    journey.action(&Duplicate);
    journey.settle();
    journey.frames(4);

    assert_eq!(journey.cart_rows(), 4, "the cart made the copy");
    assert_eq!(journey.entity_count(), 4, "and the document gained it");
    let copy = Selection::Entity(3);
    let (rect, selected) = journey.outline(copy);
    assert_eq!(
        [rect[0], rect[1]],
        [BOX_A_POS[0] + 16.0, BOX_A_POS[1] + 16.0],
        "the copy sits one tile down and right of its original"
    );
    assert!(selected, "and the copy is what is selected now");
    assert_eq!(
        journey.entity_pos(3),
        [BOX_A_POS[0] + 16.0, BOX_A_POS[1] + 16.0],
        "the document holds the copy at the cart's own index"
    );

    assert!(journey.undo(), "there was a duplicate to undo");
    assert_eq!(journey.entity_count(), 3);
    assert_eq!(journey.cart_rows(), 3, "the copy is gone from the cart too");
}

/// 22. The accepted limitation, asserted so a change to it fails loudly:
///     a middle drag pans the cart's camera RESOURCE, and a world whose
///     scene places an active `Camera` component draws from that instead
///     -- so the pan is inert. Journey 5 is the same gesture in a world
///     that authors none.
#[gpui::test]
async fn pan_is_inert_in_a_world_that_authors_a_camera(cx: &mut TestAppContext) {
    let mut journey = journey(cx, "worlds/journey.toml", NO_SYSTEMS, NO_SYSTEMS).await;
    let from = journey.on(BOX_A_POS);
    journey.hover(from);
    journey.frames(2);
    let camera_before = journey.camera();
    let screen_before = journey.screen_rect(BOX_A);

    let scale = journey.scale();
    let to = [from[0] + 30.0 * scale, from[1] + 20.0 * scale];
    journey.middle_press(from);
    journey.frames(2);
    journey.middle_drag_to(to);
    journey.frames(2);
    journey.middle_release(to);
    journey.frames(2);

    assert_eq!(
        journey.camera(),
        camera_before,
        "the scene's own camera is what the cart reports, and the pan did \
         not touch it (v1 limitation: `effective_camera` prefers the \
         component)"
    );
    assert_eq!(
        journey.screen_rect(BOX_A),
        screen_before,
        "so nothing moved on screen either"
    );
}
