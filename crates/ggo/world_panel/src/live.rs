//! Live mode's pure half: the link transport over the emu panel's
//! endpoint, the cart-index <-> document-selection map (the encoder's
//! order: direct entities, then each instance's subtree depth-first),
//! hit-testing over the cart's published rects, and the payload builders.

// This module lands ahead of the panel code that drives it (the Live-mode
// session is the next commit); its tests are the only callers so far, so the
// helpers would otherwise read as dead. Drop this once Live mode is wired.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use emerald_editor_link::LinkIo;
use ggo_worldlib::render::Selection;
use ggo_worldlib::world_doc::WorldDocStore;
use ggo_worldlib::world_file::world_to_toml;

/// [`LinkIo`] over the emu panel's endpoint: frames payloads as APP
/// datagrams on the way out, hands back the already-decoded APP payloads
/// the emulator thread collected on the way in.
pub struct EndpointIo(pub Arc<ggo_common::LinkEndpoint>);

impl LinkIo for EndpointIo {
    fn send(&mut self, payload: &[u8]) -> std::io::Result<()> {
        // Delimiters, sentinel, channel, CRC and COBS's own overhead; a
        // reservation, not a bound (`encode_payload` caps payload length).
        let mut wire = Vec::with_capacity(payload.len() + 16);
        if !ggo_wire::encode_payload(ggo_wire::channel::APP, payload, |byte| wire.push(byte)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "link payload exceeds the ggo-wire datagram limit",
            ));
        }
        self.0.send_wire(wire);
        Ok(())
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

pub fn rows_from(entities: &[emerald_editor_runtime::wire::EntityRow]) -> Vec<CartRow> {
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
