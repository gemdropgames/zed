//! The "reference sheet" an import leaves beside a `.til`: the source
//! PNG's tile layout, as a grid of POOL indices. The `.til` itself is
//! written deduplicated (one copy of every distinct tile, in first-seen
//! order), so the sheet is the only record of where those tiles sat in
//! the artwork -- the tile picker's second view shows it so a sprite can
//! be assembled by picking off the art as drawn rather than off the
//! order-scrambled pool.
//!
//! Keyed by the `.til` (not the `.spr`) at
//! `<asset root>/.ggo-ide/<til rel>.reference.json`, so a sprite created
//! later and bound to the same tileset sees the same sheet. Editor-only,
//! best-effort metadata like [`super::editor_meta`]: missing or corrupt is
//! simply "no reference sheet".

use std::collections::HashMap;
use std::path::Path;

use ggo_worldlib::sprites::tileset_doc::TILE_PIXELS;
use serde::{Deserialize, Serialize};

/// A source sheet's tile layout: `cols * rows` pool indices, row-major,
/// one per tile cell of the imported artwork.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceSheet {
    pub cols: usize,
    pub rows: usize,
    pub tiles: Vec<u16>,
}

impl ReferenceSheet {
    /// Every tile names a pool index below `tile_count`, and the grid is
    /// the shape it claims -- false once the pool has been rebuilt under
    /// the sheet (a re-import with a different crop, a manual dedup that
    /// dropped tiles), in which case the sheet is stale and not shown.
    pub fn is_valid_for(&self, tile_count: usize) -> bool {
        self.cols > 0
            && self.rows > 0
            && self.tiles.len() == self.cols * self.rows
            && self.tiles.iter().all(|&t| (t as usize) < tile_count)
    }

    /// Lay `frames` (each a row-major `w_tiles x h_tiles` tile map, as
    /// `SpriteState::frames[i].map`) out `frames_per_row` frames wide --
    /// the shape a frame-cut sprite import's source sheet had. A
    /// trailing partial frame row is padded with tile 0.
    pub fn from_frames(
        frames: &[Vec<u16>],
        w_tiles: usize,
        h_tiles: usize,
        frames_per_row: usize,
    ) -> Option<Self> {
        if frames.is_empty() || w_tiles == 0 || h_tiles == 0 || frames_per_row == 0 {
            return None;
        }
        let frames_per_row = frames_per_row.min(frames.len());
        let frame_rows = frames.len().div_ceil(frames_per_row);
        let cols = frames_per_row * w_tiles;
        let rows = frame_rows * h_tiles;
        let mut tiles = vec![0u16; cols * rows];
        for (frame_ix, map) in frames.iter().enumerate() {
            if map.len() != w_tiles * h_tiles {
                return None;
            }
            let frame_col = frame_ix % frames_per_row;
            let frame_row = frame_ix / frames_per_row;
            for ty in 0..h_tiles {
                let dst = (frame_row * h_tiles + ty) * cols + frame_col * w_tiles;
                tiles[dst..dst + w_tiles].copy_from_slice(&map[ty * w_tiles..(ty + 1) * w_tiles]);
            }
        }
        Some(Self { cols, rows, tiles })
    }
}

/// What deduplicating an as-sliced tile grid yields: the unique tiles
/// (unpacked indices, first-seen order) and the sheet that maps every
/// original cell back onto them.
pub struct Deduped {
    pub indices: Vec<u8>,
    pub tile_count: usize,
    pub sheet: ReferenceSheet,
}

/// Collapse a row-major grid of unpacked tiles (`tile_count *
/// TILE_PIXELS` index bytes, laid out `cols` wide as `slice_to_tiles`
/// cuts them) into its distinct tiles, keeping the first occurrence of
/// each and recording the grid as a [`ReferenceSheet`] over the result.
pub fn dedup_grid(indices: &[u8], tile_count: usize, cols: usize) -> Deduped {
    let cols = cols.max(1);
    let mut seen: HashMap<&[u8], u16> = HashMap::new();
    let mut unique = Vec::with_capacity(indices.len());
    let mut sheet_tiles = Vec::with_capacity(tile_count);
    for tile in indices.chunks_exact(TILE_PIXELS).take(tile_count) {
        let next = seen.len() as u16;
        let ix = *seen.entry(tile).or_insert_with(|| {
            unique.extend_from_slice(tile);
            next
        });
        sheet_tiles.push(ix);
    }
    let rows = sheet_tiles.len().div_ceil(cols);
    sheet_tiles.resize(cols * rows, 0);
    Deduped {
        tile_count: seen.len(),
        indices: unique,
        sheet: ReferenceSheet {
            cols,
            rows,
            tiles: sheet_tiles,
        },
    }
}

/// `<til rel>.reference.json` under the hidden `.ggo-ide/` dir, beside
/// the tileset's own editor sidecar.
pub fn sidecar_rel_path(til_rel: &str) -> String {
    format!(".ggo-ide/{til_rel}.reference.json")
}

/// Read the sheet recorded for `til_rel` under asset `root`, or `None`
/// when missing or unreadable.
pub fn load(root: &Path, til_rel: &str) -> Option<ReferenceSheet> {
    let bytes = std::fs::read(root.join(sidecar_rel_path(til_rel))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the sheet for `til_rel` under asset `root`, creating
/// `.ggo-ide/` subdirs as needed.
pub fn save(root: &Path, til_rel: &str, sheet: &ReferenceSheet) -> Result<(), String> {
    let path = root.join(sidecar_rel_path(til_rel));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(sheet).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(fill: u8) -> Vec<u8> {
        vec![fill; TILE_PIXELS]
    }

    #[test]
    fn dedup_grid_keeps_first_occurrences_and_maps_the_grid_back() {
        // 3x2 grid: A B A / C B A
        let grid: Vec<u8> = [1, 2, 1, 3, 2, 1].into_iter().flat_map(tile).collect();
        let deduped = dedup_grid(&grid, 6, 3);
        assert_eq!(deduped.tile_count, 3);
        assert_eq!(
            deduped.indices,
            [1, 2, 3].into_iter().flat_map(tile).collect::<Vec<_>>()
        );
        assert_eq!(
            deduped.sheet,
            ReferenceSheet {
                cols: 3,
                rows: 2,
                tiles: vec![0, 1, 0, 2, 1, 0],
            }
        );
        assert!(deduped.sheet.is_valid_for(3));
        assert!(
            !deduped.sheet.is_valid_for(2),
            "a shrunken pool invalidates"
        );
    }

    #[test]
    fn dedup_grid_pads_a_partial_last_row() {
        let grid: Vec<u8> = [5, 5, 6].into_iter().flat_map(tile).collect();
        let deduped = dedup_grid(&grid, 3, 2);
        assert_eq!(deduped.tile_count, 2);
        assert_eq!((deduped.sheet.cols, deduped.sheet.rows), (2, 2));
        assert_eq!(deduped.sheet.tiles, vec![0, 0, 1, 0]);
    }

    #[test]
    fn from_frames_lays_frame_maps_out_row_major() {
        // Two 2x1-tile frames, two per row -> a 4x1 sheet; three frames
        // wrap to a second row padded with tile 0.
        let frames = vec![vec![1, 2], vec![3, 4], vec![5, 6]];
        let sheet = ReferenceSheet::from_frames(&frames, 2, 1, 2).expect("sheet");
        assert_eq!((sheet.cols, sheet.rows), (4, 2));
        assert_eq!(sheet.tiles, vec![1, 2, 3, 4, 5, 6, 0, 0]);
        assert!(ReferenceSheet::from_frames(&[], 2, 1, 2).is_none());
        assert!(
            ReferenceSheet::from_frames(&[vec![1]], 2, 1, 1).is_none(),
            "a map of the wrong footprint is refused"
        );
    }

    #[test]
    fn from_frames_clamps_frames_per_row_to_the_frame_count() {
        let frames = vec![vec![1, 2, 3, 4]];
        let sheet = ReferenceSheet::from_frames(&frames, 2, 2, 8).expect("sheet");
        assert_eq!((sheet.cols, sheet.rows), (2, 2));
        assert_eq!(sheet.tiles, vec![1, 2, 3, 4]);
    }

    #[test]
    fn save_then_load_round_trips_keyed_by_the_til() {
        let dir = tempfile::tempdir().unwrap();
        let sheet = ReferenceSheet {
            cols: 2,
            rows: 1,
            tiles: vec![0, 1],
        };
        save(dir.path(), "art/hero.til", &sheet).unwrap();
        assert!(
            dir.path()
                .join(".ggo-ide/art/hero.til.reference.json")
                .exists()
        );
        assert_eq!(load(dir.path(), "art/hero.til"), Some(sheet));
        assert_eq!(load(dir.path(), "art/other.til"), None);
    }

    #[test]
    fn load_corrupt_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(sidecar_rel_path("hero.til"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load(dir.path(), "hero.til"), None);
    }
}
