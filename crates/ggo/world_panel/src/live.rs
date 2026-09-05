//! Live mode's pure half: the link transport over the emu panel's
//! endpoint, the cart-index <-> document-selection map (the encoder's
//! order: direct entities, then each instance's subtree depth-first),
//! hit-testing over the cart's published rects, and the payload builders.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use emerald_editor_link::{EntityRow, LinkIo, LinkMailbox};
use ggo_worldlib::backgrounds::MergedBackground;
use ggo_worldlib::render::Selection;
use ggo_worldlib::world_doc::WorldDocStore;
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

/// The lookups the Live gestures need: turning a click on the cart's
/// picture back into a document selection, and back again to drive the
/// entities a drag moves.
impl IndexMap {
    pub fn selection_of(&self, cart_index: u32) -> Option<Selection> {
        self.entries.get(cart_index as usize).copied()
    }

    /// Every cart index that belongs to `selection` (one for an entity, a
    /// contiguous run for an instance).
    pub fn indices_of(&self, selection: Selection) -> Vec<u32> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| **entry == selection)
            .map(|(index, _)| index as u32)
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

/// One published rect from the cart, in world pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CartRow {
    pub index: u32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
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
        })
        .collect()
}

fn contains(row: &CartRow, x: f64, y: f64) -> bool {
    x >= row.x && x < row.x + row.w && y >= row.y && y < row.y + row.h
}

/// Topmost (last) row under a world point, as the cart draws later rows
/// above earlier ones.
pub fn hit_row(rows: &[CartRow], x: f64, y: f64) -> Option<u32> {
    rows.iter()
        .rev()
        .find(|row| contains(row, x, y))
        .map(|row| row.index)
}

pub fn rows_in_rect(rows: &[CartRow], x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<u32> {
    let (left, right) = (x0.min(x1), x0.max(x1));
    let (top, bottom) = (y0.min(y1), y0.max(y1));
    rows.iter()
        .filter(|row| {
            row.x < right && row.x + row.w > left && row.y < bottom && row.y + row.h > top
        })
        .map(|row| row.index)
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
            counts.contains(selection).then(|| {
                (
                    selection,
                    [row.x, row.y, row.w, row.h],
                    selected.contains(&selection),
                )
            })
        })
        .collect()
}

/// The document selection under a world point: the topmost cart rect
/// there, mapped back through the flattened index map.
pub fn hit(live: &LiveView, counts: DocCounts, world: [f64; 2]) -> Option<Selection> {
    let index = hit_row(&live.rows, world[0], world[1])?;
    let selection = live.index_map.selection_of(index)?;
    counts.contains(selection).then_some(selection)
}

/// Every document selection a rubber-band covers, in cart order and
/// without repeats (an instance owns a contiguous run of cart indices).
pub fn hits_in_rect(
    live: &LiveView,
    counts: DocCounts,
    start: [f64; 2],
    current: [f64; 2],
) -> Vec<Selection> {
    let mut hits = Vec::new();
    for index in rows_in_rect(&live.rows, start[0], start[1], current[0], current[1]) {
        if let Some(selection) = live.index_map.selection_of(index)
            && counts.contains(selection)
            && !hits.contains(&selection)
        {
            hits.push(selection);
        }
    }
    hits
}

/// Where every cart index the selection owns sits right now, in the
/// runtime's raw fixed point -- the anchors a live drag adds its delta to.
pub fn drag_origins(live: &LiveView, selected: &[Selection]) -> Vec<(u32, i32, i32)> {
    selected
        .iter()
        .flat_map(|selection| live.index_map.indices_of(*selection))
        .filter_map(|index| {
            let row = live.rows.iter().find(|row| row.index == index)?;
            Some((index, to_raw(row.x), to_raw(row.y)))
        })
        .collect()
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

/// Which bit of the session's system mask a system's position in
/// [`LinkMailbox::system_names`] owns, or `None` past bit 63: the mask is
/// a `u64` and the cart's name list is `u8`-counted, so it can name more
/// systems than the mask can address.
pub fn system_bit(index: usize) -> Option<u64> {
    u32::try_from(index)
        .ok()
        .and_then(|shift| 1u64.checked_shl(shift))
}

/// The systems rail's rows: each of the cart's systems and whether `mask`
/// has it on. Names with no mask bit are dropped rather than shown
/// unusable. Borrowed from the mailbox's own list -- this runs once per
/// render of a connected session, which is once per cart frame.
pub fn system_rows(names: &[String], mask: u64) -> Vec<(&str, bool)> {
    names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let bit = system_bit(index)?;
            Some((name.as_str(), mask & bit != 0))
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

fn cart_clock(endpoint: &ggo_common::LinkEndpoint, epoch: Instant) -> Instant {
    epoch + FRAME_TIME * endpoint.frame_number().unwrap_or(0)
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
    /// Where each cart index a drag is moving sat when the drag began, in
    /// the runtime's raw fixed point -- `SetTransform` is absolute, so the
    /// mirror of a drag is "origin + the delta the document just took".
    /// Cleared on release.
    pub drag_origin: Vec<(u32, i32, i32)>,
    /// The absolute `SetTransform` payloads an in-flight drag still owes
    /// the cart, one per moved row. REPLACED (never appended to) by each
    /// mouse-move and flushed once per tick, which is one cart frame: the
    /// cart's APP receive queue is four datagrams deep, so a drag that put
    /// one datagram per row on the wire per move event would overrun it.
    ///
    /// Resolved to absolute positions at the move rather than kept as a
    /// delta so the last one still flushes after the release has dropped
    /// [`Self::drag_origin`].
    pub pending_transforms: Vec<(u32, i32, i32)>,
    pub world_dirty: bool,
    pub layers_dirty: bool,
    pub camera_dirty: bool,
    /// Slots still to push for the current `layers_dirty` cycle. One goes
    /// out per tick: the cart's APP receive queue is four datagrams deep,
    /// and a blob transfer already fills it.
    pub layer_queue: VecDeque<LayerLoad>,
    /// Which of the cart's own systems are enabled, one bit per entry of
    /// [`LinkMailbox::system_names`]. The mailbox re-applies this after
    /// every greeting on its own, so the panel only pushes changes.
    pub sys_mask: u64,
    pub poll: Option<Task<()>>,
}

impl LiveView {
    pub fn new(endpoint: Arc<ggo_common::LinkEndpoint>, now: Instant) -> Self {
        // The cart clock starts wherever the endpoint's frame counter
        // already is (a reused cart has been running a while), so every
        // baseline below has to be taken on THAT clock, not on `now`.
        let cart = cart_clock(&endpoint, now);
        LiveView {
            mailbox: LinkMailbox::new(EndpointIo(endpoint.clone())),
            endpoint,
            status: LiveStatus::Building,
            epoch: now,
            started: now,
            last_hello: cart,
            connect_since: cart,
            stale_hello: None,
            frame: None,
            rows: Vec::new(),
            index_map: IndexMap::new(0, &[]),
            world_sync: WorldSync::Loaded,
            drag_origin: Vec::new(),
            pending_transforms: Vec::new(),
            world_dirty: false,
            layers_dirty: false,
            camera_dirty: false,
            layer_queue: VecDeque::new(),
            // Editor systems only: a viewer that ran the cart's gameplay
            // systems would move the entities the user is dragging.
            sys_mask: 0,
            poll: None,
        }
    }

    /// The cart clock: emulator-derived, monotonic, and frozen while the
    /// emulator is paused (`frame_number` stops advancing).
    pub fn cart_now(&self) -> Instant {
        cart_clock(&self.endpoint, self.epoch)
    }

    /// Whether the cart's rows describe the document the panel is showing
    /// -- see [`WorldSync`].
    pub fn loaded(&self) -> bool {
        self.world_sync == WorldSync::Loaded
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

    /// Put the drag's outstanding moves on the wire: one datagram per
    /// moved row, once per tick, which is one cart frame. Coalescing
    /// happens at the other end -- each mouse-move REPLACES
    /// [`Self::pending_transforms`] -- so the count here is the size of the
    /// selection, and there is no per-tick cap on top of that.
    ///
    /// A selection wider than the cart's four-deep APP receive queue
    /// therefore sheds its tail every tick, and does so for certain while
    /// a blob transfer is using the same queue (this deliberately does not
    /// wait for one: a drag the user can see lagging is worse than a
    /// datagram queued behind a blob). That is left uncapped on purpose.
    /// The payloads are ABSOLUTE and idempotent, so a lost one costs a
    /// frame of staleness on that row and is corrected by the next tick's
    /// datagram; the release re-sends the whole world anyway. A cap would
    /// have to choose which rows go stale and would still not bound the
    /// queue, because the layer and world transfers share it.
    pub fn flush_pending_transforms(&mut self) {
        for (index, x, y) in std::mem::take(&mut self.pending_transforms) {
            if let Err(error) = self.mailbox.set_transform(index, x, y) {
                log::warn!("GGO: live drag update for cart entity {index}: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::render::Selection;

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
        assert_eq!(m.indices_of(Selection::Instance(0)), [2, 3, 4]);
        assert_eq!(m.indices_of(Selection::Entity(1)), [1]);
    }

    /// An instance whose world is empty (or failed to read -- both count
    /// 0) must not shift the instances after it, and must own no index.
    #[test]
    fn index_map_handles_an_instance_that_contributes_nothing() {
        let m = IndexMap::new(2, &[0, 3]);
        assert_eq!(m.len(), 5);
        assert_eq!(m.selection_of(2), Some(Selection::Instance(1)));
        assert!(m.indices_of(Selection::Instance(0)).is_empty());
        assert_eq!(m.indices_of(Selection::Instance(1)), [2, 3, 4]);
    }

    #[test]
    fn hit_row_prefers_the_last_row_under_the_point() {
        let rows = vec![
            CartRow {
                index: 0,
                x: 0.0,
                y: 0.0,
                w: 16.0,
                h: 16.0,
            },
            CartRow {
                index: 1,
                x: 8.0,
                y: 8.0,
                w: 16.0,
                h: 16.0,
            },
        ];
        assert_eq!(hit_row(&rows, 10.0, 10.0), Some(1));
        assert_eq!(hit_row(&rows, 2.0, 2.0), Some(0));
        assert_eq!(hit_row(&rows, 100.0, 100.0), None);
        assert_eq!(rows_in_rect(&rows, 0.0, 0.0, 9.0, 9.0), [0, 1]);
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
        }
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
    fn hit_maps_the_topmost_row_back_to_its_selection() {
        let live = fixture_view();
        assert_eq!(
            hit(&live, FIXTURE_COUNTS, [41.0, 9.0]),
            Some(Selection::Entity(1))
        );
        // Either of the instance's two rows names the instance itself.
        assert_eq!(
            hit(&live, FIXTURE_COUNTS, [81.0, 1.0]),
            Some(Selection::Instance(0))
        );
        assert_eq!(
            hit(&live, FIXTURE_COUNTS, [97.0, 1.0]),
            Some(Selection::Instance(0))
        );
        assert_eq!(hit(&live, FIXTURE_COUNTS, [500.0, 500.0]), None);
    }

    /// The index map is rebuilt from counts that can be one tick behind an
    /// instance edit, so a row can name something the document has since
    /// lost. That is a miss, not a selection of nothing.
    #[test]
    fn a_row_the_document_no_longer_has_is_not_a_hit() {
        let live = fixture_view();
        let shrunk = DocCounts {
            entities: 1,
            instances: 0,
        };
        assert_eq!(hit(&live, shrunk, [41.0, 9.0]), None);
        assert_eq!(hit(&live, shrunk, [81.0, 1.0]), None);
        assert_eq!(
            hit(&live, shrunk, [1.0, 1.0]),
            Some(Selection::Entity(0)),
            "the survivor still hits"
        );
    }

    #[test]
    fn hits_in_rect_names_each_selection_once_in_cart_order() {
        let live = fixture_view();
        assert_eq!(
            hits_in_rect(&live, FIXTURE_COUNTS, [0.0, 0.0], [200.0, 200.0]),
            [
                Selection::Entity(0),
                Selection::Entity(1),
                Selection::Instance(0)
            ],
            "the instance owns two rows and is named once"
        );
        assert_eq!(
            hits_in_rect(&live, FIXTURE_COUNTS, [78.0, 0.0], [90.0, 4.0]),
            [Selection::Instance(0)]
        );
        assert!(hits_in_rect(&live, FIXTURE_COUNTS, [500.0, 500.0], [600.0, 600.0]).is_empty());
    }

    #[test]
    fn drag_origins_are_every_cart_index_the_selection_owns() {
        let live = fixture_view();
        assert_eq!(
            drag_origins(&live, &[Selection::Instance(0)]),
            [(2, to_raw(80.0), 0), (3, to_raw(96.0), 0)],
            "moving an instance moves its whole subtree"
        );
        assert_eq!(
            drag_origins(&live, &[Selection::Entity(1)]),
            [(1, to_raw(40.0), to_raw(8.0))]
        );
        assert!(
            drag_origins(&live, &[Selection::Entity(9)]).is_empty(),
            "a selection the map does not cover owns no cart index"
        );
    }

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
    fn a_system_owns_the_mask_bit_at_its_position_and_nothing_past_63() {
        assert_eq!(system_bit(0), Some(1));
        assert_eq!(system_bit(1), Some(0b10));
        assert_eq!(system_bit(63), Some(1 << 63));
        assert_eq!(system_bit(64), None, "the mask is a u64");
        assert_eq!(system_bit(usize::MAX), None);
    }

    #[test]
    fn the_systems_rail_reads_each_name_off_its_own_bit() {
        let names = vec!["animate".to_string(), "ai".to_string(), "audio".to_string()];
        assert_eq!(
            system_rows(&names, 0),
            [("animate", false), ("ai", false), ("audio", false)]
        );
        assert_eq!(
            system_rows(&names, 0b101),
            [("animate", true), ("ai", false), ("audio", true)]
        );
        assert!(system_rows(&[], u64::MAX).is_empty());
    }

    /// A cart may name more systems than a `u64` has bits; those have no
    /// bit to toggle, so the rail must not offer a control that does
    /// nothing.
    #[test]
    fn the_systems_rail_drops_names_that_have_no_mask_bit() {
        let names: Vec<String> = (0..70).map(|index| format!("s{index}")).collect();
        let rows = system_rows(&names, u64::MAX);
        assert_eq!(rows.len(), 64);
        assert_eq!(rows[63].0, "s63");
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
