//! GGO map painting, as a LIBRARY: everything needed to edit a `.map`,
//! minus a surface to edit it on.
//!
//! Levels are the fork's one authored art form -- tilesets come in from
//! external editors through `ggo_import_panel`, sprites are assembled from
//! those tiles in the sprite panel, and levels are PAINTED. Painting a
//! level places *tiles*, never pixels, so this stays inside F5's
//! no-pixel-painting decision (spec, "The art pipeline").
//!
//! **No editing logic lives here.** Every mutation is a
//! `ggo_worldlib::sprites::map_doc::MapOp` applied to a `MapDocStore`, and
//! the stamp/selection maths is that module's `palette_sel_rect` +
//! `build_stamp` + `pack_cell`/`unpack_cell`. All of it was extracted and
//! unit-tested in worldlib during F1 round 2.
//!
//! This crate used to also BE a panel: a docked map editor with its own
//! `.map` open interceptor, its own "New Map…" menu entry, its own canvas
//! and camera. That editor is gone (spec 2026-08-29, world-hosted map
//! editing) -- `ggo_world_panel` hosts the paint surface now, in world
//! space and alongside the entities the map sits under, so there is one
//! place a level is edited rather than two that had to be kept in step. A
//! `.map` is consequently not directly openable from the project panel any
//! more; it is reached through the world that references it (accepted
//! limitation, same spec).
//!
//! What is left is the four pieces a host mounts:
//!
//! * [`loader`] -- everything off the UI thread: the `.map` open, the bound
//!   tileset, both composes.
//! * [`geom`] -- the pure geometry (cell hit-testing under zoom/pan, resize
//!   clamping, the strip's tile indexing).
//! * [`paint_session`] -- the document, the tileset cache and the tool
//!   state machine. Touches no gpui state at all.
//! * [`paint_ui`] -- the shared gpui widgets (tool rail, stamp controls,
//!   strip picker, terrain editor, resize fields), generic over a
//!   [`paint_ui::PaintHost`].

mod geom;
pub mod loader;
pub mod paint_session;
pub mod paint_ui;

pub use paint_session::{MapTool, PaintSession};
