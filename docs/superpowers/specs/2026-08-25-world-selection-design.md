# World editor interaction floor — design

Status: approved 2026-08-25. Task 4 of `tasks/editor-gaps.md`.

## Goal

Multi-select (shift-click, rubber-band, select-all), group move as one
undo, copy/paste/duplicate through the OS clipboard as TOML, a confirm
before removing instances, an entity/instance list in the dock, and two
fixes: redo of an add-instance leaving a placeholder, and the world
panel's atlas leak.

## Facts this rests on

- `OpenWorld::selected: Option<Selection>` (`Selection::{Entity(i),
  Instance(i)}`, index-based) read by the draw list (outline), delete,
  nudge, drag, the inspector and the toolbar.
- `WorldDocStore::apply` coalesces `MoveEntity`/`MoveInstance` by
  `(kind, index, gesture)` into the top undo entry; no batch op exists.
- `AddInstance`'s inverse snapshots `resolved: None`; redo re-pushes it,
  so the instance renders as a placeholder until reload.
- `open.images` (`HashMap<usize, Arc<RenderImage>>`) is rebuilt on load,
  add-instance and every asset-naming field commit; nothing ever calls
  `drop_image`.
- Left-drag on empty canvas currently deselects and does nothing else.
- The tileset panel's `begin_stroke`/`end_stroke` is the in-fork precedent
  for folding many changes into one undo entry.

## Decisions

1. **Selection is an ordered set**: `selected: Vec<Selection>`, deduped,
   last element = primary (what the inspector edits). Click replaces,
   shift-click toggles, empty-space left-drag rubber-bands (bbox
   intersection; shift adds), `ctrl-a` selects all, `escape` clears.
   Structural ops (remove, undo/redo) clear the set, as today.
2. **Two new worldlib ops** (`../ggo/tools/ggo-worldlib/src/world_doc.rs`):
   - `WorldOp::MoveMany { moves: Vec<(Selection, [f64; 2])>, gesture:
     Option<String> }` — inverse holds the previous positions; coalesces on
     `(MoveMany, gesture)` like the single moves (amends the top entry's
     new positions). Used by drag and nudge when more than one item is
     selected; a single item keeps `MoveEntity`/`MoveInstance`.
   - `WorldOp::Batch(Vec<WorldOp>)` — applies in order, inverse is the
     list of inverses, undone in reverse; one undo entry. Used by paste,
     duplicate and multi-delete. Nested gestures inside a batch are not
     coalesced (a batch always seals the active gesture).
3. **Clipboard = TOML text** on the OS clipboard: the selected
   `[[entity]]` / `[[instance]]` tables in world-file syntax, encoded and
   parsed by worldlib's world-file codec (a `WorldFile` fragment with no
   backgrounds). Duplicate does the same round trip without touching the
   clipboard.
4. **Paste placement**: if the cursor is over the world canvas, the
   group's top-left position (min x, min y over the pasted items) lands
   at the cursor's world position (snapped to the tile grid when Snap is
   on); otherwise every item is offset by one tile (+16, +16) from its
   copied position. The pasted set becomes the selection. Pasted
   instances go through the same cycle guard as "+ Instance"; a rejected
   stem is reported on the toolbar and skipped.
5. **Delete** applies one `Batch` of removes in descending index order.
   A confirm (`ggo_common::confirm_destructive`) is shown only when the
   set contains at least one instance ("Remove N instance(s)?"); entities
   alone delete without a prompt (undoable).
6. **Entity list**: a column beside the inspector in the dock's third
   row, `LIST_WIDTH = 140`: one row per entity (`#i <label>` where the
   label is the first non-Transform component name, plus a `Sprite` /
   `MetaSprite` stem when present) and per instance (`⧉ <stem>`).
   Click / shift-click select like the canvas; selected rows highlighted;
   the list scrolls.
7. **Redo re-resolve**: after `store.redo()`, any instance with neither
   `resolved` nor `error` is resolved (`loader::resolve_instance`),
   `set_instances_resolved(.., false)`, asset loads filled, images rebuilt.
8. **Atlas retirement**: every image-map rebuild computes the keys the
   new map no longer holds and queues their images; the canvas render
   drops the queue two-stage (what was queued before the previous
   render), and `on_release` drops everything.

## Keys (`GgoWorldPanel`)

`ctrl/cmd-c` Copy, `ctrl/cmd-v` Paste, `ctrl/cmd-d` Duplicate,
`ctrl/cmd-a` SelectAll, `escape` ClearSelection. Existing Delete /
Nudge / Undo / Redo unchanged.

## Tests

- worldlib: `MoveMany` coalesces within a gesture and seals across;
  `Batch` is one undo/redo entry and restores indices; a batch seals an
  active gesture.
- Panel: shift-click toggles; rubber-band selects by bbox; group drag
  produces one undo entry; nudge with a set; copy→clipboard TOML→paste
  round trip for an entity + instance with both placement rules; paste
  of a cycling instance is refused and reported; duplicate keeps the
  clipboard untouched; delete of entities is immediate, of instances
  gated (tested through the batch path with the confirm bypassed);
  list rows and click selection; redo of an add-instance re-resolves;
  image-map rebuild queues exactly the dropped keys.

## Out of scope

Lasso selection, alignment/distribute tools, prefab overrides, drag
between worlds, undo grouping across paste + move.
