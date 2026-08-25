# World editor interaction floor — plan

Spec: `docs/superpowers/specs/2026-08-25-world-selection-design.md`.
Branches: zed `world-selection` (off `ggo`), ggo `world-batch-ops` (off `main`).

## Phase A — worldlib ops (`../ggo/tools/ggo-worldlib`)
What: `WorldOp::MoveMany` (gesture-coalesced), `WorldOp::Batch`; world-file
fragment encode/parse for the clipboard; rect hit set.
Why: one undo entry per user gesture needs store support.
1. `world_doc.rs`: variants, inverses, apply/undo/redo, tests.
2. `world_file.rs`: `fragment_to_toml(&[WorldEntity], &[WorldInstance])` /
   `parse_fragment(&str)`. `render.rs`: `items_in_rect`. Tests.
3. Commit, push, merge to `main`.

## Phase B — selection set (zed)
4. `selected: Vec<Selection>`; readers updated; shift-click; primary.
5. Rubber-band: marquee state on `EditDrag`/canvas, overlay draw, hit set.
6. `ctrl-a` / `escape`. Group drag + nudge via `MoveMany`. Tests. Commit.

## Phase C — clipboard, delete, list
7. Copy / Paste / Duplicate with placement rules; cursor tracking on the canvas.
8. Delete as `Batch` + instance confirm.
9. Entity list column. Tests. Commit.

## Phase D — fixes
10. Redo re-resolve. 11. Atlas retirement + `on_release`. Tests. Commit.

## Phase E — wrap
12. clippy + sweep; `MIGRATION.md` rows; tick task 4; review; merge → `ggo`, push `main`.
