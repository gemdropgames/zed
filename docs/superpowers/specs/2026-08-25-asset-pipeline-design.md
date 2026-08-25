# Asset pipeline — design

Task 6 of `tasks/editor-gaps.md`. Six gaps in the import path: no OS
drag-and-drop, no memory of where a tileset came from, PNG only, no
thumbnails, one-shot quantization, no wizard keys.

Decisions taken with the user (2026-08-25): drops land anywhere in the
editor; re-import is checked on open (no watcher); the Aseprite reader is
our own, in worldlib; thumbnails are inline in the project panel; palette
surgery is sort + swap + move; the wizard gets the core key set.

## 1. Library half (`../ggo/tools/ggo-worldlib`)

Everything the editor does not need a window for lives here, tested
without gpui.

### 1.1 `sprites/aseprite.rs`

`pub fn decode_aseprite(bytes: &[u8]) -> Result<Vec<DecodedFrame>, AsepriteError>`
with `DecodedFrame { rgba: Vec<u8>, w: usize, h: usize }` (the canvas
size, every frame). Format: the documented `.ase` layout — 128-byte
header (magic `0xA5E0`, frames, w, h, color depth 32/16/8, transparent
index), per-frame header (magic `0xF1FA`), chunks:

| chunk | handling |
|---|---|
| `0x2004` layer | record visibility, opacity, blend mode, group nesting (a hidden group hides its children) |
| `0x2005` cel | raw (0), linked (1 → the referenced frame's cel), zlib-compressed (2, `flate2`); tilemap (3) → `Unsupported` |
| `0x2019` palette, `0x0004`/`0x0011` old palette | indexed → RGBA lookup; `transparent_index` → alpha 0 |
| everything else (tags `0x2018`, slices, user data, colour profile) | skipped |

Flatten per frame: visible layers bottom-up, normal source-over with
`layer_opacity × cel_opacity / 255²`; other blend modes are treated as
normal (documented limitation). Grayscale = value + alpha. Output is
straight RGBA8, same shape as `DecodedPng`.

Errors are one enum (`BadMagic`, `Truncated`, `Unsupported(&'static str)`,
`Inflate`); the editor shows the message. Tests: hand-built byte fixtures
(a 2×2 RGBA frame, an indexed frame with transparent index, a linked cel,
a hidden layer, a compressed cel, two frames).

### 1.2 `sprites/tileset_meta.rs` — the import record

```rust
pub struct ImportRecord {
    pub source: String,          // project-relative when inside, else absolute
    pub mtime: u64,              // seconds since epoch, 0 when unreadable
    pub crop: Option<(usize, usize, usize, usize)>, // x y w h
    pub reserve_transparent: bool,
    pub as_sprite: bool,
    pub frame_w: Option<usize>,
    pub frame_h: Option<usize>,
}
impl TilesetMeta { pub import: Option<ImportRecord> }   // serde default
pub fn source_changed(record, project_root) -> bool     // mtime differs or file gone
```

Written by the wizard's commit, read by the tileset panel's open and by
the wizard's Re-import. Sidecar path stays
`<worktree>/.ggo-ide/<til-rel>.editor.json`.

### 1.3 `sprites/import.rs` — palette surgery

Pure permutations on a `Preview` (palette + indices move together):

- `sort_by_luma(&mut self, pin_slot0: bool)` — luminance ascending; slot 0
  stays when the transparent slot is reserved.
- `swap(&mut self, a: usize, b: usize)`
- `move_slot(&mut self, from: usize, delta: isize)` — clamped.

Out-of-range indices are ignored, never panic. Tests: permutation keeps
the rendered RGBA identical (`preview_rgba` before == after) for every op.

### 1.4 Source decoding

`import::decode_source(name: &str, bytes: &[u8]) -> Result<Vec<DecodedFrame>, ImportError>`
dispatches on extension: `.png` → one frame via `decode_png`;
`.ase`/`.aseprite` → `decode_aseprite`. The wizard keeps working on frame 0
(`WizardState::new` takes a `DecodedPng`; a `DecodedFrame` converts into
it). The extra frames ride along in the editor's `OpenImport` for sprite
mode.

## 2. Editor half (zed)

### 2.1 Drop anywhere — `workspace`

```rust
pub type ExternalDropInterceptor =
    Arc<dyn Fn(&[PathBuf], &mut Workspace, &mut Window, &mut Context<Workspace>) -> bool>;
pub fn register_external_drop_interceptor(cx: &mut App, f: ExternalDropInterceptor);
impl Workspace { pub fn intercept_external_drop(&mut self, paths, window, cx) -> bool }
```

`Pane::handle_external_paths_drop` asks the workspace first, after the
active item's own `handle_drop` and before the open-paths path. Same
shape and comment style as `intercept_path_open` — a GGO seam, marked as
such, in an upstream file.

`ggo_import_panel::init` registers one: if every dropped path is
`.png`/`.ase`/`.aseprite`, open the first in the wizard (via
`open_abs_source`), focus the panel, and report the ignored count in the
status line; otherwise return `false`.

### 2.2 Wizard keys and actions — `ggo_import_panel`

`actions!(ggo_import, [ToggleFocus, Import, ClearCrop, ChooseSource, Reimport, ZoomIn, ZoomOut])`
bound in `bind_panel_keys` under the panel's key context (re-bound on
`KeymapEventChannel` like every GGO panel): `enter` Import,
`escape`/`delete`/`backspace` ClearCrop, `ctrl-o`/`cmd-o` ChooseSource,
`ctrl-r`/`cmd-r` Reimport, `=`/`+` ZoomIn, `-` ZoomOut. Enter inside a
destination field still commits (the field's own binding is the same
action). Escape with no crop is a no-op.

### 2.3 Aseprite in the wizard

`loader` decodes through `decode_source`. `OpenImport` gains
`frames: Vec<DecodedFrame>`. In sprite mode with more than one frame the
frame W/H fields are hidden and `frame_rects` is one rect per frame
(the crop, applied to every frame); `sprite_import` is called once per
frame's RGBA. A single-frame source behaves exactly as today. The PNG
context-menu entry and `is_png_path` extend to the two Aseprite
extensions ("Import as tileset…" on a `.ase`).

### 2.4 Remember the source; re-import on change

`commit` writes `TilesetMeta.import = Some(record)` into the written
`.til`'s sidecar (load-modify-save, keeping cols/zoom/terrains), keyed by
the `.til`'s worktree-relative path; a `.til` outside the worktree gets
no record.

Tileset panel: on a successful open, read the sidecar; when
`source_changed` → a banner under the header: `Source <name> changed —`
**Re-import…** / **Dismiss**. Re-import opens the import panel on the
recorded source with crop, reserve-transparent, as-sprite and frame cut
restored and the destination prefilled to this `.til`; the user presses
Enter (Import). The wizard's own `Reimport` action does the same for the
source currently open (looks the record up by the destination it would
write to). A missing source shows the banner with "(missing)" and no
Re-import button.

### 2.5 Thumbnails — `workspace` + `ggo_common` + `project_panel`

```rust
// workspace
pub trait ThumbnailProvider {
    fn thumbnail(&self, path: &Path, cx: &mut App) -> Option<Arc<RenderImage>>;
    fn entity_id(&self) -> EntityId; // for the project panel to observe
}
pub fn set_thumbnail_provider(cx, Arc<dyn ThumbnailProvider>); pub fn thumbnail_provider(cx) -> Option<..>
```

`ggo_common::ThumbnailCache` (entity, one per app) implements it: a miss
records the path, spawns a background decode (`.til` → `compose_tile_grid`
first 8 tiles wide; `.spr` → first frame via worldlib preview compose;
`.png` → `image` decode), nearest-neighbour downscales to 16×16 into an
`Arc<RenderImage>`, stores it keyed by (path, mtime) and notifies. The
project panel: at construction, `cx.observe` the provider's entity so a
finished decode re-renders; in `render_entry`, when the provider returns
an image for a file, an `img(image)` 16×16 replaces the file icon. Retired
images are dropped via `cx.drop_image` when a key is replaced (the atlas
contract from task 3). Cap: 512 entries, LRU by insertion order.

### 2.6 Palette surgery UI — `ggo_import_panel::render_palette`

Tileset mode: swatches are buttons — click one, click another → `swap`;
a selected swatch shows ◀ ▶ (`move_slot ±1`); **Sort** (`sort_by_luma`,
pinned when reserve-transparent) and **Reset** (re-quantize) buttons. The
ops apply to the settled `Preview` the commit writes. Changing the crop
re-quantizes and discards the edits (stated in a tooltip). Sprite mode
quantizes the whole source in `sprite_import` and ignores the preview, so
the row is read-only there with a "palette editing: tileset mode" hint.

## 3. Testing

- worldlib: aseprite fixtures (§1.1), palette permutation invariants,
  `source_changed`, `decode_source` dispatch, `ImportRecord` round-trip.
- import panel: drop interceptor claims `.png` and rejects `.txt`;
  keys dispatch (`enter` commits, `escape` clears the crop); `.ase` source
  loads and sprite mode writes N frames; commit writes the record; Reimport
  restores crop + dest; palette swap then commit writes the permuted
  `.pal` and remapped tiles.
- tileset panel: banner appears when the source mtime differs, absent
  when equal, "(missing)" when gone.
- project panel: entry render shows the thumbnail image when the provider
  has one (a fake provider in the test).

## 4. Out of scope

Live file watching; Aseprite tags → clips, tilemap layers, non-normal
blend modes; an asset-browser dock; thumbnails for `.map`/worlds.
