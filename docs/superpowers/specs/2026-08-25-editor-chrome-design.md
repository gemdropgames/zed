# Editor chrome — design

Task 10 of `tasks/editor-gaps.md`, four of its eight items (the emerald
and charts items are deferred to their own tasks): map editor as a center
tab, import wizard as a center tab, keybindings declared in the keymap
assets, and a slider primitive replacing steppers.

## 1. Map editor → center tab per `.map`

`ggo_map_panel::map_item::MapEditorItem` mirrors `tileset_item.rs`: it
owns an `Entity<MapPanel>`, opens `rel` into it, observes the panel to
refresh the tab, and implements `Item` (`tab_content_text` = file stem,
`is_dirty`/`can_save`/`save` route to the panel's document). Public
`open_map_item(workspace, rel, window, cx)` activates an existing item for
the rel or adds one to the active pane (the tileset rule: one tab per
file).

`intercept_map_open` calls `open_map_item`; "New Map…" creates the file
then calls it. The dock registration (`add_panel`, `ToggleFocus`, the
`Panel` impl, `position`, `DEFAULT_WIDTH`, `set_active`) is removed; the
dirty guard moves to the item's `Item::save`/`is_dirty` (the workspace's
own close-dirty prompt covers closing), and `prepare_to_close_dirty` is
no longer needed here. Tests that reached the panel through
`workspace.panel::<MapPanel>()` use the item instead.

## 2. Import wizard → center tab

`ggo_import_panel::import_item::ImportItem`: a singleton tab (the
emulator's shape, `open_emu_item`) wrapping the one `ImportPanel`.
`open_import_item(workspace, window, cx, f)` activates the existing tab or
adds one, then runs `f` on the panel. The context-menu entry, the OS-drop
interceptor, the `ReimportTileset` handler and `pick_source` all route
through it; `add_panel`/`ToggleFocus`/`Panel` go away. Tab text:
"Import" (or "Import · <source name>" once a source is open); the tab is
never dirty (a pending import is not a document).

## 3. Keybindings in the keymap assets

Every GGO binding moves into `assets/keymaps/default-linux.json` and
`default-macos.json` as `{"context": "<KEY_CONTEXT>", "bindings": {...}}`
blocks, actions named `ggo_<ns>::<Action>` — one block per panel context
that binds today (map, tileset, import, sprite, world, emu, charts,
emerald, audio; whatever `bind_panel_keys` fns exist). `ctrl-` on Linux,
`cmd-` on macOS, mirroring upstream. The imperative `bind_panel_keys`
fns and their `KeymapEventChannel` observers are deleted, so the keymap
editor lists and rebinds these like any other action, and a settings
reload rebuilds them from the asset like everything else.

Tests keep their tripwire role: `ggo_common::test_support::bind_ggo_keymap(cx)`
loads the Linux asset through `settings::KeymapFile`, keeps only the GGO
context blocks, and binds them; every panel's keystroke test calls it
where it called `init` before. A binding that goes missing from the asset
fails the test that used it.

## 4. `ui::Slider`

`crates/ui/src/components/slider.rs`, a `RenderOnce` component:

```rust
Slider::new(id, value: f32 /* 0.0..=1.0 */)
    .width(px)            // default 96px
    .label(impl Into<SharedString>)   // optional, drawn before the track
    .on_change(impl Fn(f32, &mut Window, &mut App) + 'static)
```

A track with a thumb; mouse-down and drag on the track set the value from
the x position (clamped), calling `on_change` on every change. No
keyboard handling of its own (the panels keep their key/wheel steps).
Callers map their integer ranges: `value = (v - min) / (max - min)`,
snapping on change.

Replacements: sprite onion opacity and preview size; zoom in tileset, map,
import and world; palSub in map. The `-`/`+` steppers those rows had are
removed; the readout label stays beside the slider.

## 5. Testing

- Map item: wraps, mirrors dirty, routes save (the tileset item test
  ported); `intercept_map_open` opens one tab per rel and re-activates it.
- Import item: singleton; drop / menu / re-import land in the tab.
- Keymap: every existing keystroke test passes through the asset helper;
  a test asserts the asset parses and each GGO context block is present.
- Slider: a component test drives mouse-down at 25% / drag to 75% and
  checks `on_change` values; one panel test (map zoom) checks the slider
  callback steps the zoom.

## 6. Out of scope

Emerald cadence/reorder/undo, charts ignore-set and file split,
keyboard handling inside the slider, a modal import wizard.
