# Live World View v2 Design

Date: 2026-09-06
Status: Approved design, pre-implementation
Supersedes the view/ownership parts of `2026-09-04-live-world-view-design.md`;
the link protocol, emerald runtime and document side of that spec stand.

## Problem

Phase 3 of the live world view shipped with four defects the user hit on
first use:

1. The device frame paints at the canvas's top-left, scaled by the Design
   zoom (fit-to-world, so small), on the Design background: a black box in
   the corner until the world blob lands, and never the whole view.
2. Entity outlines are placed in Design world coordinates on the
   assumption that the cart's camera equals the canvas's top-left world
   point. The cart never reports its camera, so anything that moves it (a
   dropped datagram, a cart system such as camera follow) leaves the
   outlines detached from the picture. Row sizes are a hardcoded 16x16 on
   the cart, so the outlines do not match the drawn sprites either.
3. Entering Live opens (quietly) the emulator tab, because the viewer cart
   runs in the single `EmuPanel` run slot. Only one viewer can exist.
4. One world is open at a time: the dock `WorldPanel` owns the document
   and the center tab is a view of it.

The user's requests, verbatim in intent: the emulator view is the whole
view or centered; click/drag and bounding boxes work in the emulator view;
no emulator tab, the world view merely uses emulation; several world
tabs at once, each with its own emulator; a loading screen while the
viewer boots; nothing from the Design renderer under the Live picture;
the systems toggle is a vertical list.

## Decisions

- Multiple different worlds open at once. Each center tab owns its world
  document and its viewer emulator. The dock follows the active tab.
- Live camera: the frame is scaled by an integer factor (largest that
  fits, wheel zoom changes it), centered and letterboxed. The cart owns the
  camera and reports it every frame; the host places overlays from that
  report.
- Design mode stays as a separate mode and as the boot-failure fallback.
  In Live nothing from the Design renderer is painted.
- Emulator runs for viewers are headless: no `EmuPanel` state, no tab.

## Architecture

```
WorldDock (registered Panel, one per workspace)
  active: WeakEntity<WorldPanel>  -- follows the active WorldCanvasItem
  renders active's rails/inspector, or "No world open"
  open_world(rel): find the tab for rel, or make WorldPanel + WorldCanvasItem

WorldCanvasItem (center tab)  --owns-->  WorldPanel (document, ops, undo,
                                            selection, Live state)
                                            live_endpoint: Arc<LinkEndpoint>
                                                   |
                                     ggo_common::boot_viewer(workspace, rel)
                                                   |
                              ViewerRun (Entity in ggo_emu_panel, headless)
                                build task -> drive::Session (audio None)
                                pump task -> frame -> RenderImage -> endpoint.frame
```

### `ggo_emu_panel`: `ViewerRun`

A new entity, one per viewer. It owns:

- the editor-cart build (`emd editor-cart --ggo` through the existing proc
  runner), sharing `prepare_viewer_build` and the build-output parsing with
  `EmuPanel` (those move to free functions; `EmuPanel` keeps calling them
  for its own runs);
- one `drive::Session` started with `audio: None` and `link:
  Some(endpoint)`;
- the pump task over the session's frame channel: each `Frame` becomes a
  `RenderImage`, is published into `endpoint.frame`, `endpoint.tick()` is
  called, and the image replaced one publish ago is retired with
  `cx.drop_image(stale, None)` (the same one-late rule `EmuPanel::on_frame`
  follows today);
- endpoint state: `Building` while building, `Running` once the session
  is up, `Stopped(reason)` on any failure, run exit, or a stop request.

It stops when `endpoint.stop_requested()` is seen at a frame boundary, or
when the entity is dropped (its `Session` stops on `Drop`). It never
touches `EmuPanel`'s status row, run kind, or item. `boot_viewer`,
`RunKind::Viewer`, `viewer_link`, `previously_published`,
`release_link_of_current_run` and `stop_for_world_panel` leave `EmuPanel`.

The registry hook keeps its shape: `ggo_common::boot_viewer(workspace,
world_rel, window, cx) -> Option<Arc<LinkEndpoint>>`. The emu crate's
registered booter creates a `ViewerRun` and parks it in a workspace-scoped
registry (`Vec<Entity<ViewerRun>>` on a global keyed by workspace entity
id) that prunes runs whose endpoint is `Stopped`. Many runs coexist, one
emulator thread each.

Watch mode: `EmuPanel`'s source watcher already debounces "Rust sources or
assets changed". It additionally emits a global `ViewerSourcesChanged`
event; every `ViewerRun` rebuilds on it (endpoint `Building` again, then
`Running`). The world panel already re-greets after a reboot.

### `ggo_world_panel`: per-tab documents, thin dock

- `WorldPanel` stops implementing `Panel`. It is the per-document entity:
  everything it has today (state, ops, undo/redo, selection, clipboard,
  paint, audio budget, Live state, `live_endpoint`) plus a `WeakEntity<
  Workspace>`. Its `Render` keeps drawing what the dock shows today: the
  toolbar, mode switch, status rows, rails, lists, inspector.
- New `WorldDock` implements `Panel` with the existing key
  `GGO_WORLD_PANEL_KEY` and persistent name, so layout persistence and the
  dock button are unchanged. It holds `active: Option<WeakEntity<
  WorldPanel>>`, `canvas_mode: CanvasMode` (the sticky default new tabs
  open in) and `live_sys_mask` (session-wide, as today). It subscribes to
  the workspace's active item changes; when the active item is a
  `WorldCanvasItem`, `active` becomes its panel. Render: `active`'s element
  or "No world open".
- `WorldDock::open_world(rel, window, cx)`: if a `WorldCanvasItem` for
  `rel` exists in any pane, activate it; else create `WorldPanel`, load
  `rel`, create the item, add it to the active pane. Every path that
  opened a world through the panel (explorer double-click, the emerald
  panel's five call sites, MCP `world_open`, `ggo_smoke`) calls this.
  `world_read` and the other readers use `WorldDock::active()`.
- `WorldCanvasItem` holds `Entity<WorldPanel>` (strong). Its tab title,
  dirty state, save, reload are unchanged. Closing the tab drops the item,
  the panel, and the panel's endpoint (`request_stop` in `Drop`), which
  ends the emulator thread.
- Live entry no longer reuses an endpoint per emerald project; each panel
  boots its own viewer. The `live_endpoint` project-key logic goes.

### Live view geometry

New `LiveView` fields: `scale: u32` and `camera: Option<[i32; 2]>` (raw
Q16.16 from the cart). Pure functions in `live.rs`, all unit tested:

- `fit_scale(canvas: Size) -> u32`: largest `s ≥ 1` with `320·s ≤ w` and
  `240·s ≤ h`; 1 when nothing fits.
- `frame_bounds(canvas, scale) -> Bounds`: `320·s × 240·s`, centered in
  the canvas, clipped by the canvas content mask when larger.
- `world_to_screen(frame_origin, scale, camera, world) -> Point` and its
  inverse. Rows: `screen = frame_origin + (row − camera) · scale`.
- Wheel: `scale ± 1` per notch, clamped `1..=8`; the frame stays centered.
  The first layout after entering Live (or after a canvas resize with no
  user zoom yet) uses `fit_scale`.

`paint_live` paints: the theme background over the whole canvas, the frame
(nearest), row outlines (selected bright, others dimmed as today), the
marquee. No grid, no device-screen outline, no Design items. With no frame
yet, only the background.

Gestures in Live use the Live transform, not `canvas_view()`: hit-test,
marquee, drag origin and delta (delta ÷ scale = world px, then the existing
snap + `SetTransform` mirror + `MoveEntity` on drop, untouched). Left drag on empty
space is the marquee, as in Design. Middle drag pans: it sends `Camera` with the cart's
last reported camera minus the drag delta ÷ scale, and the overlay keeps
using the cart's report, so a cart system that also moves the camera never
fights the outlines. Design's `ViewShared` pan/zoom are not touched by
Live gestures.

### Loading and failure

While the panel is in Live and the endpoint is `Building`, or `Running`
without both a `HelloAck` and a first frame, the tab renders a centered
`Icon::ArrowCircle` spinner with "Booting viewer for `<stem>`…" (or
"Building viewer cart…") instead of the canvas. The existing deadlines
(`BUILD_DEADLINE`, `CONNECT_DEADLINE`, `STALE_AFTER`) and
`fall_back_to_design` stay; failure shows the reason on the status row and
the Design canvas.

### Systems rail

`v_flex`, one row per system: checkbox then name. Same gating and
`debug_selector`s as today.

## Protocol change (emerald)

- New `CartMsg::Camera { x: i32, y: i32 }`, kind `0x89`, Q16.16 world
  offset of the cart's `Camera` resource. Emitted every frame beside
  `FrameSeq` while a host is connected (9 bytes; unconditional so a lost
  datagram heals next frame).
- Entity rows publish the drawn footprint: `w, h = MetaSprite::size()`,
  and `x, y` = transform + `offset` (minus half the size when `centered`).
  Entities without a `MetaSprite` keep 16x16 at the transform.
- `LinkMailbox::camera() -> Option<(i32, i32)>` on the host side, updated
  from the message.
- `LINK_PROTO_VERSION` 1 → 2. The host's existing "viewer cart predates
  the link protocol; rebuild it" path covers older carts.
- Tests: wire round trip for `Camera`; `link.rs` publishes it each frame
  and the in-memory `LinkIo` pair test reads it back through the mailbox;
  a row test with a sized, offset, centered `MetaSprite`.

## Testing (zed)

- `ggo_emu_panel`: a `ViewerRun` over a fake cart publishes frames into
  its endpoint and retires the previous image; `request_stop` ends it with
  `Stopped`; two runs coexist with independent endpoints; booting a viewer
  leaves `EmuPanel`'s `session`, `run_kind` and status untouched and opens
  no `EmulatorItem`.
- `ggo_world_panel`: geometry unit tests (fit scale for several canvas
  sizes, centering, wheel clamp, `world_to_screen` with a non-zero camera,
  hit-test through that transform); overlay rows from injected `Entities`
  + `Camera`; loading render while `Building`; `WorldDock::open_world`
  twice with two worlds yields two tabs, two panels, two endpoints, and the
  dock follows pane activation; closing a tab sets its endpoint's stop
  flag; the systems rail renders vertically. Existing Design and Live
  tests keep passing (adjusted for the entity split).
- `ggo_smoke` journeys re-pointed at `WorldDock`.
- Gates: `./script/clippy -p <crate> && cargo test -p <crate> --lib` per
  crate; `cargo clippy --workspace --all-targets -- -D warnings && cargo
  test --workspace` in emerald.

## Phases

1. emerald: `Camera` message, real row footprints, version bump.
2. zed `ggo_emu_panel` + `ggo_common`: `ViewerRun`, registry, `EmuPanel`
   cleanup.
3. zed `ggo_world_panel` (+ callers): `WorldDock` split, multi-tab.
4. zed `ggo_world_panel`: Live geometry, loading screen, vertical rail.

Each phase on its own branch, reviewed by a fresh opus subagent, then
merged (zed: `ggo`, and `main` fast-forwarded; emerald: `main`).

## Out of scope

- Zoom below 1x, fractional scales.
- Persisting the systems mask or the canvas mode across restarts.
- Sharing one viewer between two tabs of the same world.
- Moving `encode_world` off the UI thread (still a parked follow-up).
- Hardware peer (phase 4 of the original spec).
