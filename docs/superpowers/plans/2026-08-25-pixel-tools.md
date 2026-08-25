# Pixel tools — plan

Spec: `docs/superpowers/specs/2026-08-25-pixel-tools-design.md`.
Branches: zed `pixel-tools` (off `ggo`), ggo `pixel-tools` (off `main`).

## Phase A — worldlib
What: `sprites/pixel_tools.rs` geometry (line, rect, ellipse, brush,
mirror, flood) + `TilesetOp::{DeleteRow, DeleteColumn}`.
Why: pure, testable, and the panel stays a thin mapper.
1. pixel_tools + tests. 2. Delete ops + tests. Commit.

## Phase B — panel tools
What: Fill / Line / Rect / Ellipse tools, brush size, mirror toggles,
`write_points`, shape preview overlay.
3. `write_points`; pencil/eraser through brush + mirror. 4. Shape drags +
overlay preview + commit on release. 5. Fill. Toolbar + keys. Tests.
Commit.

## Phase C — selection move/flip, delete row/col
6. Move-drag inside the marquee; Flip H/V. 7. Edge "−" bars → delete
ops. Tests. Commit.

## Phase D — focus mode
8. `focus` state, 1-tile compose, mapping, arrows/escape, toolbar.
Tests. Commit.

## Phase E — wrap
9. Sweep tests + clippy; MIGRATION.md row; tick task 7; review agent;
fix; merge `pixel-tools` → `ggo`, push `ggo` + `main`; ggo → `main`,
push.
