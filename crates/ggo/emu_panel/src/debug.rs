//! The debug column: tiles / tilemap / OAM / palette viewers decoded from
//! the drive thread's [`PpuSnapshot`] (see `drive::Session::snapshot`).
//!
//! Everything that turns a snapshot into pixels is a pure function here,
//! tested against a hand-built snapshot; the pane owns the state
//! ([`DebugState`]) and the throttle. Images follow the pane's atlas
//! contract (module doc of `ggo_emu_panel`): every `RenderImage` a viewer
//! replaces goes into [`DebugState::retired`] and is `drop_image`d on the
//! next render, and all of them are dropped when the column closes or the
//! pane is torn down.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{RenderImage, Task};

use ggo_emu_core::peripherals::{SCREEN_HEIGHT, SCREEN_WIDTH};
use ggo_emu_core::ppu::{
    BANK_BGFG, BANK_SPRITE, LAYER_COUNT, MAP_H, MAP_W, OAM_ENTRIES, OamEntry, PAL_ENTRIES,
    PALETTES, PpuSnapshot, TILE_PX, VRAM_TILE_CAP,
};

/// The tile sheet: 1024 tiles as a 32×32 grid at 1×.
pub const SHEET_TILES_PER_ROW: usize = 32;
pub const SHEET_PX: usize = SHEET_TILES_PER_ROW * TILE_PX;
/// A tilemap layer at 1×: 32×32 cells of 16 px.
pub const MAP_PX: usize = MAP_W * TILE_PX;
/// How often the viewers re-decode while the cart runs. Paused/stepped
/// snapshots decode immediately.
pub const LIVE_DECODE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DebugTab {
    Tiles,
    Map,
    Oam,
    Palettes,
}

impl DebugTab {
    pub const ALL: [DebugTab; 4] = [
        DebugTab::Tiles,
        DebugTab::Map,
        DebugTab::Oam,
        DebugTab::Palettes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DebugTab::Tiles => "Tiles",
            DebugTab::Map => "Map",
            DebugTab::Oam => "OAM",
            DebugTab::Palettes => "Palettes",
        }
    }
}

/// What one decode pass produced for the active tab.
pub struct Decoded {
    pub tab: DebugTab,
    /// `SHEET_PX²` (Tiles), `MAP_PX²` (Map) or `SCREEN_WIDTH×SCREEN_HEIGHT`
    /// (OAM composite); `None` for Palettes, which paints from the
    /// snapshot directly.
    pub image: Option<Arc<RenderImage>>,
    pub snapshot: Arc<PpuSnapshot>,
}

pub struct DebugState {
    pub open: bool,
    pub tab: DebugTab,
    /// Tiles tab: which bank / palette colors the sheet.
    pub bank: usize,
    pub palette: usize,
    /// Map tab: which layer.
    pub layer: usize,
    /// The last decode, whose image the active tab paints.
    pub decoded: Option<Decoded>,
    /// Images replaced since the last render. Two-stage, like the pane's
    /// frame double buffer: a render moves these to `retired_previous`
    /// and drops what was there -- so an image is never handed back to
    /// the atlas while the scene that last painted it may still be in
    /// flight.
    pub retired: Vec<Arc<RenderImage>>,
    pub retired_previous: Vec<Arc<RenderImage>>,
    pub hover: Option<String>,
    pub generation: u64,
    pub last_decode_started: Option<Instant>,
    /// Identity of the snapshot the last decode used, so a paused run
    /// (same `Arc` every render) is decoded once, not every frame.
    pub last_decoded_ptr: usize,
    pub task: Option<Task<()>>,
}

impl DebugState {
    pub fn new() -> Self {
        DebugState {
            open: false,
            tab: DebugTab::Tiles,
            bank: BANK_BGFG,
            palette: 0,
            layer: 0,
            decoded: None,
            retired: Vec::new(),
            retired_previous: Vec::new(),
            hover: None,
            generation: 0,
            last_decode_started: None,
            last_decoded_ptr: 0,
            task: None,
        }
    }

    /// Replace the current decode, queueing its image for atlas release.
    pub fn set_decoded(&mut self, decoded: Decoded) {
        if let Some(old) = self.decoded.take()
            && let Some(image) = old.image
        {
            self.retired.push(image);
        }
        self.decoded = Some(decoded);
    }

    /// Queue every image for release (run stopped, column closed, pane
    /// torn down); the caller drains [`Self::take_all_retired`]. The
    /// snapshot itself is kept, so a tab or selector change after a run
    /// ends still has something to decode.
    pub fn retire_all(&mut self) {
        if let Some(decoded) = &mut self.decoded
            && let Some(image) = decoded.image.take()
        {
            self.retired.push(image);
        }
        self.last_decoded_ptr = 0;
    }

    /// Advance the two-stage queue by one render: what was retired before
    /// the previous render is returned to be dropped now.
    pub fn take_retired_for_render(&mut self) -> Vec<Arc<RenderImage>> {
        let drop_now = std::mem::take(&mut self.retired_previous);
        self.retired_previous = std::mem::take(&mut self.retired);
        drop_now
    }

    /// Both stages at once, for teardown.
    pub fn take_all_retired(&mut self) -> Vec<Arc<RenderImage>> {
        let mut all = std::mem::take(&mut self.retired_previous);
        all.append(&mut self.retired);
        all
    }

    /// Whether a decode of `snapshot` is due now: never for the snapshot
    /// already decoded; always when `immediate` (paused -- identity changes
    /// only on step -- or no run at all); else at most every
    /// [`LIVE_DECODE_INTERVAL`].
    pub fn decode_due(&self, snapshot: &Arc<PpuSnapshot>, immediate: bool, now: Instant) -> bool {
        let ptr = Arc::as_ptr(snapshot) as usize;
        if ptr == self.last_decoded_ptr {
            return false;
        }
        if immediate {
            return true;
        }
        self.last_decode_started
            .is_none_or(|started| now.duration_since(started) >= LIVE_DECODE_INTERVAL)
    }
}

// ------------------------------------------------------------- decoders

/// Copy a 16×16 RGBA tile into a BGRA canvas at `(x, y)`, clipping to the
/// canvas. Transparent source pixels (alpha 0) are skipped, so sprites
/// composite over what is already there.
fn blit_tile_bgra(canvas: &mut [u8], width: usize, height: usize, x: i32, y: i32, tile: &[u8]) {
    for ty in 0..TILE_PX {
        let cy = y + ty as i32;
        if cy < 0 || cy >= height as i32 {
            continue;
        }
        for tx in 0..TILE_PX {
            let cx = x + tx as i32;
            if cx < 0 || cx >= width as i32 {
                continue;
            }
            let s = (ty * TILE_PX + tx) * 4;
            if tile[s + 3] == 0 {
                continue;
            }
            let d = (cy as usize * width + cx as usize) * 4;
            canvas[d] = tile[s + 2];
            canvas[d + 1] = tile[s + 1];
            canvas[d + 2] = tile[s];
            canvas[d + 3] = 0xFF;
        }
    }
}

/// The 1024-tile sheet through `bank`/`palette`, BGRA `SHEET_PX²`.
pub fn tile_sheet_bgra(snapshot: &PpuSnapshot, bank: usize, palette: usize) -> Vec<u8> {
    let mut canvas = vec![0u8; SHEET_PX * SHEET_PX * 4];
    let mut tile = vec![0u8; TILE_PX * TILE_PX * 4];
    for index in 0..VRAM_TILE_CAP {
        snapshot.tile_rgba(index as u16, bank, palette, false, false, &mut tile);
        let x = (index % SHEET_TILES_PER_ROW) * TILE_PX;
        let y = (index / SHEET_TILES_PER_ROW) * TILE_PX;
        blit_tile_bgra(&mut canvas, SHEET_PX, SHEET_PX, x as i32, y as i32, &tile);
    }
    canvas
}

/// One tilemap layer composed at 1× with each cell's own palette and
/// flips (bank 0, as the compositor draws layers), BGRA `MAP_PX²`.
pub fn map_bgra(snapshot: &PpuSnapshot, layer: usize) -> Vec<u8> {
    let mut canvas = vec![0u8; MAP_PX * MAP_PX * 4];
    let mut tile = vec![0u8; TILE_PX * TILE_PX * 4];
    for cy in 0..MAP_H {
        for cx in 0..MAP_W {
            let cell = snapshot.map_cell(layer, cx, cy);
            snapshot.tile_rgba(
                cell.tile,
                BANK_BGFG,
                cell.palette as usize,
                cell.hflip,
                cell.vflip,
                &mut tile,
            );
            blit_tile_bgra(
                &mut canvas,
                MAP_PX,
                MAP_PX,
                (cx * TILE_PX) as i32,
                (cy * TILE_PX) as i32,
                &tile,
            );
        }
    }
    canvas
}

/// Every enabled sprite drawn at its position on a screen-sized canvas
/// (sprite bank, its own palette, size and flips; affine ignored), BGRA
/// `SCREEN_WIDTH×SCREEN_HEIGHT`. Lower OAM indices draw last, so they
/// win overlaps like the hardware's sprite-vs-sprite rule.
pub fn oam_composite_bgra(snapshot: &PpuSnapshot) -> Vec<u8> {
    let (width, height) = (SCREEN_WIDTH, SCREEN_HEIGHT);
    let mut canvas = vec![0u8; width * height * 4];
    let mut tile = vec![0u8; TILE_PX * TILE_PX * 4];
    for index in (0..OAM_ENTRIES).rev() {
        let entry = snapshot.oam_entry(index);
        if !entry.enabled {
            continue;
        }
        let side = entry.tiles_per_side as usize;
        for row in 0..side {
            for col in 0..side {
                // Flipping a multi-tile sprite mirrors which tile lands
                // where as well as each tile's pixels.
                let src_col = if entry.hflip { side - 1 - col } else { col };
                let src_row = if entry.vflip { side - 1 - row } else { row };
                let tile_index = entry.tile as usize + src_row * side + src_col;
                snapshot.tile_rgba(
                    tile_index as u16,
                    BANK_SPRITE,
                    entry.palette as usize,
                    entry.hflip,
                    entry.vflip,
                    &mut tile,
                );
                blit_tile_bgra(
                    &mut canvas,
                    width,
                    height,
                    entry.x as i32 + (col * TILE_PX) as i32,
                    entry.y as i32 + (row * TILE_PX) as i32,
                    &tile,
                );
            }
        }
    }
    canvas
}

/// OAM rows for the list: enabled entries first (by index), then the
/// rest, each with its index.
pub fn oam_rows(snapshot: &PpuSnapshot) -> Vec<(usize, OamEntry)> {
    let mut rows: Vec<(usize, OamEntry)> = (0..OAM_ENTRIES)
        .map(|i| (i, snapshot.oam_entry(i)))
        .collect();
    rows.sort_by_key(|(index, entry)| (!entry.enabled, *index));
    rows
}

pub fn oam_row_label(index: usize, entry: &OamEntry) -> String {
    format!(
        "#{index:03} {:>4},{:>4}  tile {:>4}  {}x{}  {}{}  pal {:>2}  prio {}{}",
        entry.x,
        entry.y,
        entry.tile,
        entry.tiles_per_side,
        entry.tiles_per_side,
        if entry.hflip { "H" } else { "-" },
        if entry.vflip { "V" } else { "-" },
        entry.palette,
        entry.priority,
        if entry.affine { "  affine" } else { "" }
    )
}

/// A palette entry as `#RRGGBB (0xNNNN)`.
pub fn rgb565_label(rgb565: u16) -> String {
    let argb = ggo_emu_core::peripherals::rgb565_to_argb(rgb565);
    format!("#{:06X} (0x{rgb565:04X})", argb & 0x00FF_FFFF)
}

/// The palette grid's cell under a point, `(bank, palette, entry)`.
pub fn palette_cell_at(x: f32, y: f32, swatch_px: f32) -> Option<(usize, usize, usize)> {
    if x < 0.0 || y < 0.0 || swatch_px <= 0.0 {
        return None;
    }
    let entry = (x / swatch_px) as usize;
    let row = (y / swatch_px) as usize;
    if entry >= PAL_ENTRIES || row >= 2 * PALETTES {
        return None;
    }
    Some((row / PALETTES, row % PALETTES, entry))
}

/// Which layers are enabled, for greying out the Map tab's selector.
pub fn layer_labels(snapshot: &PpuSnapshot) -> [String; LAYER_COUNT] {
    core::array::from_fn(|layer| {
        format!(
            "BG{layer}{}",
            if snapshot.layer_enable[layer] {
                format!(" (prio {})", snapshot.layer_prio[layer])
            } else {
                " (off)".to_string()
            }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_emu_core::ppu::{Ppu, TILE_BYTES};

    /// A snapshot with tile 1 = solid palette index 1, tile 2 = only its
    /// top-left pixel set, red in bg palette 0 / blue in bg palette 1 /
    /// green in sprite palette 2.
    fn fixture() -> PpuSnapshot {
        let mut ppu = Ppu::new();
        let solid = [0x11u8; TILE_BYTES];
        ppu.load_tiles(1, &solid);
        let mut corner = [0u8; TILE_BYTES];
        corner[0] = 0x01;
        ppu.load_tiles(2, &corner);
        ppu.set_palette(BANK_BGFG, 1, 0xF800);
        ppu.set_palette(BANK_BGFG, 16 + 1, 0x001F);
        ppu.set_palette(BANK_SPRITE, 2 * 16 + 1, 0x07E0);
        ppu.snapshot()
    }

    fn px(canvas: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * width + x) * 4;
        [canvas[i], canvas[i + 1], canvas[i + 2], canvas[i + 3]]
    }

    #[test]
    fn the_tile_sheet_places_tiles_in_a_32_wide_grid_through_the_chosen_palette() {
        let snap = fixture();
        let sheet = tile_sheet_bgra(&snap, BANK_BGFG, 0);
        assert_eq!(sheet.len(), SHEET_PX * SHEET_PX * 4);
        // Tile 1 sits at column 1, row 0: red, BGRA order.
        assert_eq!(px(&sheet, SHEET_PX, 16, 0), [0, 0, 0xFF, 0xFF]);
        assert_eq!(px(&sheet, SHEET_PX, 31, 15), [0, 0, 0xFF, 0xFF]);
        // Tile 0 is transparent.
        assert_eq!(px(&sheet, SHEET_PX, 0, 0), [0, 0, 0, 0]);
        // Tile 33 (row 1, col 1) is empty VRAM.
        assert_eq!(px(&sheet, SHEET_PX, 16, 16), [0, 0, 0, 0]);
        let blue = tile_sheet_bgra(&snap, BANK_BGFG, 1);
        assert_eq!(px(&blue, SHEET_PX, 16, 0), [0xFF, 0, 0, 0xFF]);
    }

    #[test]
    fn the_map_composes_cells_with_their_own_palette_and_flips() {
        let mut ppu = Ppu::new();
        let mut corner = [0u8; TILE_BYTES];
        corner[0] = 0x01;
        ppu.load_tiles(2, &corner);
        ppu.set_palette(BANK_BGFG, 1, 0xF800);
        ppu.set_palette(BANK_BGFG, 16 + 1, 0x001F);
        // Cell (1,0): tile 2, palette 0, no flip -> red at (16,0).
        ppu.set_map_cell(0, 1, 2);
        // Cell (0,1): tile 2, palette 1, h+v flipped -> blue at (15,31).
        ppu.set_map_cell(0, 32, 2 | (1 << 10) | 0x4000 | 0x8000);
        let snap = ppu.snapshot();
        let map = map_bgra(&snap, 0);
        assert_eq!(map.len(), MAP_PX * MAP_PX * 4);
        assert_eq!(px(&map, MAP_PX, 16, 0), [0, 0, 0xFF, 0xFF]);
        assert_eq!(px(&map, MAP_PX, 17, 0), [0, 0, 0, 0]);
        assert_eq!(px(&map, MAP_PX, 15, 31), [0xFF, 0, 0, 0xFF]);
        // Layer 1 is empty.
        assert!(map_bgra(&snap, 1).iter().all(|b| *b == 0));
    }

    #[test]
    fn the_oam_composite_draws_enabled_sprites_at_their_position_and_clips() {
        let mut ppu = Ppu::new();
        let solid = [0x11u8; TILE_BYTES];
        ppu.load_tiles(1, &solid);
        ppu.set_palette(BANK_SPRITE, 2 * 16 + 1, 0x07E0);
        // #5: enabled, 1x1, tile 1, palette 2, at (10, 20).
        ppu.oam_set(5, 10, 20, 1, 0x0200 | 0x10);
        // #7: disabled, would cover (100, 100).
        ppu.oam_set(7, 100, 100, 1, 0x0200);
        // #9: enabled 2x2 at (-8, 230): partly off both edges.
        ppu.oam_set(9, -8, 230, 1, 0x0200 | 0x10 | 0x01);
        let snap = ppu.snapshot();
        let composite = oam_composite_bgra(&snap);
        assert_eq!(composite.len(), SCREEN_WIDTH * SCREEN_HEIGHT * 4);
        assert_eq!(px(&composite, SCREEN_WIDTH, 10, 20), [0, 0xFF, 0, 0xFF]);
        assert_eq!(px(&composite, SCREEN_WIDTH, 25, 35), [0, 0xFF, 0, 0xFF]);
        assert_eq!(px(&composite, SCREEN_WIDTH, 26, 20), [0, 0, 0, 0]);
        assert_eq!(
            px(&composite, SCREEN_WIDTH, 100, 100),
            [0, 0, 0, 0],
            "disabled"
        );
        assert_eq!(
            px(&composite, SCREEN_WIDTH, 0, 239),
            [0, 0xFF, 0, 0xFF],
            "clipped sprite still draws inside"
        );

        let rows = oam_rows(&snap);
        assert_eq!(rows[0].0, 5);
        assert_eq!(rows[1].0, 9);
        assert!(
            !rows[2].1.enabled,
            "disabled entries follow, in index order"
        );
        assert_eq!(rows[2].0, 0);
        assert!(
            oam_row_label(5, &rows[0].1)
                .starts_with("#005   10,  20  tile    1  1x1  --  pal  2  prio 0")
        );
    }

    #[test]
    fn palette_helpers_label_and_locate_swatches() {
        assert_eq!(rgb565_label(0xF800), "#FF0000 (0xF800)");
        assert_eq!(rgb565_label(0x07E0), "#00FF00 (0x07E0)");
        assert_eq!(palette_cell_at(0.0, 0.0, 12.0), Some((0, 0, 0)));
        assert_eq!(
            palette_cell_at(12.0 * 15.5, 12.0 * 16.0, 12.0),
            Some((1, 0, 15))
        );
        assert_eq!(palette_cell_at(12.0 * 16.0, 0.0, 12.0), None);
        assert_eq!(palette_cell_at(0.0, 12.0 * 32.0, 12.0), None);
        assert_eq!(palette_cell_at(-1.0, 0.0, 12.0), None);
    }

    #[test]
    fn decode_is_due_once_per_snapshot_when_paused_and_throttled_when_running() {
        let mut state = DebugState::new();
        let snap = Arc::new(fixture());
        let now = Instant::now();
        assert!(state.decode_due(&snap, true, now));
        state.last_decoded_ptr = Arc::as_ptr(&snap) as usize;
        assert!(
            !state.decode_due(&snap, true, now),
            "same snapshot, already decoded"
        );
        let next = Arc::new(fixture());
        state.last_decode_started = Some(now);
        assert!(!state.decode_due(&next, false, now), "running: throttled");
        assert!(state.decode_due(&next, false, now + LIVE_DECODE_INTERVAL));
        assert!(
            state.decode_due(&next, true, now),
            "paused: a new snapshot decodes at once"
        );
        let labels = layer_labels(&snap);
        assert_eq!(labels[0], "BG0 (off)");
    }

    #[test]
    fn retirement_is_two_stage_per_render_and_immediate_on_teardown() {
        let mut state = DebugState::new();
        let snap = Arc::new(fixture());
        let image = |n: u32| {
            let buffer = image::ImageBuffer::from_raw(1, 1, vec![n as u8; 4]).unwrap();
            Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
        };
        state.set_decoded(Decoded {
            tab: DebugTab::Tiles,
            image: Some(image(1)),
            snapshot: snap.clone(),
        });
        state.set_decoded(Decoded {
            tab: DebugTab::Tiles,
            image: Some(image(2)),
            snapshot: snap,
        });
        assert_eq!(state.retired.len(), 1);
        assert!(
            state.take_retired_for_render().is_empty(),
            "first render: nothing old enough"
        );
        assert_eq!(
            state.take_retired_for_render().len(),
            1,
            "second render: drops it"
        );
        state.retire_all();
        assert_eq!(state.take_all_retired().len(), 1);
        let kept = state.decoded.as_ref().expect("the snapshot is kept");
        assert!(kept.image.is_none(), "only the image was released");
    }
}
