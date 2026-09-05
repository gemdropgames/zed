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

/// The lookups the Live GESTURES need: turning a click on the cart's
/// picture back into a document selection, and back again to drive the
/// entities a drag moves. The session (Task 4) only builds the map; Task 5
/// is what reads it.
#[allow(dead_code)]
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

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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

// Hit-testing over the cart's rows is the Live canvas's half of the
// gesture story (Task 5); the session only keeps the rows fresh.
#[allow(dead_code)]
fn contains(row: &CartRow, x: f64, y: f64) -> bool {
    x >= row.x && x < row.x + row.w && y >= row.y && y < row.y + row.h
}

/// Topmost (last) row under a world point, as the cart draws later rows
/// above earlier ones.
#[allow(dead_code)]
pub fn hit_row(rows: &[CartRow], x: f64, y: f64) -> Option<u32> {
    rows.iter()
        .rev()
        .find(|row| contains(row, x, y))
        .map(|row| row.index)
}

#[allow(dead_code)]
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
    /// The cart's latest presented frame. Cloned out of the endpoint --
    /// the emu panel owns dropping the image.
    pub frame: Option<(u32, Arc<RenderImage>)>,
    /// The cart's published rects, in world pixels.
    pub rows: Vec<CartRow>,
    pub index_map: IndexMap,
    pub world_dirty: bool,
    pub layers_dirty: bool,
    pub camera_dirty: bool,
    /// Slots still to push for the current `layers_dirty` cycle. One goes
    /// out per tick: the cart's APP receive queue is four datagrams deep,
    /// and a blob transfer already fills it.
    pub layer_queue: VecDeque<LayerLoad>,
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
            world_dirty: false,
            layers_dirty: false,
            camera_dirty: false,
            layer_queue: VecDeque::new(),
            poll: None,
        }
    }

    /// The cart clock: emulator-derived, monotonic, and frozen while the
    /// emulator is paused (`frame_number` stops advancing).
    pub fn cart_now(&self) -> Instant {
        cart_clock(&self.endpoint, self.epoch)
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
}
