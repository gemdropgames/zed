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

const NO_SYSTEMS: emerald_editor_runtime::link::SystemTable = &[];
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
    WorldEntity {
        components: json!({ "Transform": { "pos": pos, "z": 0.0 } })
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
    _dir: tempfile::TempDir,
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
        _dir: dir,
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
                    || !live.pending_transforms.is_empty()
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

    /// Pick a tool through the rail's own handler, as the radio does.
    fn set_tool(&mut self, tool: u8) {
        self.panel
            .update(self.cx, |panel, cx| panel.set_live_tool(tool, cx));
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
        _dir,
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
