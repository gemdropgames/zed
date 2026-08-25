# Editor chrome — plan

Spec: `docs/superpowers/specs/2026-08-25-editor-chrome-design.md`.
Branch: zed `chrome` (off `ggo`). No ggo-repo changes.

## Phase A — map editor as a center tab
What: `map_item.rs` + `open_map_item`; interceptor and New Map route to
it; dock registration removed; tests ported.
Why: one editing surface shape for `.til` and `.map`.

## Phase B — import wizard as a center tab
What: `import_item.rs` (singleton) + `open_import_item`; menu, drop,
re-import, picker route to it; dock registration removed.

## Phase C — keybindings in the keymap assets
What: GGO context blocks in `default-linux.json` / `default-macos.json`;
`bind_panel_keys` + reload observers deleted;
`ggo_common::test_support::bind_ggo_keymap`; keystroke tests use it.
Why: the keymap editor sees and rebinds them.

## Phase D — `ui::Slider`
What: the component + test; replace the steppers (sprite onion opacity,
preview size; zoom in tileset/map/import/world; map palSub).

## Phase E — wrap
Sweep tests + clippy (map, import, tileset, sprite, world, emu, charts,
common, ui); MIGRATION.md rows (map/import shape, keymaps, slider); tick
the four task-10 items; review agent; fix; merge `chrome` → `ggo`, push
`ggo` + `main`.
