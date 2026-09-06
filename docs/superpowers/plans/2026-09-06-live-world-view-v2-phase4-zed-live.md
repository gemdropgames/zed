# Live World View v2 — Phase 4 (Live geometry, loading screen, rail) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In Live the emulator picture fills the tab (integer-scaled, centered), the cart's reported camera places every outline and hit rect on that picture, click/drag/marquee/pan work in that space, a loading screen covers the boot, and the systems rail is a vertical list.

**Architecture:** A `LiveCamera` (frame origin, integer scale, cart camera) is derived per render from the canvas bounds, the user's wheel scale and the mailbox's camera report. It is expressed as a worldlib `View` (`zoom = scale`, `pan = origin − camera·scale`) so every existing screen↔world helper and the drag/marquee code keep working untouched. Design's `ViewShared` pan/zoom are no longer read in Live. The frame paints at the derived bounds; nothing from the Design renderer paints in Live.

**Tech Stack:** Rust, GPUI canvas painting, `ggo_worldlib::drag_ops::View`, `emerald_editor_link::LinkMailbox::camera()` (Phase 1).

**Spec:** `docs/superpowers/specs/2026-09-06-live-world-view-v2-design.md` ("Live view geometry", "Loading and failure", "Systems rail").

## Global Constraints

- Branch `live-world-view-v2`. Commit per task, no AI trailers. Gate: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`.
- Device frame is `DEVICE_SCREEN_W × DEVICE_SCREEN_H` (320×240, from `ggo_worldlib::render`).
- Scale is an integer `1..=LIVE_SCALE_MAX (8)`. Default is the largest that fits the canvas; a canvas smaller than 320×240 uses 1 and clips.
- Overlay placement uses the cart's reported camera only. Until the first report, rows are placed as if the camera were `(0, 0)`.
- Left drag on empty space is the marquee (as Design). Middle drag pans by sending `Camera`. Wheel changes the scale by one step, frame stays centered.
- Nothing from `canvas::paint_scene`, the grid, or the device-screen outline paints in Live.
- Loading screen while the endpoint is `Building`, or `Running` before both `HelloAck` and a first frame.
- No `unwrap()` outside tests; comments explain why only.

---

### Task 1: Pure geometry in `live.rs`

**Files:**
- Modify: `crates/ggo/world_panel/src/live.rs` (new section after `from_raw`; tests module)

**Interfaces:**
- Produces:

```rust
pub const LIVE_SCALE_MAX: u32 = 8;

/// The largest integer scale at which the device frame fits `canvas`, min 1.
pub fn fit_scale(canvas_w: f64, canvas_h: f64) -> u32;

/// Where the scaled frame sits: centered in the canvas (canvas-relative px).
pub fn frame_origin(canvas_w: f64, canvas_h: f64, scale: u32) -> [f64; 2];

/// The Live transform as a worldlib `View`: `screen = origin + (world − camera)·scale`.
pub fn live_view(origin: [f64; 2], scale: u32, camera: [f64; 2]) -> View;

/// `scale ± 1`, clamped to `1..=LIVE_SCALE_MAX`.
pub fn scale_step(scale: u32, dir: i32) -> u32;
```

- [ ] **Step 1: Write the failing tests**

In `live.rs`'s `mod tests`:

```rust
    #[test]
    fn fit_scale_is_the_largest_integer_that_fits_min_one() {
        assert_eq!(fit_scale(320.0, 240.0), 1);
        assert_eq!(fit_scale(640.0, 480.0), 2);
        assert_eq!(fit_scale(1000.0, 480.0), 2, "height limits");
        assert_eq!(fit_scale(2000.0, 2000.0), 8, "capped at LIVE_SCALE_MAX");
        assert_eq!(fit_scale(100.0, 100.0), 1, "too small still 1");
    }

    #[test]
    fn frame_origin_centers_the_scaled_frame() {
        assert_eq!(frame_origin(640.0, 480.0, 2), [0.0, 0.0]);
        assert_eq!(frame_origin(800.0, 600.0, 2), [80.0, 60.0]);
        assert_eq!(frame_origin(100.0, 100.0, 1), [-110.0, -70.0], "clipped when larger");
    }

    #[test]
    fn live_view_maps_world_through_camera_scale_and_origin() {
        let view = live_view([80.0, 60.0], 2, [10.0, 5.0]);
        let [sx, sy] = ggo_worldlib::drag_ops::world_to_screen(30.0, 25.0, &view);
        assert_eq!([sx, sy], [80.0 + 40.0, 60.0 + 40.0]);
        let back = ggo_worldlib::drag_ops::screen_to_world(sx, sy, &view);
        assert_eq!(back, [30.0, 25.0]);
    }

    #[test]
    fn scale_step_clamps() {
        assert_eq!(scale_step(1, -1), 1);
        assert_eq!(scale_step(1, 1), 2);
        assert_eq!(scale_step(8, 1), 8);
        assert_eq!(scale_step(5, -1), 4);
    }
```

If `drag_ops::world_to_screen` does not exist, compute `[30.0 * 2.0 + view.pan_x, 25.0 * 2.0 + view.pan_y]` inline instead.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ggo_world_panel --lib live::tests`
Expected: compile errors, functions missing.

- [ ] **Step 3: Implement**

```rust
use ggo_worldlib::drag_ops::View;
use ggo_worldlib::render::{DEVICE_SCREEN_H, DEVICE_SCREEN_W};

pub const LIVE_SCALE_MAX: u32 = 8;

pub fn fit_scale(canvas_w: f64, canvas_h: f64) -> u32 {
    let by_w = (canvas_w / DEVICE_SCREEN_W).floor();
    let by_h = (canvas_h / DEVICE_SCREEN_H).floor();
    // `as` saturates: a NaN or negative canvas size lands on 0, then 1.
    (by_w.min(by_h) as u32).clamp(1, LIVE_SCALE_MAX)
}

pub fn frame_origin(canvas_w: f64, canvas_h: f64, scale: u32) -> [f64; 2] {
    let scale = f64::from(scale);
    [
        ((canvas_w - DEVICE_SCREEN_W * scale) / 2.0).floor(),
        ((canvas_h - DEVICE_SCREEN_H * scale) / 2.0).floor(),
    ]
}

pub fn live_view(origin: [f64; 2], scale: u32, camera: [f64; 2]) -> View {
    let zoom = f64::from(scale);
    View {
        zoom,
        pan_x: origin[0] - camera[0] * zoom,
        pan_y: origin[1] - camera[1] * zoom,
        dpr: None,
    }
}

pub fn scale_step(scale: u32, dir: i32) -> u32 {
    let next = if dir > 0 { scale.saturating_add(1) } else { scale.saturating_sub(1) };
    next.clamp(1, LIVE_SCALE_MAX)
}
```

`frame_origin` floors so the frame lands on whole device pixels.

- [ ] **Step 4: Run tests**

Run: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib live::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/world_panel/src/live.rs
git commit -m "ggo_world_panel: live view geometry (fit scale, centered frame, camera view)"
```

---

### Task 2: `LiveView` carries scale and camera; the session sends the camera the cart should start at

**Files:**
- Modify: `crates/ggo/world_panel/src/live.rs` (`LiveView` struct + `new`)
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`live_step` ~lines 1316-1332 (HelloAck arm), 1369-1371 (`camera_moved`), 1461-1466 (camera push), 1488-1500 (rows/frame copy); `handle_pan_move` ~3739; `wheel_zoom` ~3768; `reset_view_impl`; `layout_camera` ~1107; `view_top_left_world` ~1088; `ViewShared.camera_moved` ~550)

**Interfaces:**
- Produces on `LiveView`:

```rust
    /// User-chosen integer scale; `None` = fit the canvas.
    pub scale: Option<u32>,
    /// The cart's camera in world px, from its per-frame report.
    pub camera: Option<[f64; 2]>,
    /// A camera the host owes the cart (a pan), flushed once per tick.
    pub pending_camera: Option<[f64; 2]>,
    /// A middle-drag in progress: cursor start and the camera it started from.
    pub pan_drag: Option<([f64; 2], [f64; 2])>,
```

`camera_dirty` is deleted; `ViewShared.camera_moved`, `layout_camera`'s `camera_moved = true`, and `view_top_left_world` go with it (Design's `layout_camera` keeps the centering; only the flag leaves).

- [ ] **Step 1: Write the failing tests**

In `ggo_world_panel.rs` tests, next to the Live session tests (~line 13254 onward). The existing helpers are: `live_panel(cx, &dir) -> (panel, endpoint, cx)` (~13003; after Phase 3 it fetches the panel via `panel::<WorldDock>` + `open_in_panel` + `dock.active()` — Phase 3 Task 2 updates it), `cart_says(&endpoint, bytes)`, `hello_ack(version, &[names])` (pass `emerald_editor_runtime::wire::LINK_PROTO_VERSION` now that it is 2), `cart_rows(&endpoint, &[(index, x, y)])` (~13080), `cart_frame(&endpoint, seq)` (~13119), `host_sent(&endpoint) -> Vec<Vec<u8>>`, `open_of(panel)`. Add one builder beside `cart_frame`:

```rust
    /// `0x89 Camera`: `x i32, y i32` Q16.16.
    fn cart_camera(endpoint: &ggo_common::LinkEndpoint, x: f64, y: f64) {
        let mut out = vec![0x89];
        out.extend_from_slice(&live::to_raw(x).to_le_bytes());
        out.extend_from_slice(&live::to_raw(y).to_le_bytes());
        cart_says(endpoint, out);
    }
```

and a `connected_live_panel(cx, &dir)` that wraps `live_panel` + `set_state(Running)` + `run_until_parked` + `cart_says(hello_ack(..))` + `run_until_parked` (the first lines of `live_mode_connects_on_hello_ack_and_sends_the_world` ~13284). Then:

```rust
    /// The cart's camera report, not the design pan, is what Live draws
    /// against: a report of (100, 50) puts a row at world (110, 60) ten
    /// px right and down of the frame's origin at scale 1.
    #[gpui::test]
    async fn live_overlay_rows_follow_the_carts_camera_report(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = connected_live_panel(cx, &dir).await;
        cart_rows(&endpoint, &[(0, 110.0, 60.0)]);
        cart_camera(&endpoint, 100.0, 50.0);
        cart_frame(&endpoint, 3);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let ViewerState::Ready(open) = &panel.state else { panic!() };
            let live = open.live.as_ref().expect("live");
            assert_eq!(live.camera, Some([100.0, 50.0]));
        });
        // 640x480 canvas -> scale 2, origin (0,0): the row's screen rect is
        // ((110-100)*2, (60-50)*2) = (20, 20), 32x32.
        let rect = panel.read_with(cx, |panel, _| panel.test_live_row_screen_rect(0, [640.0, 480.0]).expect("row"));
        assert_eq!(rect, [20.0, 20.0, 32.0, 32.0]);
    }

    #[gpui::test]
    async fn a_middle_drag_in_live_sends_the_camera_and_never_touches_the_design_pan(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = connected_live_panel(cx, &dir).await;
        cart_camera(&endpoint, 100.0, 50.0);
        cart_frame(&endpoint, 3);
        cx.run_until_parked();
        host_sent(&endpoint);
        let design_pan_before = panel.read_with(cx, |p, _| p.test_design_pan());
        panel.update(cx, |panel, cx| {
            panel.test_live_pan_begin([200.0, 200.0], cx);
            panel.test_live_pan_move([230.0, 210.0], [640.0, 480.0], cx);   // scale 2: delta (15, 5) world px
        });
        cx.run_until_parked();
        // `0x03 Camera`: `x i32, y i32`.
        let sent = host_sent(&endpoint).into_iter().rev().find(|m| m.first() == Some(&0x03)).expect("a Camera went out");
        let x = i32::from_le_bytes(sent[1..5].try_into().unwrap());
        let y = i32::from_le_bytes(sent[5..9].try_into().unwrap());
        assert_eq!((x, y), (live::to_raw(85.0), live::to_raw(45.0)));
        assert_eq!(panel.read_with(cx, |p, _| p.test_design_pan()), design_pan_before);
    }

    #[gpui::test]
    async fn wheel_in_live_steps_the_integer_scale(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, _endpoint, cx) = connected_live_panel(cx, &dir).await;
        panel.update(cx, |panel, cx| panel.test_live_wheel(1, [640.0, 480.0], cx));
        assert_eq!(panel.read_with(cx, |p, _| p.test_live_scale([640.0, 480.0])), 3, "fit 2 -> 3");
        panel.update(cx, |panel, cx| {
            for _ in 0..10 { panel.test_live_wheel(-1, [640.0, 480.0], cx); }
        });
        assert_eq!(panel.read_with(cx, |p, _| p.test_live_scale([640.0, 480.0])), 1);
    }
```

Test helpers to add on `WorldPanel` under `#[cfg(test)]`: `test_live_row_screen_rect(index, canvas_size) -> Option<[f64; 4]>` (through `live_camera_for` below and `canvas::item_bounds`), `test_design_pan() -> Option<[f64; 2]>`, `test_live_pan_begin/move`, `test_live_wheel(dir, canvas_size, cx)`, `test_live_scale(canvas_size) -> u32`. Also adapt `the_first_layout_hands_its_centering_to_the_cart` (~14818): the camera the cart receives is now the document's `active_camera_origin` sent on `HelloAck`, not the layout's top-left.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ggo_world_panel --lib live_overlay_rows_follow`
Expected: compile errors (`camera` field, helpers missing).

- [ ] **Step 3: Implement**

`LiveView`: add the four fields (`scale: None, camera: None, pending_camera: None, pan_drag: None` in `new`); delete `camera_dirty`.

`WorldPanel`:

```rust
    /// The Live transform for a canvas of `size` (canvas-relative px):
    /// the frame centered at the effective scale, offset by the cart's
    /// camera. `(view, frame bounds, scale)`.
    fn live_camera_for(&self, size: [f64; 2]) -> Option<(View, [f64; 4], u32)> {
        let ViewerState::Ready(open) = &self.state else { return None; };
        let live = open.live.as_ref()?;
        let scale = live.scale.unwrap_or_else(|| live::fit_scale(size[0], size[1]));
        let origin = live::frame_origin(size[0], size[1], scale);
        let camera = live.camera.unwrap_or([0.0, 0.0]);
        let s = f64::from(scale);
        Some((
            live::live_view(origin, scale, camera),
            [origin[0], origin[1], DEVICE_SCREEN_W * s, DEVICE_SCREEN_H * s],
            scale,
        ))
    }
```

`canvas_view()`: when `self.live_active()`, return `self.live_camera_for(size).map(|(view, ..)| view)` where `size` comes from `open.view.borrow().last_bounds` (the canvas stamps it in both modes); Design keeps its branch. Every Live gesture (`canvas_primary_down_with`, `canvas_drag_to`, double-click, marquee) already goes through `canvas_view()`, so they need no other change.

`live_step`:
- HelloAck arm: replace `camera_dirty = true` with `live.pending_camera = Some(active_camera_origin(&state))` — the document's own camera origin (the `active_camera_origin` the Design renderer frames) is where the cart starts. `state` is `open.store.state()` which the caller already has; thread it in.
- Delete the `camera_moved` block.
- Camera push: `} else if let Some([x, y]) = live.pending_camera.take() { live.mailbox.set_camera(live::to_raw(x), live::to_raw(y)) … }`.
- After the rows copy: `live.camera = live.mailbox.camera().map(|(x, y)| [live::from_raw(x), live::from_raw(y)]); changed |= live.camera != camera_before;`.

`handle_pan_move` / the middle-button down and up handlers (~lines 6023-6100 in `render_canvas`): when `live_active()`, drive `live.pan_drag` instead of `ViewShared.drag`:

```rust
    fn live_pan_begin(&mut self, cursor: [f64; 2]) {           // middle down
        if let ViewerState::Ready(open) = &mut self.state
            && let Some(live) = open.live.as_mut()
        {
            live.pan_drag = Some((cursor, live.camera.unwrap_or([0.0, 0.0])));
        }
    }
    fn live_pan_move(&mut self, cursor: [f64; 2], size: [f64; 2], cx: &mut Context<Self>) -> bool {
        let Some((_, _, scale)) = self.live_camera_for(size) else { return false; };
        let ViewerState::Ready(open) = &mut self.state else { return false; };
        let Some(live) = open.live.as_mut() else { return false; };
        let Some((start, camera0)) = live.pan_drag else { return false; };
        let s = f64::from(scale);
        // Dragging the picture right moves the camera left.
        live.pending_camera = Some([camera0[0] - (cursor[0] - start[0]) / s, camera0[1] - (cursor[1] - start[1]) / s]);
        cx.notify();
        true
    }
    fn live_pan_end(&mut self) { /* pan_drag = None */ }
```

While a pan is in flight the overlay uses `pending_camera.unwrap_or(camera)` so outlines and picture move together once the cart catches up — one frame of lag, as before, but no divergence.

`wheel_zoom`: when `live_active()`: `live.scale = Some(live::scale_step(current, dir))` where `current = live.scale.unwrap_or_else(|| fit_scale(..))`; `cx.notify()`; return. Design branch unchanged.

`reset_view_impl`: when `live_active()`, `live.scale = None` (fit) and `pending_camera = Some(active_camera_origin(..))`; else the Design reset.

Delete `view_top_left_world`, `ViewShared.camera_moved`, and the `camera_moved = true` line in `layout_camera`.

- [ ] **Step 4: Run tests**

Run: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`
Expected: PASS. Existing Live tests that asserted a `Camera` datagram after a pan/zoom of the design view (grep `set_camera\|Camera` in the tests) now assert the HelloAck-time camera and the middle-drag camera instead.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/world_panel/src
git commit -m "ggo_world_panel: live gestures run in the cart's camera space"
```

---

### Task 3: Paint the frame centered and scaled; the loading screen

**Files:**
- Modify: `crates/ggo/world_panel/src/canvas.rs` (`LiveScene` ~484, `live_frame_bounds` ~505 deleted, `paint_live` ~520, tests ~827-840)
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`render_canvas` Live branch ~5923-5953; `fn render_live_loading`)
- Modify: `crates/ggo/world_panel/src/world_canvas_item.rs` (`render`: ask the panel for the loading screen)

**Interfaces:**
- Produces:

```rust
pub struct LiveScene {
    pub frame: Option<Arc<RenderImage>>,
    /// Canvas-relative frame rect `[x, y, w, h]`.
    pub frame_rect: [f64; 4],
    pub view: View,                     // from live::live_view
    pub rows: Vec<(Selection, [f64; 4], bool)>,
    pub marquee: Option<[f64; 4]>,
    pub background: Hsla,
    pub accent: Hsla,
}
pub fn paint_live(scene: &LiveScene, canvas_bounds: Bounds<Pixels>, window: &mut Window);
```

and on `WorldPanel`: `pub(crate) fn live_loading_text(&self) -> Option<String>` — `Some("Building viewer cart…")` while the endpoint is `Building`, `Some(format!("Booting viewer for {stem}…"))` while `Running` without `Connected` + a frame, `None` otherwise (including Design mode and failures).

- [ ] **Step 1: Write the failing tests**

`canvas.rs` tests: replace `live_frame_bounds_scale_the_device_screen_by_zoom` with:

```rust
    #[test]
    fn paint_live_places_the_frame_at_the_scene_rect() {
        // Pure check of the rect math the painter uses: a scene rect
        // [80, 60, 640, 480] on a canvas at (10, 10) paints at (90, 70).
        let b = live_frame_bounds_px(bounds(point(px(10.), px(10.)), size(px(800.), px(600.))), [80.0, 60.0, 640.0, 480.0]);
        assert_eq!(b.origin, point(px(90.), px(70.)));
        assert_eq!(b.size, size(px(640.), px(480.)));
    }
```

`ggo_world_panel.rs` tests:

```rust
    #[gpui::test]
    async fn live_shows_a_loading_screen_until_connected_with_a_frame(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;   // endpoint still Building
        assert_eq!(panel.read_with(cx, |p, _| p.live_loading_text()), Some("Building viewer cart…".into()));
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |p, _| p.live_loading_text()), Some("Booting viewer for test…".into()));
        cart_says(&endpoint, hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &[]));
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |p, _| p.live_loading_text()), Some("Booting viewer for test…".into()), "connected but no frame yet");
        // A published frame: what the emu side does per presented frame.
        let image = Arc::new(gpui::RenderImage::new(vec![image::Frame::new(image::ImageBuffer::from_pixel(320, 240, image::Rgba([0u8, 0, 0, 255])))]));
        *endpoint.frame.lock().unwrap() = Some((1, image));
        endpoint.tick();
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |p, _| p.live_loading_text()), None);
        panel.update_in(cx, |p, window, cx| p.set_canvas_mode(CanvasMode::Design, window, cx));
        assert_eq!(panel.read_with(cx, |p, _| p.live_loading_text()), None, "Design never loads");
    }
```

(`image` is already a dependency of the crate via `canvas::build_image_cache`; if `image::ImageBuffer::from_pixel` is unavailable use `from_raw(320, 240, vec![0u8; 320 * 240 * 4])`.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ggo_world_panel --lib live_shows_a_loading_screen`
Expected: compile error.

- [ ] **Step 3: Implement**

`canvas.rs`:

```rust
/// The scene's canvas-relative frame rect in window px.
pub fn live_frame_bounds_px(canvas_bounds: Bounds<Pixels>, rect: [f64; 4]) -> Bounds<Pixels> {
    bounds(
        point(canvas_bounds.origin.x + px(rect[0] as f32), canvas_bounds.origin.y + px(rect[1] as f32)),
        size(px(rect[2] as f32), px(rect[3] as f32)),
    )
}

pub fn paint_live(scene: &LiveScene, canvas_bounds: Bounds<Pixels>, window: &mut Window) {
    window.with_content_mask(Some(ContentMask { bounds: canvas_bounds }), |window| {
        window.paint_quad(fill(canvas_bounds, scene.background));
        if let Some(frame) = &scene.frame {
            let b = live_frame_bounds_px(canvas_bounds, scene.frame_rect);
            if let Err(error) = window.paint_image(b, b, Corners::default(), frame.clone(), 0, false, true) {
                log::warn!("GGO: live frame paint failed: {error}");
            }
        }
        for (_, [x, y, w, h], selected) in &scene.rows {
            let b = item_bounds(&scene.view, canvas_bounds.origin, *x, *y, *w, *h);
            let mut color = scene.accent;
            if !*selected { color.a *= LIVE_ROW_DIM_ALPHA; }
            window.paint_quad(outline(b, color, BorderStyle::default()));
        }
        if let Some([x, y, w, h]) = scene.marquee {
            let b = item_bounds(&scene.view, canvas_bounds.origin, x, y, w, h);
            window.paint_quad(outline(b, color(MARQUEE_COLOR), BorderStyle::default()));
        }
    });
}
```

Delete `live_frame_bounds`, the `zoom`/`pan`/`grid` fields, and update the `LiveScene` doc: rows are world px, placed through `view`, which is the cart's camera.

`render_canvas` Live branch: the prepaint closure stamps `last_bounds` (keep calling `layout_camera` for that, or stamp directly) and builds:

```rust
                move |canvas_bounds, _window, _cx| {
                    let size = [f64::from(canvas_bounds.size.width), f64::from(canvas_bounds.size.height)];
                    view_shared.borrow_mut().last_bounds = Some(canvas_bounds);
                    let scale = scale_override.unwrap_or_else(|| live::fit_scale(size[0], size[1]));
                    let origin = live::frame_origin(size[0], size[1], scale);
                    let s = f64::from(scale);
                    canvas::LiveScene {
                        frame,
                        frame_rect: [origin[0], origin[1], DEVICE_SCREEN_W * s, DEVICE_SCREEN_H * s],
                        view: live::live_view(origin, scale, camera),
                        rows, marquee, background, accent,
                    }
                },
```

with `scale_override = live.scale`, `camera = live.pending_camera.or(live.camera).unwrap_or([0.0, 0.0])` captured before the closure (the paint closure must not read the panel).

Loading: `WorldPanel::live_loading_text()` as specified. `WorldCanvasItem::render`: before `render_canvas`, `if let Some(text) = panel.live_loading_text()` render

```rust
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .debug_selector(|| "ggo-world-live-loading".into())
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::Medium)
                            .color(Color::Accent)
                            .with_rotate_animation(2),
                    )
                    .child(Label::new(text).color(Color::Muted))
                    .into_any_element()
```

(the spinner idiom `hardware_item.rs:640` uses).

- [ ] **Step 4: Run tests**

Run: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/world_panel/src
git commit -m "ggo_world_panel: centered integer-scaled live frame, boot screen"
```

---

### Task 4: Vertical systems rail

**Files:**
- Modify: `crates/ggo/world_panel/src/ggo_world_panel.rs` (`render_systems_rail` ~5646-5700)

- [ ] **Step 1: Write the failing test**

Next to the existing systems-rail test (grep `ggo-world-system-0`):

```rust
    #[gpui::test]
    async fn the_systems_rail_is_a_vertical_list(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (panel, endpoint, cx) = live_panel(cx, &dir).await;
        endpoint.set_state(ggo_common::ViewerState::Running);
        cx.run_until_parked();
        cart_says(&endpoint, hello_ack(emerald_editor_runtime::wire::LINK_PROTO_VERSION, &["animate", "physics"]));
        cx.run_until_parked();
        let (first, second) = panel.update_in(cx, |_, window, _| {
            let a = window.debug_bounds("ggo-world-system-0-off").expect("row 0");
            let b = window.debug_bounds("ggo-world-system-1-off").expect("row 1");
            (a, b)
        });
        assert_eq!(first.origin.x, second.origin.x, "same column");
        assert!(second.origin.y > first.origin.y, "stacked");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ggo_world_panel --lib the_systems_rail_is_a_vertical_list`
Expected: FAIL (`h_flex` puts them side by side).

- [ ] **Step 3: Implement**

`let mut rail = v_flex().gap_1().px_1().pb_1().child(Label::new("Systems")…);` — drop `flex_wrap()`. Each checkbox row stays as is.

- [ ] **Step 4: Run tests**

Run: `./script/clippy -p ggo_world_panel && cargo test -p ggo_world_panel --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ggo/world_panel/src/ggo_world_panel.rs
git commit -m "ggo_world_panel: systems rail as a vertical list"
```

---

### Task 5: Whole-fork gate, manual check, review, merge

- [ ] **Step 1: Gate**

```bash
for c in ggo_common ggo_world_panel ggo_emu_panel ggo_emerald_panel ggo_smoke; do ./script/clippy -p $c && cargo test -p $c --lib || exit 1; done
```

- [ ] **Step 2: Manual check in the running app**

Open a world in Live: loading screen, then the frame centered and integer-scaled filling the tab; outlines sit on the sprites; click selects, drag moves the sprite in the picture, marquee selects, middle-drag pans the picture with the outlines; wheel steps the scale; switch a system on (vertical list) that moves the camera and the outlines keep following; open a second world in a second tab; close it; no emulator tab ever appears. Note anything off in the review brief.

- [ ] **Step 3: Review**

Fresh opus reviewer over `git diff ggo...live-world-view-v2` (whole branch) for practices and for every section of the spec. Fix findings; re-run the gate; commit.

- [ ] **Step 4: Merge**

```bash
git checkout ggo && git merge --ff-only live-world-view-v2 && git push origin ggo ggo:main
```

Then update the memory file `project-live-world-view-plans.md` with the new state.
