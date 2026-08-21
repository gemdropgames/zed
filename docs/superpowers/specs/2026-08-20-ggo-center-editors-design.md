# GGO center-pane editors: sprite tabs and the world canvas

2026-08-20. Approved in-chat (clay): sprite editor moves to the center
pane as one tab per file; the world CANVAS moves to the center while the
world panel's dock keeps document ownership and all inspector/entity
editing; opening a world also opens its `.toml` as an adjacent tab.

## Problem

Both GGO editors live in the right dock. The sprite editor is a
single-document panel, so two sprites cannot be open at once and the
editing surface competes with the dock's width. The world canvas is
cramped for the same reason: the user wants to see and move the world
around in a center-pane viewport while keeping the right dock for
entity/inspector editing.

## Goals

- Clicking a `.spr` opens a center-pane tab for THAT file; a second
  `.spr` opens a second tab; re-clicking an open one focuses its tab.
- Clicking a `worlds/**.toml` opens the world canvas as a center-pane
  tab, loads the world into the (still-docked) world panel, and opens
  the `.toml` text file as an adjacent tab in the same pane, with the
  canvas tab active.
- The world panel dock keeps: loading/saving, the inspector, entity
  editing, schemas — everything but the canvas.
- Dirty state and save flow through the workspace's normal item
  close-prompt machinery.

## Non-goals

- Multi-world: the world panel stays single-document; there is exactly
  one canvas item.
- Extracting a standalone world-document entity shared by N views
  (rejected approach: doubles the churn for no user-visible gain).
- Sprite dock panel nostalgia: it is removed, not kept as a second
  surface (two surfaces over one document is a sync bug factory).

## Design

### SpriteEditorItem (crate `ggo_sprite_panel`, new file `sprite_item.rs`)

A workspace `Item` wrapping its own `Entity<SpritePanel>`:

- Construction: `SpriteEditorItem::open(rel, workspace, window, cx)`
  creates a fresh `SpritePanel` entity (the existing type, unmodified
  document logic), points it at `rel` via the existing
  `open_rel_path`/`load_rel_path` flow, and wraps it.
- `Render`: delegates to the inner panel entity (its existing `Render`
  impl).
- `Item`:
  - `tab_content`: file stem; dirty dot via the store's `dirty()`.
  - `is_dirty` / `can_save` / `save`: route to the inner panel's store
    and `save_impl`. The dock-era `prepare_to_close` guard is replaced
    by the workspace's standard dirty-item close prompts.
  - No serialization (item is not restored across restarts in v1); no
    `clone_on_split`.
- Identity: the worktree-relative rel. The interceptor scans the
  workspace's `SpriteEditorItem`s for a matching rel and activates it
  instead of opening a duplicate.

Dock removal: `init()` stops registering `SpritePanel` as a dock panel;
the `ToggleFocus` action and its keybinding are retired. The
`PanelForm` bars, keymap context, and all document logic stay exactly
where they are (they are properties of the view, not of the dock).

### WorldCanvasItem (crate `ggo_world_panel`, new file `world_canvas_item.rs`)

A workspace `Item` holding `WeakEntity<WorldPanel>`:

- The dock `WorldPanel` remains registered and remains the document
  owner: ViewerState, store, loader, save, schemas, inspector, entity
  ops all stay in it.
- The canvas element construction (the `gpui::canvas` scene build plus
  pan/zoom/mouse handlers in `canvas.rs` and `render_canvas`) is
  parameterized so the same code can be built against the panel's
  `OpenWorld` from either view. The item renders the canvas full-tab by
  reading/updating the panel entity; the panel's own render drops the
  canvas and gives the inspector the full dock width.
- `Item`: tab title "World: <stem>"; `is_dirty` mirrors the panel;
  `save` routes to the panel's save. Closing the tab closes only the
  viewport — the document (and any dirt) lives on in the dock panel.
- Single instance: opening a second world re-points the same item
  (matching the panel's single-document model).

### Routing (interceptors)

- `.spr`: claim → focus existing `SpriteEditorItem` for the rel, else
  add a new one to the active pane. No dock involvement.
- `worlds/**.toml`: claim → load into the dock `WorldPanel` (existing
  flow), reveal the dock, open/focus the `WorldCanvasItem` in the
  active pane, then `workspace.open_path` the `.toml` itself as an
  adjacent tab in the same pane, and re-activate the canvas tab.

### Error/edge handling

- A `SpriteEditorItem` whose load fails shows the panel's existing
  Error state in the tab; the tab stays open (same as a broken file).
- Canvas item with no world loaded (panel Empty) renders the panel's
  empty-state message.
- Workspace close with dirty sprite tabs: standard multi-item save
  prompts (workspace machinery), replacing the panel's
  `prepare_to_close` veto.
- The world `.toml` open uses the normal editor path — edits there and
  panel saves can race exactly as they already could when opening the
  file manually; out of scope to reconcile.

### Testing

- Sprite: interceptor claims `.spr` → item appears in active pane; two
  fixture sprites → two tabs; re-click focuses, no duplicate; dirty
  item close-prompts via workspace flow; save through `Item::save`
  writes the trio and clears dirty.
- World: interceptor claims → canvas item + toml tab in pane, canvas
  active, dock panel Ready; canvas item renders against panel state
  (bounds recorded, click routes); closing canvas tab leaves panel
  Ready and dirty state intact.
- Existing sprite panel tests are rewritten to drive the item-wrapped
  panel (the entity API is unchanged, so most rewire mechanically).

## Files touched

- `crates/ggo/sprite_panel/src/sprite_item.rs` (new)
- `crates/ggo/sprite_panel/src/ggo_sprite_panel.rs` (init/routing,
  dock deregistration, test rewiring)
- `crates/ggo/world_panel/src/world_canvas_item.rs` (new)
- `crates/ggo/world_panel/src/ggo_world_panel.rs` (canvas extraction,
  inspector-full-width render, routing)
- `crates/ggo/world_panel/src/canvas.rs` (parameterization only if
  signatures need it)
