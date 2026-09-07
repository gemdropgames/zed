//! Live mode's pure half: the link transport over the emu panel's
//! endpoint, the cart-index <-> document-selection map (the encoder's
//! order: direct entities, then each instance's subtree depth-first),
//! the overlay rows the canvas outlines, and the payload builders.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use emerald_editor_link::{EditCommand, EditorMode, EntityRow, LinkIo, LinkMailbox};
use ggo_worldlib::backgrounds::MergedBackground;
use ggo_worldlib::drag_ops::View;
use ggo_worldlib::render::{DEVICE_SCREEN_H, DEVICE_SCREEN_W, Selection};
use ggo_worldlib::world_doc::{WorldDocStore, WorldInstance, WorldState};
use ggo_worldlib::world_file::world_to_toml;
use gpui::{RenderImage, Task};

use crate::loader;

/// [`LinkIo`] over the emu panel's endpoint: frames payloads as APP
/// datagrams on the way out, hands back the already-decoded APP payloads
/// the emulator thread collected on the way in.
pub struct EndpointIo(pub Arc<ggo_common::LinkEndpoint>);

impl LinkIo for EndpointIo {
    fn send(&mut self, payload: &[u8]) -> std::io::Result<()> {
        // `LinkEndpoint::send_app` is the one APP-framing site on this side
        // of the link (Phase 2 review): framing here as well would be a
        // second copy of the wire format to keep in step.
        self.0
            .send_app(payload)
            .map_err(|reason| std::io::Error::new(std::io::ErrorKind::InvalidInput, reason))
    }

    fn recv(&mut self) -> Vec<Vec<u8>> {
        self.0.try_recv_inbound()
    }
}

/// Flattened cart index -> document selection, in the encoder's order:
/// the world's direct entities `0..n`, then each `[[instance]]`'s whole
/// subtree, depth-first, in `[[instance]]` order.
pub struct IndexMap {
    entries: Vec<Selection>,
}

impl IndexMap {
    /// `instance_counts[i]` is the number of entities instance `i`
    /// contributes (its whole subtree, depth-first).
    pub fn new(direct_entities: usize, instance_counts: &[usize]) -> Self {
        let mut entries: Vec<Selection> = (0..direct_entities).map(Selection::Entity).collect();
        for (instance, count) in instance_counts.iter().enumerate() {
            entries.extend(std::iter::repeat_n(Selection::Instance(instance), *count));
        }
        IndexMap { entries }
    }
}

/// The lookups the Live mirror needs: turning a row the cart published
/// back into a document selection, and back again to name the cart
/// indices a document item owns.
impl IndexMap {
    pub fn selection_of(&self, cart_index: u32) -> Option<Selection> {
        self.entries.get(cart_index as usize).copied()
    }

    /// Each `[[instance]]`'s `(first cart index, count)` run, in instance
    /// order -- the group table the cart selects and drags whole. Derived
    /// from the map rather than from the counts it was built with so the
    /// ranges can never disagree with the indices the rows are resolved
    /// through.
    pub fn instance_ranges(&self) -> Vec<(u32, u32)> {
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        let mut open: Option<(usize, u32, u32)> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            let instance = match entry {
                Selection::Instance(instance) => Some(*instance),
                Selection::Entity(_) => None,
            };
            match (open, instance) {
                (Some((current, first, count)), Some(instance)) if current == instance => {
                    open = Some((current, first, count + 1));
                    continue;
                }
                _ => {}
            }
            if let Some((_, first, count)) = open.take() {
                ranges.push((first, count));
            }
            if let Some(instance) = instance {
                // `as` cannot lose anything a cart index can hold: the map
                // is indexed by the same `u32` the wire carries.
                open = Some((instance, index as u32, 1));
            }
        }
        if let Some((_, first, count)) = open {
            ranges.push((first, count));
        }
        ranges
    }

    /// Every cart index a document item owns: the one row a direct entity
    /// publishes, or an `[[instance]]`'s whole subtree.
    pub fn indices_of(&self, selection: Selection) -> Vec<u32> {
        self.entries
            .iter()
            .enumerate()
            // `as` cannot lose anything a cart index can hold: the map is
            // indexed by the same `u32` the wire carries.
            .filter_map(|(index, entry)| (*entry == selection).then_some(index as u32))
            .collect()
    }

    /// Read only by the tests: the map's production users address it by
    /// index, never by size. Kept because "how many cart indices does this
    /// document flatten to" is exactly what an index-map test asserts.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// How big the open document is, in the two frames a [`Selection`]
/// indexes. Carried rather than re-derived because `WorldDocStore::state`
/// deep-clones the whole document: the overlay asks "does this still
/// exist?" once per published row per render, and that is not a question
/// worth a document clone each time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocCounts {
    pub entities: usize,
    pub instances: usize,
}

impl DocCounts {
    /// Whether `selection` still indexes something. The index map is
    /// rebuilt from counts that can be one tick behind an instance edit,
    /// so a cart row can name a selection the document no longer has.
    pub fn contains(&self, selection: Selection) -> bool {
        match selection {
            Selection::Entity(index) => index < self.entities,
            Selection::Instance(index) => index < self.instances,
        }
    }
}

/// One published row from the cart, in world pixels. `x`/`y` are the
/// entity's TRANSFORM -- what a drag writes back through `SetTransform` --
/// while the sprite is DRAWN at `(x + ox, y + oy)` sized `(w, h)`. The two
/// differ for a centered sprite, so the overlay uses [`CartRow::drawn`]
/// and only the document mirror uses `x`/`y`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CartRow {
    pub index: u32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub ox: f64,
    pub oy: f64,
}

impl CartRow {
    /// The rect the cart actually draws: `[x, y, w, h]` shifted by the
    /// sprite's draw offset.
    pub fn drawn(&self) -> [f64; 4] {
        [self.x + self.ox, self.y + self.oy, self.w, self.h]
    }
}

pub fn rows_from(entities: &[EntityRow]) -> Vec<CartRow> {
    entities
        .iter()
        .map(|entity| CartRow {
            index: entity.index,
            x: from_raw(entity.x),
            y: from_raw(entity.y),
            w: f64::from(entity.w),
            h: f64::from(entity.h),
            ox: f64::from(entity.ox),
            oy: f64::from(entity.oy),
        })
        .collect()
}

/// The Live overlay: one entry per published cart rect that still maps to
/// something in the document -- the selection it stands for, its world
/// rect, and whether it is selected. Empty until the cart has republished
/// for the world blob the panel last sent, because rows from the PREVIOUS
/// world would otherwise be outlined over a frame that no longer holds
/// them.
pub fn overlay_rows(
    live: &LiveView,
    counts: DocCounts,
    selected: &[Selection],
) -> Vec<(Selection, [f64; 4], bool)> {
    if !live.loaded() {
        return Vec::new();
    }
    live.rows
        .iter()
        .filter_map(|row| {
            let selection = live.index_map.selection_of(row.index)?;
            counts
                .contains(selection)
                .then(|| (selection, row.drawn(), selected.contains(&selection)))
        })
        .collect()
}

/// What moved between two states of the same document, when NOTHING else
/// changed: each moved item, where it is now, and how far it travelled.
///
/// `None` the moment anything a `SetTransform` cannot carry differs -- an
/// added or removed item, another field, a background slot -- because
/// only the world blob can describe that to the cart. This is how an undo
/// step, which the store is opaque about, is told from a structural one.
pub fn moves_between(
    before: &WorldState,
    after: &WorldState,
) -> Option<Vec<(Selection, [f64; 2], [f64; 2])>> {
    if before.entities.len() != after.entities.len()
        || before.instances.len() != after.instances.len()
        || before.backgrounds != after.backgrounds
    {
        return None;
    }
    let mut moves = Vec::new();
    for (index, (was, now)) in before.entities.iter().zip(&after.entities).enumerate() {
        if was == now {
            continue;
        }
        let (Some(from), Some(to)) = (
            crate::inspector::transform_pos(was),
            crate::inspector::transform_pos(now),
        ) else {
            return None;
        };
        // Rewriting the old entity's position and asking whether it is now
        // the new one is what proves the position was the ONLY difference:
        // a `SetField` on some other field of the same entity is not
        // something the cart can be told with a transform.
        let mut moved = was.clone();
        crate::inspector::set_transform_pos(&mut moved.components, to);
        if moved != *now {
            return None;
        }
        moves.push((
            Selection::Entity(index),
            to,
            [to[0] - from[0], to[1] - from[1]],
        ));
    }
    for (index, (was, now)) in before.instances.iter().zip(&after.instances).enumerate() {
        if was == now {
            continue;
        }
        let moved = WorldInstance {
            pos: now.pos,
            ..was.clone()
        };
        if moved != *now {
            return None;
        }
        moves.push((
            Selection::Instance(index),
            now.pos,
            [now.pos[0] - was.pos[0], now.pos[1] - was.pos[1]],
        ));
    }
    Some(moves)
}

/// Pixels -> the runtime's Q16.16 fixed point. Rounds to the nearest raw
/// unit rather than truncating, so a drag never drifts a sub-unit per step;
/// the `as` cast saturates at the bounds (and maps NaN to zero), which is
/// the clamp a wildly out-of-range world coordinate needs.
pub fn to_raw(px: f64) -> i32 {
    (px * 65536.0).round() as i32
}

pub fn from_raw(raw: i32) -> f64 {
    f64::from(raw) / 65536.0
}

/// The largest integer scale the emulator picture is drawn at, so a huge
/// tab doesn't blow one device pixel up past a readable block.
pub const LIVE_SCALE_MAX: u32 = 8;

/// The largest integer scale at which the device frame fits `canvas`, min 1.
pub fn fit_scale(canvas_w: f64, canvas_h: f64) -> u32 {
    let by_w = (canvas_w / DEVICE_SCREEN_W).floor();
    let by_h = (canvas_h / DEVICE_SCREEN_H).floor();
    // `as` saturates: a NaN or negative canvas size lands on 0, then 1.
    (by_w.min(by_h) as u32).clamp(1, LIVE_SCALE_MAX)
}

/// Where the scaled frame sits: centered in the canvas (canvas-relative px).
/// Floored so the frame lands on whole device pixels.
pub fn frame_origin(canvas_w: f64, canvas_h: f64, scale: u32) -> [f64; 2] {
    let scale = f64::from(scale);
    [
        ((canvas_w - DEVICE_SCREEN_W * scale) / 2.0).floor(),
        ((canvas_h - DEVICE_SCREEN_H * scale) / 2.0).floor(),
    ]
}

/// The Live transform as a worldlib `View`: `screen = origin + (world - camera) * scale`.
pub fn live_view(origin: [f64; 2], scale: u32, camera: [f64; 2]) -> View {
    let zoom = f64::from(scale);
    View {
        zoom,
        pan_x: origin[0] - camera[0] * zoom,
        pan_y: origin[1] - camera[1] * zoom,
        dpr: None,
    }
}

/// The whole Live geometry for a canvas of `size` (canvas-relative px):
/// the transform world px are placed through, the frame rect
/// `[x, y, w, h]` the picture is drawn into, and the effective scale.
/// `scale_override` is the user's wheel choice; `None` fits the canvas.
///
/// One function because the paint closure -- which cannot read the panel
/// -- and the gestures, which can, must never disagree about where the
/// picture is: an outline placed by one and hit-tested by the other is
/// exactly the defect this view was rebuilt to fix.
pub fn geometry(
    size: [f64; 2],
    scale_override: Option<u32>,
    camera: [f64; 2],
) -> (View, [f64; 4], u32) {
    let scale = scale_override.unwrap_or_else(|| fit_scale(size[0], size[1]));
    let origin = frame_origin(size[0], size[1], scale);
    let scaled = f64::from(scale);
    (
        live_view(origin, scale, camera),
        [
            origin[0],
            origin[1],
            DEVICE_SCREEN_W * scaled,
            DEVICE_SCREEN_H * scaled,
        ],
        scale,
    )
}

/// `scale` +/- 1, clamped to `1..=LIVE_SCALE_MAX`.
pub fn scale_step(scale: u32, dir: i32) -> u32 {
    let next = if dir > 0 {
        scale.saturating_add(1)
    } else {
        scale.saturating_sub(1)
    };
    next.clamp(1, LIVE_SCALE_MAX)
}

/// One pointer sample as the cart reads it: where the cursor is in DEVICE
/// pixels, which buttons are held (bit 0 left, 1 middle, 2 right), which
/// modifiers (bit 0 shift, 1 ctrl/cmd, 2 alt), and the host's snap toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerState {
    pub device: (i16, i16),
    pub buttons: u8,
    pub modifiers: u8,
    pub snap: bool,
}

/// Canvas-relative px -> the cart's device px: back out the frame origin
/// and the integer scale [`geometry`] drew the picture at. Floored, so
/// every canvas px inside one device pixel names that pixel rather than
/// the next one; the `as` cast saturates at the `i16` bounds (and maps
/// NaN to zero), which is the clamp a cursor far outside the frame needs.
///
/// The inverse of the transform the picture is painted through, and it
/// has to stay that way: the cart hit-tests what the host aimed at.
pub fn device_from_canvas(local: [f64; 2], frame_rect: [f64; 4], scale: u32) -> (i16, i16) {
    let scale = f64::from(scale.max(1));
    let device = |value: f64, origin: f64| ((value - origin) / scale).floor() as i16;
    (
        device(local[0], frame_rect[0]),
        device(local[1], frame_rect[1]),
    )
}

/// How many pointer samples may wait for the wire. The queue drains one
/// per tick and a tick is a cart frame, so it only fills while the link
/// is not moving at all. Overflow drops the oldest sample that carries
/// nothing but a POSITION ([`LiveView::push_pointer`]): the button and
/// modifier edges are the ones the cart cannot reconstruct, so they keep
/// their places even under a stall.
const POINTER_QUEUE_MAX: usize = 16;

/// How many commands may wait for the wire. They only queue up while a
/// blob transfer holds the cart's receive queue, and the oldest is the
/// one to lose: a request from a quarter second ago has been superseded
/// by whatever the user asked for since.
const COMMAND_QUEUE_MAX: usize = 32;

/// How many `SetTransform`s one document step may be replayed as before
/// the whole world is re-sent instead. The cart commits one of them into
/// its single command slot per frame, so a big group's replay would
/// trickle out over that many cart frames -- past this many, the blob
/// (four datagrams and a round trip, whatever the document's size) is the
/// cheaper way to say it.
const TRANSFORM_REPLAY_MAX: usize = 32;

/// How many extra ticks an EDGE is repeated on. A pointer datagram is
/// fire-and-forget and the cart's APP receive queue is four deep, so the
/// one sample whose loss the cart cannot recover from -- it reads press
/// and release off consecutive samples -- goes out three times. Repeats
/// are idempotent: the cart reads pointer STATE, so the same sample twice
/// is the same buttons twice and no second edge.
const POINTER_EDGE_REPEATS: u8 = 2;

/// Whether `next` differs from `previous` in nothing but the cursor
/// position -- the cart sees no edge between the two.
fn steady(previous: &PointerState, next: &PointerState) -> bool {
    previous.buttons == next.buttons
        && previous.modifiers == next.modifiers
        && previous.snap == next.snap
}

/// The world blob for the open document: `world_to_toml` -> `encode_toml_at`.
pub fn encode_world(store: &WorldDocStore, assets_root: &Path) -> anyhow::Result<Vec<u8>> {
    let toml = world_to_toml(&store.to_doc())?;
    emerald_world::encode_toml_at(&toml, assets_root)
}

/// The background tile region is `BG_TILE_BASE..BG_TILE_BASE + BG_TILE_REGION`
/// of tile VRAM -- emerald-editor's `logic::layers` owns these numbers.
pub const BG_TILE_BASE: u16 = 511;
pub const BG_TILE_REGION: u16 = 496;

/// One linked layer's slice of the raw background tile region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bank {
    pub base: u16,
    pub budget: u16,
}

/// The runtime's raw-layer bank split (emerald-editor's
/// `logic::layers::banks`): the region is divided evenly over the linked
/// layers, packed in ascending slot order.
pub fn banks(linked: &[bool; 4]) -> [Option<Bank>; 4] {
    let mut out = [None; 4];
    let linked_count = linked.iter().filter(|&&is_linked| is_linked).count() as u16;
    if linked_count == 0 {
        return out;
    }
    let budget = BG_TILE_REGION / linked_count;
    let mut taken = 0u16;
    for (bank, &is_linked) in out.iter_mut().zip(linked.iter()) {
        if is_linked {
            *bank = Some(Bank {
                base: BG_TILE_BASE + taken * budget,
                budget,
            });
            taken += 1;
        }
    }
    out
}

/// The bare `map_w u16, map_h u16, cells` bytes the cart's
/// `CMD_LOAD_LAYER` wants, little-endian throughout.
pub fn layer_bytes(w: u16, h: u16, cells: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + cells.len() * 2);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    for cell in cells {
        out.extend_from_slice(&cell.to_le_bytes());
    }
    out
}

/// The cart's "no tile here" cell (emerald-editor-runtime's
/// `BLANK_TILE`), and the tileset stem an unlinked slot is cleared with.
/// Named here rather than pulled from `emerald-editor-runtime`, which
/// this crate does not depend on -- `emerald-editor-link` is the whole
/// host-side surface it needs.
pub const BLANK_TILE: u16 = 1023;
pub const BLANK_STEM: &str = "_blank";

/// One background slot's `load_layer` arguments, resolved off the
/// document: a linked slot's map bytes against its bank, or the 1x1 blank
/// map that clears an unlinked one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerLoad {
    pub layer: u32,
    pub base: u16,
    pub budget: u16,
    pub map_bytes: Vec<u8>,
    pub tileset_stem: String,
}

/// Which background slots the cart's copy is stale for.
///
/// Per slot rather than one flag for the set: a continuous stroke
/// re-dirties its own slot on every tick, and a single flag rebuilt the
/// whole four-slot queue each time -- so the queue never got past its
/// front and only slot 0 ever reached the cart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerDirty([bool; 4]);

impl LayerDirty {
    pub fn mark(&mut self, slot: u8) {
        if let Some(dirty) = self.0.get_mut(usize::from(slot)) {
            *dirty = true;
        }
    }

    /// Every slot: a greeting, a connect, or any change to the merged
    /// background set, none of which say WHICH slots moved.
    pub fn mark_all(&mut self) {
        self.0 = [true; 4];
    }

    pub fn any(&self) -> bool {
        self.0.iter().any(|dirty| *dirty)
    }

    /// The dirty slots in ascending order, clearing them.
    pub fn take(&mut self) -> Vec<u32> {
        let taken = std::mem::take(&mut self.0);
        taken
            .iter()
            .enumerate()
            .filter(|(_, dirty)| **dirty)
            .filter_map(|(slot, _)| u32::try_from(slot).ok())
            .collect()
    }
}

/// Every slot's load for the merged background set, slot 0 first.
///
/// All four slots are always covered: a slot the document does not link
/// (or whose map failed to open, or which names no tileset) is CLEARED
/// with a 1x1 blank map rather than left alone, because the cart keeps
/// whatever the previous world put there otherwise.
pub fn layer_loads(root: &Path, merged: &[MergedBackground]) -> VecDeque<LayerLoad> {
    let payloads: Vec<loader::LayerPayload> = loader::layer_payloads(root, merged)
        .into_iter()
        .filter(|payload| !payload.tileset_stem.is_empty() && usize::from(payload.slot) < 4)
        .collect();
    let mut linked = [false; 4];
    for payload in &payloads {
        linked[usize::from(payload.slot)] = true;
    }
    let banks = banks(&linked);
    (0..4u8)
        .map(|slot| {
            let bank = banks.get(usize::from(slot)).copied().flatten();
            match (payloads.iter().find(|p| p.slot == slot), bank) {
                (Some(payload), Some(bank)) => LayerLoad {
                    layer: u32::from(slot),
                    base: bank.base,
                    budget: bank.budget,
                    map_bytes: layer_bytes(payload.w, payload.h, &payload.cells),
                    tileset_stem: payload.tileset_stem.clone(),
                },
                _ => LayerLoad {
                    layer: u32::from(slot),
                    base: BG_TILE_BASE,
                    budget: 1,
                    map_bytes: layer_bytes(1, 1, &[BLANK_TILE]),
                    tileset_stem: BLANK_STEM.to_string(),
                },
            }
        })
        .collect()
}

// ------------------------------------------------------------- session

/// Which renderer the canvas is showing. Sticky for the session: opening
/// another world keeps the mode the user last chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasMode {
    Design,
    Live,
}

/// Where the live session is between "the viewer cart is being built" and
/// "the cart is mirroring the document". `Failed` is terminal: the panel
/// has already fallen back to [`CanvasMode::Design`] and only keeps the
/// session around so the toolbar can say why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Building,
    Connecting,
    Connected,
    Failed(String),
}

/// The tool radio's rows: each tool the cart named and whether it is the
/// active one. A tool is selected by its POSITION in the cart's list, so
/// names past what a `u8` can number are dropped rather than shown
/// unusable -- the cart can name more tools than `SetTool` can reach.
/// Borrowed from the mailbox's own list: this runs once per render of a
/// connected session, which is once per cart frame.
pub fn tool_rows(names: &[String], tool: u8) -> Vec<(&str, bool)> {
    names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let number = u8::try_from(index).ok()?;
            Some((name.as_str(), number == tool))
        })
        .collect()
}

/// What the live status line says, and whether it should offer a retry.
/// `frame` is the cart's own frame counter, which is the only visible
/// proof that a connected cart is still running.
pub fn status_line(status: &LiveStatus, frame: u32) -> (String, bool) {
    match status {
        LiveStatus::Building => ("Building viewer cart…".to_string(), false),
        LiveStatus::Connecting => ("Connecting…".to_string(), false),
        LiveStatus::Connected => (format!("Live · frame {frame}"), false),
        LiveStatus::Failed(reason) => (reason.clone(), true),
    }
}

/// One cart frame of emulator time. The mailbox's timeouts are measured
/// on the CART's clock, so the poll clock is derived from the endpoint's
/// frame counter rather than read off the wall -- see the plan's
/// "Contracts learned in Phase 1".
pub const FRAME_TIME: Duration = Duration::from_micros(16_667);
/// How often the poll loop wakes when no frame arrives -- ticks stop
/// while the emulator is paused or between runs.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long a `Hello` may go unanswered before it is re-sent (cart clock).
pub const HELLO_RETRY: Duration = Duration::from_millis(500);
/// How long the cart may run without answering a `Hello` before the panel
/// gives up on it (cart clock).
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(5);
/// How long the viewer cart may stay in `Building` before the panel gives
/// up. The build produces no frames, so this one IS wall time.
pub const BUILD_DEADLINE: Duration = Duration::from_secs(120);
/// How long a connected cart may go without framing before the host
/// re-greets it, and again before the session is failed (cart clock).
pub const STALE_AFTER: Duration = Duration::from_secs(2);
/// How long a world the encoder refused waits before the SAME document is
/// tried again (cart clock). A changed document retries at once; this is
/// only the backstop for a failure that came from outside the document (an
/// instanced world file being rewritten under the panel).
pub const ENCODE_RETRY: Duration = Duration::from_secs(1);

fn cart_clock(endpoint: &ggo_common::LinkEndpoint, epoch: Instant) -> Instant {
    epoch + FRAME_TIME * endpoint.frame_number().unwrap_or(0)
}

/// Where the cart clock's base has to move to so the clock does not run
/// BACKWARDS across a rebuild: the replacement cart starts numbering its
/// frames at zero again, and every deadline on this clock is a
/// `duration_since` -- which saturates to zero rather than going negative,
/// so a clock that jumped back would simply freeze the staleness and
/// connect deadlines for as many frames as the old run had drawn.
fn rebase_epoch(epoch: Instant, last_frame: u32, frame: u32) -> Instant {
    match last_frame.checked_sub(frame) {
        Some(0) | None => epoch,
        Some(dropped) => epoch + FRAME_TIME * dropped,
    }
}

/// How far the world document the panel last sent has got, and therefore
/// what the cart's published rows describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorldSync {
    /// The cart has framed since the blob landed: its rows describe the
    /// document the panel is showing.
    Loaded,
    /// A blob is queued or in flight. The rows are still the PREVIOUS
    /// world's.
    Sending,
    /// The blob was acked while the cart was on this frame. `!busy()` only
    /// says the bytes landed -- the cart rebuilds its world and
    /// republishes over the frames that follow, so the first frame AFTER
    /// this one is what proves the rows are the new world's.
    ///
    /// The cart's own [`LinkMailbox::frame_seq`] counter is the clock
    /// here, not `last_progress`: that one is stamped with whatever `now`
    /// the host passes `poll`, which is the cart clock -- frozen while the
    /// emulator is paused or lock-stepped, so it cannot tell "a datagram
    /// arrived" from "no time passed".
    Acked(u32),
}

/// One live session: the link to the viewer cart running the open world,
/// and everything the panel mirrors off it.
pub struct LiveView {
    pub endpoint: Arc<ggo_common::LinkEndpoint>,
    pub mailbox: LinkMailbox<EndpointIo>,
    pub status: LiveStatus,
    /// Base of the cart clock: `epoch + FRAME_TIME * frame_number` is the
    /// `now` every [`LinkMailbox`] call is measured against.
    pub epoch: Instant,
    /// The endpoint frame number [`Self::advance_cart_clock`] last read.
    /// A cart that was rebuilt under the session numbers from zero again,
    /// which is only visible as this counter dropping.
    pub last_frame: u32,
    /// When the session began, on the executor's clock -- the only
    /// deadline that is not the cart's is the build's.
    pub started: Instant,
    /// Cart clock at the last `Hello`.
    pub last_hello: Instant,
    /// Cart clock when the cart was first seen `Running`; the
    /// [`CONNECT_DEADLINE`] runs from here, not from the build.
    pub connect_since: Instant,
    /// Cart clock at the re-`Hello` a stale session triggered, if one is
    /// outstanding. A second [`STALE_AFTER`] with no answer fails it.
    pub stale_hello: Option<Instant>,
    /// The cart's latest presented frame, re-cloned out of the endpoint
    /// every tick.
    ///
    /// The emu panel calls `Window::drop_image` on each frame it RETIRES,
    /// and an `Arc` clone does not keep the atlas tile alive. So paint
    /// only the frame this field holds right now, and never carry an
    /// `Arc<RenderImage>` from one tick into the next for painting -- by
    /// the time it is drawn the tile behind it may already have been
    /// handed back.
    pub frame: Option<(u32, Arc<RenderImage>)>,
    /// The cart's published rects, in world pixels.
    pub rows: Vec<CartRow>,
    pub index_map: IndexMap,
    pub world_sync: WorldSync,
    /// The rows the document mirror has already folded in, as the last
    /// tick left them. An instance's members have no document position of
    /// their own -- the document only knows where the `[[instance]]` sits
    /// -- so a member row is mirrored as the DELTA against this baseline,
    /// and there is nothing to measure against until it exists.
    ///
    /// Cleared whenever the mirror is not folding -- a world blob in
    /// flight or still owed, or the cart in Play. The rows on the far side
    /// of that gap describe a different document (or a play-through), and
    /// differencing across it would move an instance by the gap between
    /// two unrelated worlds.
    pub mirror_rows: Vec<CartRow>,
    /// The cart's selection as it last published it, in its own flattened
    /// indices -- what the mirror compares against to tell "the cart
    /// selected something else" from "the cart re-sent the same set".
    pub cart_selection: Vec<u32>,
    /// The rubber band the cart is dragging out, in world px as
    /// `[x0, y0, x1, y1]`; `None` when there is none to draw.
    pub marquee: Option<[f64; 4]>,
    /// The cart gestures the mirror is folding document ops into,
    /// outermost first: every op tagged with one amends ONE undo entry, so
    /// a whole cart-side drag undoes in a single step.
    ///
    /// A stack rather than a slot because the cart NESTS -- a Nudge in the
    /// middle of a drag reports `Begin a, Begin b, End b, End a` -- and
    /// closing `b` has to hand the rest of the drag back to `a`. A slot
    /// left those frames untagged, which is one undo entry per frame.
    ///
    /// Kept here rather than read off `LinkMailbox::innermost_open`, which
    /// is the same stack: the mailbox pops on the `End` datagram, and
    /// `poll` has already run by the time the mirror folds, so its top no
    /// longer names the gesture this tick's rows were published under. The
    /// mirror replays the edges instead, applying the `End`s only after
    /// the fold -- which is the wire's own frame rule.
    pub gesture: Vec<u32>,
    /// The synthetic gesture a burst of UNPROMPTED motion folds under. A
    /// user edit system animating an entity reports moved rows with no
    /// gesture around them, and one undo entry per cart frame is not an
    /// undo history. Opened by the first such tick and retired by the
    /// first tick whose rows all sat still, so one burst is one entry.
    pub auto_gesture: Option<u32>,
    /// How many synthetic bursts this session has opened, so a new burst
    /// never reuses a retired one's id (which would amend its entry).
    auto_gestures: u32,
    /// Which of the cart's system tables is running. Play is the GAME's:
    /// its entities move because the game moved them, and folding that
    /// into the document would rewrite the world from a play-through.
    pub mode: EditorMode,
    pub world_dirty: bool,
    /// The document generation whose encode last failed, and the cart-clock
    /// instant that generation may be tried again at. Encoding walks every
    /// instanced world file on the UI thread, and the poll wakes on every
    /// cart frame, so a document the encoder keeps refusing must not be
    /// re-encoded per tick. Cleared by a successful send and by a greeting.
    pub world_retry_at: Option<(u64, Instant)>,
    pub layers_dirty: LayerDirty,
    /// User-chosen integer picture scale; `None` fits the canvas.
    pub scale: Option<u32>,
    /// The cart's camera in world px, from its per-frame report.
    pub camera: Option<[f64; 2]>,
    /// A camera the host owes the cart (a pan, a look-around, the
    /// document's own framing at the greeting), flushed once per tick.
    pub pending_camera: Option<[f64; 2]>,
    /// Pointer samples owed the cart, oldest first: one goes out per tick
    /// ([`Self::push_pointer`] says why the queue is not a single slot).
    pub pending_pointer: VecDeque<PointerState>,
    /// Whether the sample at the back of that queue is a plain move --
    /// the only kind another move is allowed to fold onto.
    pointer_moving: bool,
    /// The last sample actually put on the wire, which is what the next
    /// one is a move OF once the queue has drained.
    last_pointer: Option<PointerState>,
    /// Ticks still owed a repeat of [`Self::last_pointer`], armed by
    /// every edge that goes out ([`POINTER_EDGE_REPEATS`]).
    pointer_repeats: u8,
    /// Which mouse buttons the host believes are held, as the cart's own
    /// bit layout. gpui reports one button per event, so the mask has to
    /// be carried between them.
    pub pointer_buttons: u8,
    /// Edit commands owed the cart, in the order the user asked for them.
    /// Flushed whole every tick: a command is a discrete request, and
    /// dropping one would silently swallow a keypress.
    pub pending_commands: Vec<EditCommand>,
    /// `SetTransform`s owed the cart -- `(cart index, raw x, raw y)` --
    /// from an undo or redo of a move. One goes out per tick: the cart
    /// commits a transform into its ONE command slot and reads no further
    /// datagram until the frame after, so a burst sent in a single tick
    /// would sit in a four-deep receive queue and be dropped past the
    /// fourth.
    pub pending_transforms: VecDeque<(u32, i32, i32)>,
    /// Transforms already on the wire, in the raw units they were sent in.
    /// A row that comes back at exactly one of these is the HOST's own
    /// undo landing, not cart-side motion, and folding it would apply the
    /// undone delta a second time -- an instance member has no document
    /// position of its own, so the mirror can only read it as a delta.
    pub replayed_rows: Vec<(u32, i32, i32)>,
    /// Slots still to push, ascending, one per tick: the cart's APP
    /// receive queue is four datagrams deep, and a blob transfer already
    /// fills it. A slot re-dirtied while this queue is draining is re-read
    /// and moved to the BACK rather than restarting the cycle.
    pub layer_queue: VecDeque<LayerLoad>,
    /// Which tool the cart is running the pointer through: 0 is its
    /// built-in select, `n` the `n - 1`th of the edit systems it named in
    /// its greeting ([`LinkMailbox::tool_names`]). The cart resets to 0 on
    /// every greeting, so a re-greeted session has to be told again.
    pub tool: u8,
    pub poll: Option<Task<()>>,
}

impl LiveView {
    pub fn new(endpoint: Arc<ggo_common::LinkEndpoint>, now: Instant) -> Self {
        // The cart clock starts wherever the endpoint's frame counter
        // already is (a reused cart has been running a while), so every
        // baseline below has to be taken on THAT clock, not on `now`.
        let cart = cart_clock(&endpoint, now);
        let endpoint_frame = endpoint.frame_number().unwrap_or(0);
        LiveView {
            mailbox: LinkMailbox::new(EndpointIo(endpoint.clone())),
            endpoint,
            status: LiveStatus::Building,
            epoch: now,
            last_frame: endpoint_frame,
            started: now,
            last_hello: cart,
            connect_since: cart,
            stale_hello: None,
            frame: None,
            rows: Vec::new(),
            index_map: IndexMap::new(0, &[]),
            world_sync: WorldSync::Loaded,
            mirror_rows: Vec::new(),
            cart_selection: Vec::new(),
            marquee: None,
            gesture: Vec::new(),
            auto_gesture: None,
            auto_gestures: 0,
            mode: EditorMode::default(),
            world_dirty: false,
            world_retry_at: None,
            layers_dirty: LayerDirty::default(),
            scale: None,
            camera: None,
            pending_camera: None,
            pending_pointer: VecDeque::new(),
            pointer_moving: false,
            last_pointer: None,
            pointer_repeats: 0,
            pointer_buttons: 0,
            pending_commands: Vec::new(),
            pending_transforms: VecDeque::new(),
            replayed_rows: Vec::new(),
            layer_queue: VecDeque::new(),
            // The cart's built-in select tool, which is what a session
            // that has not been told otherwise is running.
            tool: 0,
            poll: None,
        }
    }

    /// The band the overlay paints, as an origin and a size in world px:
    /// the cart reports two CORNERS, and either of them may be the one
    /// being dragged.
    pub fn marquee_rect(&self) -> Option<[f64; 4]> {
        let [x0, y0, x1, y1] = self.marquee?;
        Some([x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()])
    }

    /// The camera the picture and the outlines are placed with: the
    /// cart's own report, else the origin. Never a camera still owed --
    /// the cart owns the camera now (it pans on the forwarded pointer),
    /// so the outlines follow the frame rather than running ahead of it.
    pub fn overlay_camera(&self) -> [f64; 2] {
        self.camera.unwrap_or([0.0, 0.0])
    }

    /// Queue one pointer sample for the cart.
    ///
    /// A move folds onto a move: the cart reads the newest pointer of a
    /// frame, and a position that was superseded before it ever went out
    /// is worth nothing. Nothing else folds:
    ///
    /// * A sample whose buttons or modifiers CHANGED is queued behind --
    ///   the cart derives `just_pressed`/`just_released` from consecutive
    ///   samples, so a press and the release that ends it inside one tick
    ///   have to reach it as two frames' input. Folding them would lose
    ///   the click, and reordering them would leave a press it never sees
    ///   released.
    /// * A move never folds onto a press or a release either, even though
    ///   the buttons match: the press point is what the cart hit-tests
    ///   and the release point is where a marquee settles, and both would
    ///   drift to wherever the cursor got to inside the same tick.
    pub fn push_pointer(&mut self, sample: PointerState) {
        let previous = self.pending_pointer.back().copied().or(self.last_pointer);
        let moving = previous.is_some_and(|previous| steady(&previous, &sample));
        if moving
            && self.pointer_moving
            && let Some(back) = self.pending_pointer.back_mut()
        {
            *back = sample;
            return;
        }
        self.pointer_moving = moving;
        self.pending_pointer.push_back(sample);
        while self.pending_pointer.len() > POINTER_QUEUE_MAX {
            // The oldest position, not the oldest sample: an edge dropped
            // here is a press the cart never sees, or a release it never
            // sees, and it cannot recover either from the samples around
            // it. With every sample an edge there is nothing to collapse
            // and the front goes after all.
            match self.oldest_move() {
                Some(index) => self.pending_pointer.remove(index),
                None => self.pending_pointer.pop_front(),
            };
            // Whatever is at the back may no longer be a move of what now
            // precedes it; the next sample queues rather than folding.
            self.pointer_moving = false;
        }
    }

    /// The oldest queued sample that carries nothing but a position: the
    /// first one the cart would see no edge at.
    fn oldest_move(&self) -> Option<usize> {
        let mut previous = self.last_pointer;
        for (index, sample) in self.pending_pointer.iter().enumerate() {
            if previous.is_some_and(|previous| steady(&previous, sample)) {
                return Some(index);
            }
            previous = Some(*sample);
        }
        None
    }

    /// The one pointer sample this tick owes the cart: the next queued
    /// one, else a repeat of the last edge that went out (see
    /// [`POINTER_EDGE_REPEATS`]), else nothing.
    pub fn take_pointer(&mut self) -> Option<PointerState> {
        let Some(sample) = self.pending_pointer.pop_front() else {
            if self.pointer_repeats > 0 {
                self.pointer_repeats -= 1;
                return self.last_pointer;
            }
            return None;
        };
        let edge = self.last_pointer.is_none_or(|last| !steady(&last, &sample));
        self.pointer_repeats = if edge { POINTER_EDGE_REPEATS } else { 0 };
        self.last_pointer = Some(sample);
        Some(sample)
    }

    /// Queue one edit command, dropping the oldest once
    /// [`COMMAND_QUEUE_MAX`] are already waiting.
    pub fn push_command(&mut self, command: EditCommand) {
        self.pending_commands.push(command);
        while self.pending_commands.len() > COMMAND_QUEUE_MAX {
            self.pending_commands.remove(0);
        }
    }

    /// The `SetTransform`s that replay one document step onto the cart --
    /// one per affected cart index, in raw units -- or `None` when the
    /// cart needs the whole world instead: a step that changed more than
    /// positions, a session whose rows do not describe this document, Play
    /// (where the entities are the game's), or a step too big to trickle
    /// out one command per cart frame.
    pub fn transform_replay(
        &self,
        before: &WorldState,
        after: &WorldState,
    ) -> Option<Vec<(u32, i32, i32)>> {
        // The same window the mirror folds in: the cart's indices only
        // describe this document while the world handshake is settled,
        // and in Play the game owns where its entities are.
        if self.mode != EditorMode::Edit || !self.loaded() || self.world_dirty {
            return None;
        }
        let mut transforms = Vec::new();
        for (target, pos, delta) in moves_between(before, after)? {
            for index in self.index_map.indices_of(target) {
                let at = match target {
                    // A direct entity's row IS its `Transform.pos`.
                    Selection::Entity(_) => pos,
                    // An instance member has no position of its own in the
                    // document -- only the `[[instance]]` does -- so each
                    // member moves from where the cart has it by the same
                    // delta. A member the cart has not published cannot be
                    // placed at all, and the world has to go instead.
                    Selection::Instance(_) => {
                        let row = self.rows.iter().find(|row| row.index == index)?;
                        [row.x + delta[0], row.y + delta[1]]
                    }
                };
                transforms.push((index, to_raw(at[0]), to_raw(at[1])));
            }
        }
        (!transforms.is_empty() && transforms.len() <= TRANSFORM_REPLAY_MAX).then_some(transforms)
    }

    /// Queue transforms for the cart, superseding anything still owed for
    /// the same index: the last place the document put a row is the only
    /// one worth sending.
    pub fn queue_transforms(&mut self, transforms: Vec<(u32, i32, i32)>) {
        for (index, x, y) in transforms {
            self.pending_transforms
                .retain(|(queued, _, _)| *queued != index);
            self.replayed_rows.retain(|(sent, _, _)| *sent != index);
            self.pending_transforms.push_back((index, x, y));
        }
    }

    /// Note that `transform` went out, so the row it produces is not
    /// mirrored back into the document ([`Self::replayed_rows`]).
    pub fn note_transform_sent(&mut self, transform: (u32, i32, i32)) {
        self.replayed_rows.push(transform);
        // A transform the cart never applies (a row it has since dropped)
        // would otherwise hold its entry forever, suppressing one honest
        // fold at that exact position.
        while self.replayed_rows.len() > TRANSFORM_REPLAY_MAX {
            self.replayed_rows.remove(0);
        }
    }

    /// Forget every transform this session owed or sent -- the cart is
    /// being handed a whole world, which places the rows itself.
    pub fn forget_transforms(&mut self) {
        self.pending_transforms.clear();
        self.replayed_rows.clear();
    }

    /// Tell the cart every button the host thinks is held has come up, at
    /// the last point it was told about. `false` when there was nothing
    /// held.
    ///
    /// The cursor leaving the canvas is one of the ways a release never
    /// reaches this element, and a gesture left open would resume from
    /// wherever the pointer came back.
    pub fn release_buttons(&mut self, snap: bool) -> bool {
        if self.pointer_buttons == 0 {
            return false;
        }
        self.pointer_buttons = 0;
        let Some(last) = self.pending_pointer.back().copied().or(self.last_pointer) else {
            return false;
        };
        self.push_pointer(PointerState {
            device: last.device,
            buttons: 0,
            modifiers: last.modifiers,
            snap,
        });
        true
    }

    /// Drop every input the host owes a cart that is being replaced: the
    /// new cart has never seen the buttons this mask says are held, and a
    /// press queued for the outgoing one would arrive as a press it never
    /// gets a release for.
    pub fn forget_input(&mut self) {
        self.pending_pointer.clear();
        self.pending_commands.clear();
        self.forget_transforms();
        self.pointer_moving = false;
        self.last_pointer = None;
        self.pointer_repeats = 0;
        self.pointer_buttons = 0;
    }

    /// The cart clock: emulator-derived, monotonic, and frozen while the
    /// emulator is paused (`frame_number` stops advancing).
    ///
    /// Read only by the tests. Production reads the clock through
    /// [`Self::advance_cart_clock`], which is the same value plus the
    /// re-base a rebuilt cart needs -- and taking it without that re-base
    /// is exactly the bug it exists to prevent.
    #[cfg(test)]
    pub fn cart_now(&self) -> Instant {
        cart_clock(&self.endpoint, self.epoch)
    }

    /// [`Self::cart_now`] for the one caller that gets to move the clock's
    /// base: the poll step. A viewer cart rebuilt under a live session
    /// restarts its frame numbering, so the base is nudged forward by
    /// whatever the counter dropped and the clock carries on from where
    /// the outgoing run left it.
    pub fn advance_cart_clock(&mut self) -> Instant {
        let frame = self.endpoint.frame_number().unwrap_or(0);
        self.epoch = rebase_epoch(self.epoch, self.last_frame, frame);
        self.last_frame = frame;
        self.epoch + FRAME_TIME * frame
    }

    /// Whether the cart's rows describe the document the panel is showing
    /// -- see [`WorldSync`].
    pub fn loaded(&self) -> bool {
        self.world_sync == WorldSync::Loaded
    }

    /// Open a cart gesture, innermost. Re-opening one already on the stack
    /// does not stack it twice: the cart's ids are unique per session, so
    /// a repeat is a duplicated datagram, not a second gesture.
    pub fn begin_gesture(&mut self, id: u32) {
        if !self.gesture.contains(&id) {
            self.gesture.push(id);
        }
    }

    /// Close `id` wherever it sits on the stack. An id that was never
    /// opened closes nothing -- a lost `Begin` must not retire the drag
    /// that is still running underneath it.
    pub fn end_gesture(&mut self, id: u32) {
        self.gesture.retain(|open| *open != id);
    }

    /// Open or retire the synthetic burst gesture for a tick that moved
    /// `moved` document items with no cart gesture around them.
    pub fn track_auto_gesture(&mut self, moved: bool) {
        if !self.gesture.is_empty() || !moved {
            // A cart gesture takes over the tagging, so the burst ends
            // here rather than resuming after the drag and folding the
            // motion on both sides of it into one entry.
            self.auto_gesture = None;
        } else if self.auto_gesture.is_none() {
            self.auto_gestures = self.auto_gestures.wrapping_add(1);
            self.auto_gesture = Some(self.auto_gestures);
        }
    }

    /// What this tick's mirrored ops are tagged with: the innermost cart
    /// gesture, else the open burst, else nothing.
    pub fn gesture_tag(&self) -> Option<String> {
        self.gesture
            .last()
            .map(|id| format!("cart-{id}"))
            .or_else(|| self.auto_gesture.map(|id| format!("cart-auto-{id}")))
    }

    /// Advance the world-blob handshake on what the last poll learned: the
    /// transfer finishing arms the wait, and the first cart frame after
    /// that is the republish the overlay was waiting for.
    pub fn advance_world_sync(&mut self) {
        let frame = self.mailbox.frame_seq();
        // `!busy()` is ambiguous on its own: it is equally true before a
        // blob has been queued and after it was acked. `world_dirty` is
        // what separates them -- a blob the panel still OWES the cart (a
        // greeting just re-armed it, or the encode keeps failing) has not
        // been acked by anyone.
        if self.world_sync == WorldSync::Sending && !self.world_dirty && !self.mailbox.busy() {
            self.world_sync = WorldSync::Acked(frame);
        }
        if let WorldSync::Acked(acked_at) = self.world_sync
            && frame > acked_at
        {
            self.world_sync = WorldSync::Loaded;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::render::Selection;

    #[test]
    fn fit_scale_is_the_largest_integer_that_fits_min_one() {
        assert_eq!(fit_scale(320.0, 240.0), 1);
        assert_eq!(fit_scale(640.0, 480.0), 2);
        assert_eq!(fit_scale(1000.0, 480.0), 2, "height limits");
        assert_eq!(fit_scale(2000.0, 2000.0), 6, "width limits");
        assert_eq!(fit_scale(4000.0, 4000.0), 8, "capped at LIVE_SCALE_MAX");
        assert_eq!(fit_scale(100.0, 100.0), 1, "too small still 1");
    }

    #[test]
    fn frame_origin_centers_the_scaled_frame() {
        assert_eq!(frame_origin(640.0, 480.0, 2), [0.0, 0.0]);
        assert_eq!(frame_origin(800.0, 600.0, 2), [80.0, 60.0]);
        assert_eq!(
            frame_origin(100.0, 100.0, 1),
            [-110.0, -70.0],
            "clipped when larger"
        );
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
    fn geometry_composes_the_fit_the_frame_rect_and_the_transform() {
        // 800x600 fits the 320x240 frame twice, centered at (80, 60).
        let (view, rect, scale) = geometry([800.0, 600.0], None, [10.0, 5.0]);
        assert_eq!(scale, 2);
        assert_eq!(rect, [80.0, 60.0, 640.0, 480.0]);
        assert_eq!(view.zoom, 2.0);
        // The camera's own world point sits at the frame's origin.
        assert_eq!(
            [view.pan_x + 10.0 * view.zoom, view.pan_y + 5.0 * view.zoom],
            [80.0, 60.0]
        );
        // The user's scale wins over the fit, and re-centers the frame.
        let (_, rect, scale) = geometry([800.0, 600.0], Some(1), [0.0, 0.0]);
        assert_eq!(scale, 1);
        assert_eq!(rect, [240.0, 180.0, 320.0, 240.0]);
    }

    /// A viewer cart rebuilt under a live session restarts its frame
    /// numbering, which would run the cart clock -- and so every deadline
    /// measured on it -- backwards. `duration_since` saturates rather than
    /// going negative, so the visible symptom is deadlines that simply
    /// never fire again.
    #[test]
    fn the_cart_clock_never_runs_backwards_across_a_rebuild() {
        let epoch = Instant::now();
        // 600 frames in, the replacement cart starts again at zero.
        let before = epoch + FRAME_TIME * 600;
        let rebased = rebase_epoch(epoch, 600, 0);
        assert_eq!(rebased, before, "the clock carries on from where it was");
        assert_eq!(
            rebase_epoch(rebased, 0, 5) + FRAME_TIME * 5,
            before + FRAME_TIME * 5,
            "and climbs again with the new cart's frames"
        );
        assert_eq!(
            rebase_epoch(epoch, 5, 9),
            epoch,
            "a counter that only moves forward leaves the base alone"
        );
        assert_eq!(rebase_epoch(epoch, 5, 5), epoch, "and so does a paused one");
    }

    #[test]
    fn device_from_canvas_backs_out_the_frame_origin_and_the_scale() {
        let frame = [20.0, 20.0, 640.0, 480.0];
        assert_eq!(device_from_canvas([100.0, 80.0], frame, 2), (40, 30));
        // Every canvas px inside one device pixel names THAT pixel: a
        // click on the right half of a doubled pixel is not the next one.
        assert_eq!(device_from_canvas([101.0, 81.0], frame, 2), (40, 30));
        // Outside the frame is negative, not clamped to it: the cart is
        // the one that decides a miss.
        assert_eq!(device_from_canvas([18.0, 16.0], frame, 2), (-1, -2));
        assert_eq!(device_from_canvas([20.0, 20.0], frame, 1), (0, 0));
    }

    #[test]
    fn device_from_canvas_saturates_out_of_range_and_zeroes_nan() {
        let frame = [0.0, 0.0, 320.0, 240.0];
        assert_eq!(
            device_from_canvas([1.0e9, -1.0e9], frame, 1),
            (i16::MAX, i16::MIN)
        );
        assert_eq!(device_from_canvas([f64::NAN, f64::NAN], frame, 1), (0, 0));
        // A zero scale would divide by zero; `geometry` never yields one,
        // and the floor is one anyway.
        assert_eq!(device_from_canvas([5.0, 5.0], frame, 0), (5, 5));
    }

    fn sample(device: (i16, i16), buttons: u8) -> PointerState {
        PointerState {
            device,
            buttons,
            modifiers: 0,
            snap: false,
        }
    }

    fn queued(live: &LiveView) -> Vec<PointerState> {
        live.pending_pointer.iter().copied().collect()
    }

    /// A move supersedes the move before it, but a button edge never
    /// supersedes anything and nothing supersedes an edge: the cart reads
    /// press and release off consecutive samples, and hit-tests the press
    /// at the point it was made.
    #[test]
    fn a_moved_pointer_folds_in_but_a_button_edge_queues_behind_it() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        live.push_pointer(sample((1, 1), 0));
        live.push_pointer(sample((2, 2), 0));
        live.push_pointer(sample((3, 3), 0));
        assert_eq!(
            queued(&live),
            vec![sample((1, 1), 0), sample((3, 3), 0)],
            "moves fold onto the move ahead of them"
        );
        assert_eq!(live.take_pointer(), Some(sample((1, 1), 0)));
        assert_eq!(live.take_pointer(), Some(sample((3, 3), 0)));

        // A press, a drag inside the same tick, and the release that ends
        // it: three samples, and the press keeps the point it was made at.
        live.push_pointer(sample((3, 3), 1));
        live.push_pointer(sample((4, 4), 1));
        live.push_pointer(sample((5, 5), 1));
        live.push_pointer(sample((5, 5), 0));
        assert_eq!(
            queued(&live),
            vec![sample((3, 3), 1), sample((5, 5), 1), sample((5, 5), 0)]
        );
    }

    /// Once the queue has drained, a move is measured against the sample
    /// that WENT OUT -- otherwise every tick would leave one more sample
    /// behind than it sent.
    #[test]
    fn a_move_after_a_drained_move_keeps_the_queue_at_one() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        live.push_pointer(sample((1, 1), 0));
        assert_eq!(live.take_pointer(), Some(sample((1, 1), 0)));
        for step in 2..10 {
            live.push_pointer(sample((step, step), 0));
            assert_eq!(live.pending_pointer.len(), 1);
        }
        assert_eq!(live.take_pointer(), Some(sample((9, 9), 0)));
        assert_eq!(live.take_pointer(), None);
    }

    /// The queue is bounded: a link that stops draining must not grow it
    /// without limit, and what a cart that comes back needs is the
    /// freshest state.
    #[test]
    fn the_pointer_queue_is_bounded_by_the_newest_samples() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        // Every sample a button edge, so none of them folds away.
        let pushes = i16::try_from(POINTER_QUEUE_MAX).unwrap_or(i16::MAX) * 2;
        for step in 0..pushes {
            live.push_pointer(sample((step, step), u8::try_from(step % 2).unwrap_or(0)));
        }
        assert_eq!(live.pending_pointer.len(), POINTER_QUEUE_MAX);
        assert_eq!(
            live.pending_pointer.back().copied(),
            Some(sample((pushes - 1, pushes - 1), 1)),
            "the newest sample is the one kept"
        );
    }

    /// What overflow throws away is the oldest POSITION, never an edge:
    /// the cart cannot reconstruct a press or a release from the samples
    /// around it, but a position it was about to leave behind costs it
    /// nothing.
    #[test]
    fn overflow_collapses_the_oldest_move_and_keeps_the_edges() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        // Alternating buttons, so every sample is an edge -- except the
        // one slipped in at position eight, which only moves.
        for step in 0..8i16 {
            live.push_pointer(sample((step, 0), u8::from(step % 2 == 0)));
        }
        live.push_pointer(sample((100, 0), u8::from(7 % 2 == 0)));
        for step in 8..15i16 {
            live.push_pointer(sample((step, 0), u8::from(step % 2 == 0)));
        }
        assert_eq!(live.pending_pointer.len(), POINTER_QUEUE_MAX, "full");

        live.push_pointer(sample((15, 0), u8::from(15 % 2 == 0)));
        assert_eq!(live.pending_pointer.len(), POINTER_QUEUE_MAX);
        assert_eq!(
            queued(&live)
                .iter()
                .map(|sample| sample.device.0)
                .collect::<Vec<_>>(),
            (0..16).collect::<Vec<i16>>(),
            "the lone position went; every edge kept its place, in order"
        );
    }

    /// Commands are capped too, and the oldest is the one to lose.
    #[test]
    fn the_command_queue_drops_the_oldest_when_it_is_full() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        for index in 0..COMMAND_QUEUE_MAX + 2 {
            live.push_command(EditCommand::Nudge {
                dx: index as i32,
                dy: 0,
            });
        }
        assert_eq!(live.pending_commands.len(), COMMAND_QUEUE_MAX);
        assert_eq!(
            live.pending_commands.first(),
            Some(&EditCommand::Nudge { dx: 2, dy: 0 })
        );
    }

    /// An edge is repeated on the ticks that follow it while nothing else
    /// is queued -- identically, so the cart sees one edge, not three.
    #[test]
    fn an_edge_is_repeated_and_a_move_is_not() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        live.push_pointer(sample((1, 1), 1));
        assert_eq!(live.take_pointer(), Some(sample((1, 1), 1)));
        assert_eq!(live.take_pointer(), Some(sample((1, 1), 1)));
        assert_eq!(live.take_pointer(), Some(sample((1, 1), 1)));
        assert_eq!(live.take_pointer(), None, "and no more than twice over");

        live.push_pointer(sample((2, 2), 1));
        assert_eq!(live.take_pointer(), Some(sample((2, 2), 1)));
        assert_eq!(
            live.take_pointer(),
            None,
            "a move carries nothing the next sample cannot replace"
        );
    }

    /// A cart being replaced takes the host's idea of the mouse with it.
    #[test]
    fn a_forgotten_session_leaves_no_button_held() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        live.pointer_buttons = 1;
        live.push_pointer(sample((4, 4), 1));
        live.pending_commands.push(EditCommand::Delete);
        live.forget_input();
        assert!(live.pending_pointer.is_empty());
        assert!(live.pending_commands.is_empty());
        assert_eq!(live.pointer_buttons, 0);
    }

    #[test]
    fn scale_step_clamps() {
        assert_eq!(scale_step(1, -1), 1);
        assert_eq!(scale_step(1, 1), 2);
        assert_eq!(scale_step(8, 1), 8);
        assert_eq!(scale_step(5, -1), 4);
    }

    #[test]
    fn index_map_puts_direct_entities_first_then_instances_depth_first() {
        let m = IndexMap::new(2, &[3, 1]);
        assert_eq!(m.len(), 6);
        assert!(!m.is_empty());
        assert!(
            IndexMap::new(0, &[]).is_empty(),
            "a world with nothing in it flattens to no cart indices"
        );
        assert_eq!(m.selection_of(0), Some(Selection::Entity(0)));
        assert_eq!(m.selection_of(1), Some(Selection::Entity(1)));
        assert_eq!(m.selection_of(2), Some(Selection::Instance(0)));
        assert_eq!(m.selection_of(4), Some(Selection::Instance(0)));
        assert_eq!(m.selection_of(5), Some(Selection::Instance(1)));
        assert_eq!(m.selection_of(6), None);
    }

    /// An instance whose world is empty (or failed to read -- both count
    /// 0) must not shift the instances after it, and must own no index.
    #[test]
    fn index_map_handles_an_instance_that_contributes_nothing() {
        let m = IndexMap::new(2, &[0, 3]);
        assert_eq!(m.len(), 5);
        assert_eq!(m.selection_of(2), Some(Selection::Instance(1)));
        assert_eq!(m.selection_of(4), Some(Selection::Instance(1)));
    }

    /// The group table the cart is handed: one contiguous run per
    /// `[[instance]]`, and nothing for the direct entities.
    #[test]
    fn instance_ranges_are_the_contiguous_run_each_instance_owns() {
        assert_eq!(
            IndexMap::new(2, &[2, 3]).instance_ranges(),
            [(2, 2), (4, 3)]
        );
        assert!(IndexMap::new(3, &[]).instance_ranges().is_empty());
        assert_eq!(
            IndexMap::new(0, &[1, 0, 2]).instance_ranges(),
            [(0, 1), (1, 2)],
            "an instance that flattens to nothing owns no range"
        );
    }

    /// A one-entity, one-instance document: entity 0 at `entity`, the
    /// `[[instance]]` at `instance`.
    fn doc_state(entity: [f64; 2], instance: [f64; 2]) -> WorldState {
        WorldState {
            entities: vec![ggo_worldlib::world_file::WorldEntity {
                components: serde_json::json!({ "Transform": { "pos": entity, "z": 0.0 } })
                    .as_object()
                    .expect("an object literal")
                    .clone(),
            }],
            instances: vec![WorldInstance {
                world: "worlds/pair".to_string(),
                pos: instance,
                background_priority: false,
                resolved: None,
                error: None,
            }],
            backgrounds: Vec::new(),
            dirty: false,
        }
    }

    /// Only positions: anything else the step touched means the cart has
    /// to be handed the whole world instead.
    #[test]
    fn moves_between_reports_positions_and_refuses_everything_else() {
        let before = doc_state([4.0, 4.0], [10.0, 10.0]);
        assert_eq!(
            moves_between(&before, &before),
            Some(Vec::new()),
            "a step that moved nothing moved nothing"
        );
        let after = doc_state([40.0, 50.0], [10.0, 10.0]);
        assert_eq!(
            moves_between(&before, &after),
            Some(vec![(Selection::Entity(0), [40.0, 50.0], [36.0, 46.0])])
        );
        let after = doc_state([4.0, 4.0], [30.0, 10.0]);
        assert_eq!(
            moves_between(&before, &after),
            Some(vec![(Selection::Instance(0), [30.0, 10.0], [20.0, 0.0])])
        );

        let mut structural = before.clone();
        structural.entities.push(before.entities[0].clone());
        assert_eq!(moves_between(&before, &structural), None, "an added entity");

        let mut field = before.clone();
        if let Some(serde_json::Value::Object(transform)) =
            field.entities[0].components.get_mut("Transform")
        {
            transform.insert("z".to_string(), serde_json::json!(3.0));
        }
        assert_eq!(
            moves_between(&before, &field),
            None,
            "a field beside the position is not something a transform carries"
        );

        let mut renamed = before.clone();
        renamed.instances[0].world = "worlds/other".to_string();
        assert_eq!(moves_between(&before, &renamed), None, "a re-pointed instance");
    }

    /// The replay: a direct entity takes the document position, while each
    /// instance MEMBER moves from where the cart has it by the instance's
    /// delta -- the document has no position for a member at all.
    #[test]
    fn transform_replay_moves_instance_members_by_the_delta() {
        let live = offline_view(
            vec![row(0, 4.0, 4.0), row(1, 10.0, 10.0), row(2, 10.0, 34.0)],
            1,
            &[2],
        );
        let before = doc_state([4.0, 4.0], [10.0, 10.0]);
        let after = doc_state([40.0, 50.0], [30.0, 10.0]);
        assert_eq!(
            live.transform_replay(&before, &after),
            Some(vec![
                (0, to_raw(40.0), to_raw(50.0)),
                (1, to_raw(30.0), to_raw(10.0)),
                (2, to_raw(30.0), to_raw(34.0)),
            ])
        );
        assert_eq!(
            live.transform_replay(&before, &before),
            None,
            "a step with nothing to replay is not a replay"
        );
    }

    /// The replay window is the mirror's: the cart's indices only describe
    /// this document while the world handshake is settled, and in Play the
    /// entities are the game's to place.
    #[test]
    fn a_session_out_of_step_with_the_document_replays_nothing() {
        let before = doc_state([4.0, 4.0], [10.0, 10.0]);
        let after = doc_state([40.0, 50.0], [10.0, 10.0]);
        let mut live = offline_view(vec![row(0, 4.0, 4.0)], 1, &[]);
        assert!(live.transform_replay(&before, &after).is_some());

        live.world_dirty = true;
        assert_eq!(live.transform_replay(&before, &after), None, "a world owed");
        live.world_dirty = false;
        live.world_sync = WorldSync::Sending;
        assert_eq!(live.transform_replay(&before, &after), None, "a world in flight");
        live.world_sync = WorldSync::Loaded;
        live.mode = EditorMode::Play;
        assert_eq!(live.transform_replay(&before, &after), None, "Play");
    }

    /// The queue supersedes per index and retires what the cart proved it
    /// applied, so a stale replay cannot suppress an honest fold forever.
    #[test]
    fn queued_transforms_supersede_and_are_capped() {
        let mut live = offline_view(Vec::new(), 1, &[]);
        live.queue_transforms(vec![(0, 1, 1), (1, 2, 2)]);
        live.queue_transforms(vec![(0, 9, 9)]);
        assert_eq!(
            live.pending_transforms.iter().copied().collect::<Vec<_>>(),
            vec![(1, 2, 2), (0, 9, 9)],
            "the second ask for index 0 replaced the first"
        );
        for index in 0..TRANSFORM_REPLAY_MAX as u32 + 1 {
            live.note_transform_sent((index, 0, 0));
        }
        assert_eq!(live.replayed_rows.len(), TRANSFORM_REPLAY_MAX);
        live.forget_transforms();
        assert!(live.pending_transforms.is_empty());
        assert!(live.replayed_rows.is_empty());
    }

    /// A session with no cart behind it, for the pure lookups: they read
    /// `rows`/`index_map`/`world_sync` and nothing else.
    fn offline_view(rows: Vec<CartRow>, direct: usize, instances: &[usize]) -> LiveView {
        let mut live = LiveView::new(ggo_common::LinkEndpoint::new(), Instant::now());
        live.rows = rows;
        live.index_map = IndexMap::new(direct, instances);
        live
    }

    fn row(index: u32, x: f64, y: f64) -> CartRow {
        CartRow {
            index,
            x,
            y,
            w: 16.0,
            h: 16.0,
            ox: 0.0,
            oy: 0.0,
        }
    }

    /// A 32x32 sprite drawn centered on its transform, the shape that made
    /// the drawn rect and the transform disagree.
    fn centered_row(index: u32, x: f64, y: f64) -> CartRow {
        CartRow {
            index,
            x,
            y,
            w: 32.0,
            h: 32.0,
            ox: -16.0,
            oy: -16.0,
        }
    }

    /// The overlay outlines the DRAWN rect, which for a centered sprite is
    /// not the transform box -- the mirror still folds the transform.
    #[test]
    fn a_centered_row_overlays_the_drawn_rect() {
        let mut live = offline_view(vec![centered_row(0, 100.0, 100.0)], 1, &[]);
        live.world_sync = WorldSync::Loaded;
        let counts = DocCounts {
            entities: 1,
            instances: 0,
        };

        let overlay = overlay_rows(&live, counts, &[]);
        assert_eq!(overlay.len(), 1);
        assert_eq!(overlay[0].1, [84.0, 84.0, 32.0, 32.0]);
        assert_eq!(live.rows[0].x, 100.0, "and the transform is untouched");
    }

    /// Two direct entities and one instance contributing two: cart
    /// indices 0..4.
    fn fixture_view() -> LiveView {
        offline_view(
            vec![
                row(0, 0.0, 0.0),
                row(1, 40.0, 8.0),
                row(2, 80.0, 0.0),
                row(3, 96.0, 0.0),
            ],
            2,
            &[2],
        )
    }

    const FIXTURE_COUNTS: DocCounts = DocCounts {
        entities: 2,
        instances: 1,
    };

    #[test]
    fn overlay_rows_wait_for_the_world_blob_and_flag_the_selection() {
        let mut live = fixture_view();
        live.world_sync = WorldSync::Sending;
        assert!(
            overlay_rows(&live, FIXTURE_COUNTS, &[Selection::Entity(0)]).is_empty(),
            "rows from the previous world are not drawn over the new one"
        );

        live.world_sync = WorldSync::Loaded;
        let rows = overlay_rows(&live, FIXTURE_COUNTS, &[Selection::Entity(1)]);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[1],
            (Selection::Entity(1), [40.0, 8.0, 16.0, 16.0], true)
        );
        assert!(!rows[0].2, "everything else draws unselected");

        // A row the document has since lost is dropped rather than drawn
        // against an index that no longer resolves.
        let rows = overlay_rows(
            &live,
            DocCounts {
                entities: 2,
                instances: 0,
            },
            &[],
        );
        assert_eq!(rows.len(), 2);
    }

    /// `!busy()` only says the bytes were acked. The cart rebuilds its
    /// world and republishes over the frames that follow, so the first
    /// frame AFTER the ack is the signal -- and it has to work when the
    /// republished table is byte-identical to the one before it.
    #[test]
    fn world_sync_needs_a_cart_frame_after_the_ack_not_a_changed_table() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        live.world_sync = WorldSync::Sending;
        live.advance_world_sync();
        assert_eq!(
            live.world_sync,
            WorldSync::Acked(0),
            "an idle mailbox is not busy, so the send is already acked here"
        );
        assert!(!live.loaded());
        // No cart frame lands, so the wait does not end however many turns
        // pass -- an unchanged row table must not be mistaken for one.
        live.advance_world_sync();
        live.advance_world_sync();
        assert_eq!(live.world_sync, WorldSync::Acked(0));
    }

    #[test]
    fn raw_conversion_round_trips_pixels() {
        assert_eq!(to_raw(1.5), 98304);
        assert_eq!(from_raw(98304), 1.5);
        assert_eq!(to_raw(-3.0), -196608);
    }

    /// The `as` cast is the clamp: a NaN coordinate becomes 0 and a
    /// wildly out-of-range one saturates instead of wrapping.
    #[test]
    fn to_raw_saturates_out_of_range_and_zeroes_nan() {
        assert_eq!(to_raw(f64::NAN), 0);
        assert_eq!(to_raw(1e30), i32::MAX);
        assert_eq!(to_raw(-1e30), i32::MIN);
    }

    #[test]
    fn banks_split_the_region_evenly_over_linked_slots() {
        let b = banks(&[true, false, true, false]);
        assert_eq!(b[0].map(|b| (b.base, b.budget)), Some((511, 248)));
        assert!(b[1].is_none());
        assert_eq!(b[2].map(|b| (b.base, b.budget)), Some((759, 248)));
        assert!(banks(&[false; 4]).iter().all(|b| b.is_none()));
    }

    #[test]
    fn layer_dirty_marks_one_slot_and_hands_the_set_back_ascending() {
        let mut dirty = LayerDirty::default();
        assert!(!dirty.any());
        assert!(dirty.take().is_empty());

        dirty.mark(2);
        dirty.mark(0);
        assert!(dirty.any());
        assert_eq!(
            dirty.take(),
            [0, 2],
            "ascending, whatever order they came in"
        );
        assert!(!dirty.any(), "taking them clears them");

        dirty.mark(9);
        assert!(!dirty.any(), "there are four slots, and nothing past them");

        dirty.mark_all();
        assert_eq!(dirty.take(), [0, 1, 2, 3]);
    }

    #[test]
    fn layer_bytes_are_w_h_then_cells_little_endian() {
        assert_eq!(
            layer_bytes(2, 1, &[5, 1023]),
            vec![2, 0, 1, 0, 5, 0, 255, 3]
        );
    }

    #[test]
    fn encode_world_produces_a_v5_blob_with_the_document_entities() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("worlds")).unwrap();
        std::fs::write(
            dir.path().join("worlds/sub.toml"),
            "[[entity]]\nTransform = { pos = [1, 1] }\n",
        )
        .unwrap();
        let store =
            ggo_worldlib::world_doc::WorldDocStore::new(ggo_worldlib::world_doc::WorldDocWire {
                entities: vec![ggo_worldlib::world_file::WorldEntity {
                    components: serde_json::from_value(
                        serde_json::json!({ "Transform": { "pos": [4.0, 4.0] } }),
                    )
                    .unwrap(),
                }],
                instances: vec![ggo_worldlib::world_doc::WorldInstance {
                    world: "worlds/sub".into(),
                    pos: [10.0, 0.0],
                    background_priority: false,
                    resolved: None,
                    error: None,
                }],
                backgrounds: vec![],
            });
        let blob = encode_world(&store, dir.path()).unwrap();
        assert!(blob.starts_with(b"EWLD"));
        assert_eq!(blob[4], emerald_world::VERSION);
    }

    #[test]
    fn the_tool_radio_marks_the_active_tool_and_nothing_else() {
        let names = vec![
            "Select".to_string(),
            "paint".to_string(),
            "poke".to_string(),
        ];
        assert_eq!(
            tool_rows(&names, 0),
            [("Select", true), ("paint", false), ("poke", false)]
        );
        assert_eq!(
            tool_rows(&names, 2),
            [("Select", false), ("paint", false), ("poke", true)]
        );
        assert!(tool_rows(&[], 0).is_empty());
    }

    /// A cart may name more tools than a `u8` tool byte can select; those
    /// rows have no number to send, so the radio must not offer them.
    #[test]
    fn the_tool_radio_drops_names_a_tool_byte_cannot_name() {
        let names: Vec<String> = (0..300).map(|index| format!("t{index}")).collect();
        let rows = tool_rows(&names, 0);
        assert_eq!(rows.len(), 256);
        assert_eq!(rows[255].0, "t255");
    }

    /// The band the overlay paints is the cart's two corners normalised to
    /// an origin and a size -- either corner may be the dragged one.
    #[test]
    fn the_marquee_rect_is_the_carts_band_normalised() {
        let mut live = offline_view(Vec::new(), 0, &[]);
        assert_eq!(live.marquee_rect(), None);
        live.marquee = Some([10.0, 20.0, 30.0, 50.0]);
        assert_eq!(live.marquee_rect(), Some([10.0, 20.0, 20.0, 30.0]));
        live.marquee = Some([30.0, 50.0, 10.0, 20.0]);
        assert_eq!(
            live.marquee_rect(),
            Some([10.0, 20.0, 20.0, 30.0]),
            "a band dragged up-left is the same rect"
        );
    }

    #[test]
    fn the_status_line_names_where_the_session_is() {
        assert_eq!(
            status_line(&LiveStatus::Building, 9),
            ("Building viewer cart…".to_string(), false)
        );
        assert_eq!(
            status_line(&LiveStatus::Connecting, 9),
            ("Connecting…".to_string(), false)
        );
        assert_eq!(
            status_line(&LiveStatus::Connected, 9),
            ("Live · frame 9".to_string(), false),
            "the frame counter is what proves the cart is still running"
        );
        assert_eq!(
            status_line(&LiveStatus::Failed("cart never answered".into()), 9),
            ("cart never answered".to_string(), true),
            "a failure reads as itself, and asks for a retry"
        );
    }
}
