//! GGO MetaSprite panel (F2 tasks M4-M6): sprite picker, frame strip,
//! playback preview, animation EDITING -- clip CRUD, frame ops,
//! undo/redo/save over worldlib's `SpriteDocStore` -- and per-cell tile
//! assignment from a pool-tile palette, with the hardware budget line.
//! Structural mirror of `ggo_world_panel` -- `Panel` impl,
//! keybinding-reload observer, off-thread loading with a load-generation
//! guard, blur/Enter-committed single-line editors -- with the
//! sprite-specific pieces split out: `loader` owns everything off the UI
//! thread (`.spr` enumeration + open + per-frame/per-tile compose),
//! `playback` owns the pure range/loop/offset/fit math, `edits` owns the
//! pure edit rules (new-clip defaults, range validation, duration
//! parsing, post-op selection bookkeeping), `tiles` owns the preview
//! cell-hit math and the hw meter line; this module owns the panel
//! entity, the store wiring, the transport timer loop, and all gpui
//! glue. Op semantics mirror ggo-ide's `sprites/timeline.rs` message
//! handlers; guards are still re-checked here BEFORE apply -- the store
//! rejects out-of-range indices with a `DocError` these days (worldlib
//! DocOp hardening, ggo PR #73) rather than panicking, but a stale-index
//! click is a UI race to swallow silently, not an error to surface.

mod edits;
mod loader;
mod playback;
mod tiles;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, Bounds, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, KeyContext, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Render, RenderImage, Styled, Subscription, Task, WeakEntity, Window, actions, div, img, px,
};
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, DropdownMenu, ToggleState};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_worldlib::sprites::cow::{ClipEdit, SpriteState};
use ggo_worldlib::sprites::io::save_sprite;
use ggo_worldlib::sprites::sprite_doc::{DocOp, SpriteDocStore, clamp_clip_name_bytes};
use ggo_worldlib::sprites::timeline_ops::{playback_frame_at, playback_total_ms};

actions!(
    ggo_metasprite,
    [
        /// Toggles focus on the GGO metasprite panel.
        ToggleFocus,
        /// Toggles playback of the open sprite's active clip range.
        PlayPause,
        /// Undoes the last edit to the open sprite.
        Undo,
        /// Redoes the last undone edit to the open sprite.
        Redo,
        /// Saves the open sprite to its `.spr`/`.til`/`.pal` trio.
        Save,
        /// Commits the focused clip/duration field editor's text.
        CommitField,
        /// Deselects the active pool tile, so preview clicks stop
        /// assigning it to cells.
        DeselectTile
    ]
);

const GGO_METASPRITE_PANEL_KEY: &str = "GGOMetaSpritePanel";

/// The panel's key-dispatch context identifier. [`dispatch_context`]
/// additionally stamps `editing`/`not_editing` (project_panel's pattern)
/// so plain-key bindings (space) can be scoped away from focused text
/// editors -- see [`bind_panel_keys`].
///
/// [`dispatch_context`]: MetaSpritePanel::dispatch_context
const KEY_CONTEXT: &str = "GgoMetaSpritePanel";

/// Fixed default width until the panel grows real settings persistence.
const DEFAULT_WIDTH: Pixels = px(360.);

/// Frame-strip thumbnail box (px, square -- frames fit inside it via
/// `playback::fit_size`).
const THUMB_PX: f32 = 48.0;

/// Large center preview box (px, square).
const PREVIEW_PX: f32 = 240.0;

/// Tile-palette thumbnail box (px, square -- pool tiles are always
/// `TILE_PX` square, so no fit math needed).
const TILE_THUMB_PX: f32 = 24.0;

/// The tile-palette grid's max height before it scrolls.
const TILES_MAX_H: Pixels = px(92.);

/// The clip-CRUD side column's width.
const CLIPS_WIDTH: Pixels = px(148.);

/// Playback timer cadence. 16ms tracks a 60Hz frame; the ACTUAL frame
/// shown each tick is recomputed from wall-clock elapsed time
/// (`playback_frame_at`), so a late tick skips ahead rather than
/// slowing playback down.
const TICK: Duration = Duration::from_millis(16);

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as `ggo_world_panel::init`: `zed::reload_keymaps` clears
    // and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), and keymap assets are upstream files
    // this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` keeps the panel's bindings alive across
    // reloads.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| MetaSpritePanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<MetaSpritePanel>(window, cx);
        });
    })
    .detach();
}

fn bind_panel_keys(cx: &mut App) {
    // All scoped to the panel's key context. Space additionally requires
    // `not_editing` (the panel's dispatch context stamps `editing` while
    // any clip/duration field editor is focused, project_panel's
    // `not_editing` pattern) -- otherwise a panel-depth space binding
    // would win over a focused editor's plain-character input, since no
    // deeper binding shadows it. Ctrl-z/ctrl-s DON'T need the guard: the
    // default keymap binds them at the deeper `Editor` context, which
    // takes precedence while a field editor is focused. Enter is bound at
    // `KEY_CONTEXT > Editor` (single-line editors leave Enter unbound --
    // `editor::Newline` is `mode == full` only), committing the focused
    // field. `ToggleFocus` stays unbound, dispatched via
    // `Panel::toggle_action` / the command palette.
    cx.bind_keys([
        KeyBinding::new(
            "space",
            PlayPause,
            Some(&format!("{KEY_CONTEXT} && not_editing")),
        ),
        KeyBinding::new("ctrl-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-s", Save, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-s", Save, Some(KEY_CONTEXT)),
        KeyBinding::new(
            "enter",
            CommitField,
            Some(&format!("{KEY_CONTEXT} > Editor")),
        ),
        // Escape drops the active tile selection (the click-doesn't-
        // mutate affordance). No `not_editing` guard needed: while a
        // field editor is focused, the default keymap's deeper
        // `Editor`-context escape binding wins.
        KeyBinding::new("escape", DeselectTile, Some(KEY_CONTEXT)),
    ]);
}

// ------------------------------------------------------------- view state

/// What one panel text input edits. `Duration` deliberately carries no
/// frame index -- there is ONE duration editor, always bound to the
/// currently selected frame, so a selection change re-syncs its text
/// instead of rebuilding the editor set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditTarget {
    ClipName(usize),
    ClipFrom(usize),
    ClipTo(usize),
    Duration,
}

/// One panel text input: the target it edits and the single-line editor
/// entity backing it. Commit on blur (subscription) or Enter
/// ([`CommitField`]); the focus subscription re-renders the panel so the
/// key context's `editing` flag tracks reality.
struct EditorEntry {
    target: EditTarget,
    editor: Entity<Editor>,
    _subscriptions: [Subscription; 2],
}

/// An in-flight playback: wall-clock anchored (the shown frame is
/// recomputed from `started.elapsed()` every tick, never incremented), so
/// tick jitter can't drift the transport.
struct Playing {
    started: Instant,
    /// Elapsed-ms seed so playback starts ON the selected frame
    /// (`playback::start_offset_ms`).
    start_offset_ms: i64,
    /// The frame the transport is currently showing.
    frame: usize,
}

/// A loaded sprite: its doc store, per-frame image cache, transport
/// state, and field editors.
struct OpenSprite {
    rel_path: String,
    /// Sidecar rels resolved at open time -- `save_sprite` writes the
    /// same trio back.
    til_path: String,
    pal_path: String,
    store: SpriteDocStore,
    /// One composed BGRA image per frame index; rebuilt wholesale after
    /// every doc mutation (see `loader::LoadedSprite::frames`).
    frames: Vec<Arc<RenderImage>>,
    /// One composed BGRA image per pool tile index -- the tile palette's
    /// thumbnails; same invalidation as `frames`.
    pool_tiles: Vec<Arc<RenderImage>>,
    /// The active pool tile: while `Some`, a preview-cell click assigns
    /// it (`FrameTileSet`); `None` means clicks don't mutate. Cleared by
    /// re-clicking the tile, Escape, or the tile vanishing under an
    /// undo/fold-back.
    selected_tile: Option<u16>,
    /// The preview image's on-screen bounds, recorded at prepaint by the
    /// overlay canvas so the click handler can map window coords to cell
    /// hits (world_panel's `last_bounds` idiom). `None` until the first
    /// Ready-state paint.
    preview_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    selected_frame: usize,
    /// Index into the doc's clips; `None` = whole-sprite range.
    active_clip: Option<usize>,
    playing: Option<Playing>,
    /// The transport's timer loop -- dropping it (new sprite selected,
    /// panel dropped) cancels playback; a finished loop leaves a spent
    /// task behind, which the next play simply replaces.
    _tick_task: Option<Task<()>>,
    /// Clip/duration field editors, rebuilt by `ensure_editors` when the
    /// target set changes.
    editors: Vec<EditorEntry>,
    /// Inline rejection from a clip-range edit: `(clip index, message)`.
    /// Cleared on the next applied op.
    clip_error: Option<(usize, String)>,
    /// A store-level op rejection (shouldn't happen -- ops are
    /// bounds-guarded before apply -- but surfaced instead of swallowed).
    op_error: Option<String>,
    save_error: Option<String>,
}

impl OpenSprite {
    fn new(rel_path: String, loaded: loader::LoadedSprite) -> Self {
        OpenSprite {
            rel_path,
            til_path: loaded.til_path,
            pal_path: loaded.pal_path,
            store: SpriteDocStore::new(loaded.state),
            frames: loaded.frames,
            pool_tiles: loaded.pool_tiles,
            selected_tile: None,
            preview_bounds: Rc::new(RefCell::new(None)),
            selected_frame: 0,
            active_clip: None,
            playing: None,
            _tick_task: None,
            editors: Vec::new(),
            clip_error: None,
            op_error: None,
            save_error: None,
        }
    }

    /// The frame the big preview shows: the transport's while playing,
    /// the selected frame otherwise (so stopping "resets" the preview to
    /// the selection).
    fn shown_frame(&self) -> usize {
        self.playing
            .as_ref()
            .map_or(self.selected_frame, |p| p.frame)
    }

    fn durations(&self) -> Vec<u16> {
        self.store
            .state()
            .frames
            .iter()
            .map(|f| f.duration_ms)
            .collect()
    }
}

enum ViewerState {
    /// Nothing selected yet.
    Empty,
    Loading {
        rel_path: String,
    },
    Ready(Box<OpenSprite>),
    Error(String),
}

pub struct MetaSpritePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// Sorted `.spr` rel paths under the project root -- the picker feed.
    sprites: Vec<String>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl MetaSpritePanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            sprites: Vec::new(),
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree) and re-enumerate its sprites. Runs on every panel
    /// activation -- same discovery as `ggo_world_panel::refresh_worlds`.
    fn refresh_sprites(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        self.sprites = match &self.project_root {
            Some(root) => loader::list_sprites(root),
            None => Vec::new(),
        };
        cx.notify();
    }

    /// Kick off the off-thread load of `self.sprites[ix]`. A stale result
    /// (superseded by a later selection) is dropped by generation check.
    fn select_sprite(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(rel) = self.sprites.get(ix).cloned() else {
            return;
        };
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        self.state = ViewerState::Loading {
            rel_path: rel.clone(),
        };
        cx.notify();

        let load = {
            let rel = rel.clone();
            cx.background_spawn(async move { loader::load_sprite(&root, &rel) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => ViewerState::Ready(Box::new(OpenSprite::new(rel, loaded))),
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Click a strip thumbnail: select that frame (the transport, if
    /// running, keeps playing -- its own frame wins in the preview until
    /// it stops).
    fn select_frame(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && ix < open.store.state().frames.len()
        {
            open.selected_frame = ix;
            cx.notify();
        }
    }

    /// Pick the active clip (`None` = whole sprite). A running transport
    /// picks the new range up on its next tick (range/loop are re-read
    /// per tick, ggo-ide's mid-playback-edit rule).
    fn select_clip(&mut self, clip: Option<usize>, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state {
            open.active_clip = clip;
            cx.notify();
        }
    }

    /// Play/pause toggle. Play anchors a wall clock, seeds the elapsed
    /// offset so playback starts on the selected frame (clamped into the
    /// active range), and spawns the tick loop; pause drops the loop and
    /// the transport state (the preview falls back to the selected
    /// frame).
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if open.playing.is_some() {
            open.playing = None;
            open._tick_task = None;
            cx.notify();
            return;
        }
        if open.store.state().frames.is_empty() {
            return;
        }
        let durations = open.durations();
        let state = open.store.state();
        let range = playback::play_range(&state.clips, open.active_clip, state.frames.len());
        let start_offset_ms = playback::start_offset_ms(&durations, range, open.selected_frame);
        open.playing = Some(Playing {
            started: Instant::now(),
            start_offset_ms,
            frame: open
                .selected_frame
                .clamp(range.0.min(range.1), range.0.max(range.1)),
        });
        // The timer loop: sleep a tick, recompute the shown frame from
        // wall-clock elapsed, stop when a non-looping range finishes (or
        // the panel/sprite goes away). `cx.spawn` + a
        // `background_executor().timer` await is this checkout's idiom
        // for periodic UI work (gpui has no retained per-entity
        // animation-frame callback; element-level `with_animation` is a
        // fixed-duration easing wrapper, wrong shape for a
        // duration-table-driven transport).
        open._tick_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let finished = this.update(cx, |this, cx| this.advance_playback(cx));
                match finished {
                    Ok(false) => {}
                    Ok(true) | Err(_) => break,
                }
            }
        }));
        cx.notify();
    }

    /// One transport tick: re-read durations/range/loop from the doc
    /// (mid-playback edits get picked up immediately, ggo-ide's `tick`
    /// rule -- a deleted clip falls back to the whole strip via
    /// `play_range`'s stale-index rule) and recompute the shown frame.
    /// Notifies only when the shown frame actually changed (or playback
    /// stopped) -- at 16ms ticks over >= 16ms frames most ticks change
    /// nothing, and a no-op notify would re-render the whole panel at
    /// 60Hz for the duration of playback (M4 review fix). Returns true
    /// when the loop should stop: playback was cancelled, or a
    /// non-looping range ran past its total (which also resets the
    /// preview to the selected frame).
    fn advance_playback(&mut self, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
            return true;
        };
        if open.playing.is_none() {
            return true;
        }
        let state = open.store.state();
        let durations: Vec<u16> = state.frames.iter().map(|f| f.duration_ms).collect();
        let range = playback::play_range(&state.clips, open.active_clip, state.frames.len());
        let loop_ = playback::play_loop(&state.clips, open.active_clip);
        let playing = open
            .playing
            .as_mut()
            .expect("checked playing.is_some() above");
        let t_ms = playing.start_offset_ms + playing.started.elapsed().as_millis() as i64;
        if !loop_ && t_ms >= playback_total_ms(&durations, range) {
            open.playing = None;
            cx.notify();
            return true;
        }
        let frame = playback_frame_at(&durations, range, t_ms, loop_);
        if playing.frame != frame {
            playing.frame = frame;
            cx.notify();
        }
        false
    }

    // ------------------------------------------------------------ doc ops

    /// Apply one op to the store, then refresh everything derived from the
    /// doc. All callers bounds-check their indices FIRST: the store
    /// validates every index itself now (worldlib DocOp hardening, ggo
    /// PR #73 -- a bad index comes back as a `DocError`, not a panic),
    /// but a stale index from a click racing an undo is a UI no-op, not
    /// an error the user should see. A genuine store-level rejection
    /// (e.g. an over-long clip name, pre-clamped here so it shouldn't
    /// fire) is surfaced inline rather than swallowed.
    fn apply_doc(&mut self, op: DocOp, cx: &mut Context<Self>) -> bool {
        let ViewerState::Ready(open) = &mut self.state else {
            return false;
        };
        match open.store.apply(op) {
            Ok(()) => {
                self.refresh_after_doc_change(cx);
                true
            }
            Err(e) => {
                open.op_error = Some(e.to_string());
                cx.notify();
                false
            }
        }
    }

    /// After any doc mutation (op, undo, redo, save fold-back): rebuild
    /// the composed per-frame images (the whole Vec -- M4's documented
    /// invalidation point; wholesale recompose keeps every thumbnail *and*
    /// the frame-index -> image mapping trivially correct after
    /// adds/deletes/moves, at O(frames x sprite px) per edit, cheap at
    /// sprite scale), clamp the frame/clip selections into the new
    /// bounds, and clear stale inline errors. A running transport needs no
    /// help: range/loop/durations are re-read from the doc every tick.
    fn refresh_after_doc_change(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let frames = loader::compose_frames(open.store.state()).unwrap_or_default();
        let pool_tiles = loader::compose_pool_tiles(open.store.state()).unwrap_or_default();
        let frame_count = open.store.state().frames.len();
        let clip_count = open.store.state().clips.len();
        let tile_count = open.store.state().tile_count;
        open.frames = frames;
        open.pool_tiles = pool_tiles;
        open.selected_frame = open.selected_frame.min(frame_count.saturating_sub(1));
        if open.active_clip.is_some_and(|c| c >= clip_count) {
            open.active_clip = None;
        }
        if open.selected_tile.is_some_and(|t| t as usize >= tile_count) {
            // The tile went away (undo past a COW clone, dedup fold-back
            // on save) -- drop the selection rather than repointing cells
            // at whatever tile inherits the index.
            open.selected_tile = None;
        }
        open.clip_error = None;
        open.op_error = None;
        cx.notify();
    }

    fn undo_impl(&mut self, cx: &mut Context<Self>) {
        let undone = match &mut self.state {
            ViewerState::Ready(open) => open.store.undo(),
            _ => false,
        };
        if undone {
            self.refresh_after_doc_change(cx);
        }
    }

    fn redo_impl(&mut self, cx: &mut Context<Self>) {
        let redone = match &mut self.state {
            ViewerState::Ready(open) => open.store.redo(),
            _ => false,
        };
        if redone {
            self.refresh_after_doc_change(cx);
        }
    }

    /// `save_sprite` -> `set_saved_state` with its fold-back result --
    /// worldlib's own save flow (ggo-ide `editor.rs` parity: the folded
    /// state silently replaces `current` WITHOUT an undo entry, so Ctrl+Z
    /// after saving steps through the user's edits, not the fold-back).
    /// Synchronous by choice, same reasoning as `ggo_world_panel`'s save.
    fn save_impl(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.project_root.clone() else {
            return;
        };
        let saved = {
            let ViewerState::Ready(open) = &mut self.state else {
                return;
            };
            match save_sprite(
                &root,
                &open.rel_path,
                open.store.state(),
                &open.til_path,
                &open.pal_path,
            ) {
                Ok(folded) => {
                    open.store.set_saved_state(folded);
                    open.save_error = None;
                    true
                }
                Err(e) => {
                    open.save_error = Some(e.to_string());
                    false
                }
            }
        };
        if saved {
            // Fold-back may have re-identified pool tiles; recompose from
            // the state the store now actually holds.
            self.refresh_after_doc_change(cx);
        } else {
            cx.notify();
        }
    }

    // --------------------------------------------------------- frame ops

    /// Append a blank frame -- ggo-ide `Msg::AddFrame` (insert at the end,
    /// no `copy_of`; the store finds or appends an all-zero tile).
    fn add_blank_frame(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let at = open.store.state().frames.len();
        self.apply_doc(
            DocOp::FrameAdd {
                at,
                copy_of: None,
                map: None,
            },
            cx,
        );
    }

    /// Duplicate the selected frame right after itself and select the
    /// copy -- ggo-ide `Msg::DupFrame`.
    fn duplicate_selected_frame(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let i = open.selected_frame;
        if i >= open.store.state().frames.len() {
            return;
        }
        if self.apply_doc(
            DocOp::FrameAdd {
                at: i + 1,
                copy_of: Some(i),
                map: None,
            },
            cx,
        ) && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = i + 1;
        }
    }

    /// Delete the selected frame -- ggo-ide `Msg::DeleteFrame`: refuses to
    /// drop the last frame; the store adjusts clip ranges in the same op;
    /// the selection follows `edits::selection_after_frame_delete`.
    fn delete_selected_frame(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let i = open.selected_frame;
        let len = open.store.state().frames.len();
        if len <= 1 || i >= len {
            return; // always keep at least one frame
        }
        let next = edits::selection_after_frame_delete(i, i, len);
        if self.apply_doc(DocOp::FrameDelete { at: i }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = next;
        }
    }

    /// Move the selected frame one slot left/right -- ggo-ide
    /// `Msg::MoveFrame`: for an ADJACENT move, `to` is already the correct
    /// post-removal splice index (a neighbor swap), so no drop-target
    /// conversion is needed; the selection follows the moved frame.
    fn move_selected_frame(&mut self, delta: i32, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let i = open.selected_frame;
        let len = open.store.state().frames.len();
        let to = if delta < 0 {
            i.checked_sub(1)
        } else {
            i.checked_add(1)
        };
        let Some(to) = to else {
            return;
        };
        if i >= len || to >= len {
            return;
        }
        if self.apply_doc(DocOp::FrameMove { from: i, to }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.selected_frame = to;
        }
    }

    // ----------------------------------------------------------- tile ops

    /// Click a tile-palette thumbnail: select it as the active tile, or
    /// deselect on a re-click of the already-active tile (the brief's
    /// "clicks don't always mutate" affordance, alongside Escape).
    fn select_tile(&mut self, tile: u16, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        if (tile as usize) >= open.store.state().tile_count {
            return; // stale click racing an undo
        }
        open.selected_tile = if open.selected_tile == Some(tile) {
            None
        } else {
            Some(tile)
        };
        cx.notify();
    }

    fn deselect_tile(&mut self, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &mut self.state
            && open.selected_tile.is_some()
        {
            open.selected_tile = None;
            cx.notify();
        }
    }

    /// A preview click DURING playback pauses the transport and adopts
    /// its shown frame as the selection FIRST, so the click edits the
    /// frame the user actually saw -- not the stale pre-playback
    /// selection (M6 review fix-forward). A no-op when not playing.
    fn pause_playback_on_edit(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let Some(playing) = open.playing.take() else {
            return;
        };
        open._tick_task = None;
        open.selected_frame = playing.frame;
        cx.notify();
    }

    /// The preview's left-click body (the mouse listener adds only the
    /// focus grab): pause-and-sync if playing, THEN map the click to a
    /// cell edit -- reviewer-specified order, M6 fix-forward.
    fn on_preview_click(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        self.pause_playback_on_edit(cx);
        if let Some(cell) = self.preview_cell_at(position) {
            self.set_tile_on_cell(cell, cx);
        }
    }

    /// Click a cell of the selected-frame preview with a tile active:
    /// repoint that cell at the tile (`DocOp::FrameTileSet`, M1's op)
    /// through the same `apply_doc` path as every other edit -- undo,
    /// thumbnail recompose, and error surfacing come with it. Targets the
    /// SELECTED frame -- which, on a click during playback, was just
    /// synced to the transport's shown frame by
    /// [`Self::pause_playback_on_edit`]. No-tile-selected and
    /// unchanged-cell clicks are dropped without an op (M5's undo-stack
    /// hygiene rule).
    fn set_tile_on_cell(&mut self, cell: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(tile) = open.selected_tile else {
            return;
        };
        let frame = open.selected_frame;
        let state = open.store.state();
        let Some(f) = state.frames.get(frame) else {
            return;
        };
        if cell >= f.map.len() || (tile as usize) >= state.tile_count {
            return; // stale geometry/selection racing an undo
        }
        if f.map[cell] == tile {
            return; // already that tile -- don't push a no-op undo entry
        }
        self.apply_doc(DocOp::FrameTileSet { frame, cell, tile }, cx);
    }

    /// The preview click handler's window->cell mapping: local coords
    /// against the recorded preview bounds, then `tiles::cell_at` over
    /// the selected frame's tile grid.
    fn preview_cell_at(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let ViewerState::Ready(open) = &self.state else {
            return None;
        };
        let bounds = (*open.preview_bounds.borrow())?;
        let state = open.store.state();
        tiles::cell_at(
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            state.w_tiles as usize,
            state.h_tiles as usize,
        )
    }

    // ---------------------------------------------------------- clip ops

    /// Add a clip with ggo-ide `Msg::AddClip`'s defaults (see
    /// `edits::default_new_clip`).
    fn add_clip(&mut self, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let state = open.store.state();
        let clip =
            edits::default_new_clip(state.clips.len(), open.selected_frame, state.frames.len());
        self.apply_doc(DocOp::ClipAdd { clip }, cx);
    }

    /// Delete clip `i` -- ggo-ide `Msg::DeleteClip`, including its
    /// active-selection shift rule. Bounds-checked: a stale index (clip
    /// removed by an undo between render and click) should vanish as a
    /// no-op, not surface as the store's `ClipOutOfRange` error.
    fn delete_clip(&mut self, i: usize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        if i >= open.store.state().clips.len() {
            return;
        }
        let next_active = edits::active_clip_after_clip_delete(open.active_clip, i);
        if self.apply_doc(DocOp::ClipSet { at: i, clip: None }, cx)
            && let ViewerState::Ready(open) = &mut self.state
        {
            open.active_clip = next_active;
        }
    }

    /// Toggle clip `i`'s loop flag -- ggo-ide `Msg::ClipLoop` (whole-clip
    /// `ClipSet` with just the flag changed).
    fn set_clip_loop(&mut self, i: usize, loop_: bool, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some(clip) = open.store.state().clips.get(i).cloned() else {
            return;
        };
        if clip.loop_ == loop_ {
            return;
        }
        self.apply_doc(
            DocOp::ClipSet {
                at: i,
                clip: Some(ClipEdit { loop_, ..clip }),
            },
            cx,
        );
    }

    // ------------------------------------------------------ field editors

    /// A completed edit's buffer -> the op, mirroring ggo-ide's guard
    /// order per field:
    ///
    /// - clip name (`Msg::ClipNameSubmit`): trim, byte-clamp
    ///   (`clamp_clip_name_bytes`), empty -> dropped; then `ClipSet`.
    /// - clip from/to: parse `usize` (unparsable -> dropped, world_panel's
    ///   commit rule); validate the CANDIDATE range via
    ///   `edits::clip_range_error` BEFORE building the op -- a failure is
    ///   shown inline on that clip's row and nothing is applied. (ggo-ide
    ///   edits ranges with clamped sliders, so its equivalent guard is the
    ///   clamp itself; typed input needs the explicit check.)
    /// - duration (`Msg::DurationSubmit`): parse-or-0 then floor to
    ///   `MIN_FRAME_MS` (`edits::parse_duration_ms`), applied to the
    ///   selected frame.
    ///
    /// Unchanged values are dropped without an op: blur commits every
    /// focus exit, and a no-change `ClipSet`/`FrameDuration` would still
    /// push an undo entry.
    fn commit_edit(&mut self, target: EditTarget, text: String, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        match target {
            EditTarget::ClipName(i) => {
                let Some(clip) = open.store.state().clips.get(i).cloned() else {
                    return;
                };
                let name = clamp_clip_name_bytes(text.trim());
                if name.is_empty() || name == clip.name {
                    cx.notify(); // re-sync the editor's text from the doc
                    return;
                }
                self.apply_doc(
                    DocOp::ClipSet {
                        at: i,
                        clip: Some(ClipEdit { name, ..clip }),
                    },
                    cx,
                );
            }
            EditTarget::ClipFrom(i) | EditTarget::ClipTo(i) => {
                let Some(clip) = open.store.state().clips.get(i).cloned() else {
                    return;
                };
                let Ok(value) = text.trim().parse::<usize>() else {
                    cx.notify(); // dropped, not committed -- editor re-syncs
                    return;
                };
                let (from, to) = match target {
                    EditTarget::ClipFrom(_) => (value, clip.to),
                    _ => (clip.from, value),
                };
                if (from, to) == (clip.from, clip.to) {
                    cx.notify();
                    return;
                }
                let frame_count = open.store.state().frames.len();
                if let Some(err) = edits::clip_range_error(from, to, frame_count) {
                    open.clip_error = Some((i, err));
                    cx.notify();
                    return;
                }
                open.clip_error = None;
                self.apply_doc(
                    DocOp::ClipSet {
                        at: i,
                        clip: Some(ClipEdit { from, to, ..clip }),
                    },
                    cx,
                );
            }
            EditTarget::Duration => {
                let ms = edits::parse_duration_ms(&text);
                let at = open.selected_frame;
                let Some(frame) = open.store.state().frames.get(at) else {
                    return;
                };
                if frame.duration_ms == ms {
                    cx.notify();
                    return;
                }
                self.apply_doc(DocOp::FrameDuration { at, ms }, cx);
            }
        }
    }

    /// The display string an unfocused input shows for `target`.
    fn edit_display_text(
        target: &EditTarget,
        state: &SpriteState,
        selected_frame: usize,
    ) -> String {
        match target {
            EditTarget::ClipName(i) => state
                .clips
                .get(*i)
                .map_or_else(String::new, |c| c.name.clone()),
            EditTarget::ClipFrom(i) => state
                .clips
                .get(*i)
                .map_or_else(String::new, |c| c.from.to_string()),
            EditTarget::ClipTo(i) => state
                .clips
                .get(*i)
                .map_or_else(String::new, |c| c.to.to_string()),
            EditTarget::Duration => state
                .frames
                .get(selected_frame)
                .map_or_else(String::new, |f| f.duration_ms.to_string()),
        }
    }

    /// Keep the field-editor entities in sync with the doc: rebuild when
    /// the target set changed (clip added/deleted, undo/redo
    /// restructure), otherwise refresh unfocused editors' text from the
    /// doc -- a focused editor keeps the user's in-progress buffer
    /// (`ggo_world_panel::ensure_inspector`'s rule, itself ggo-ide's
    /// draft pattern).
    fn ensure_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let mut targets = Vec::with_capacity(open.store.state().clips.len() * 3 + 1);
        for i in 0..open.store.state().clips.len() {
            targets.push(EditTarget::ClipName(i));
            targets.push(EditTarget::ClipFrom(i));
            targets.push(EditTarget::ClipTo(i));
        }
        targets.push(EditTarget::Duration);

        let same_targets = open.editors.len() == targets.len()
            && open
                .editors
                .iter()
                .zip(&targets)
                .all(|(entry, target)| entry.target == *target);

        if same_targets {
            let state = open.store.state();
            for entry in &open.editors {
                if entry.editor.focus_handle(cx).is_focused(window) {
                    continue;
                }
                let text = Self::edit_display_text(&entry.target, state, open.selected_frame);
                if entry.editor.read(cx).text(cx) != text {
                    entry
                        .editor
                        .update(cx, |editor, cx| editor.set_text(text, window, cx));
                }
            }
            return;
        }

        let mut entries = Vec::with_capacity(targets.len());
        for target in targets {
            let text = Self::edit_display_text(&target, open.store.state(), open.selected_frame);
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(text, window, cx);
                editor
            });
            let commit = cx.subscribe(&editor, Self::handle_editor_event);
            // Focus entering a field flips the key context's flag to
            // `editing` (space types instead of toggling playback) -- but
            // only a re-render re-reads `dispatch_context`, so nudge one.
            let focus = cx.on_focus(&editor.focus_handle(cx), window, |_, _, cx| cx.notify());
            entries.push(EditorEntry {
                target,
                editor,
                _subscriptions: [commit, focus],
            });
        }
        open.editors = entries;
    }

    /// Blur commits the field (world_panel's rule; ggo-ide commits on
    /// Enter only because iced has no blur event -- see its
    /// `timeline.rs` module doc). The notify re-evaluates the key
    /// context's `editing` flag either way.
    fn handle_editor_event(
        &mut self,
        editor: Entity<Editor>,
        event: &EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, EditorEvent::Blurred) {
            self.commit_editor(editor.entity_id(), cx);
            cx.notify();
        }
    }

    fn commit_editor(&mut self, editor_id: EntityId, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let Some((target, text)) = open
            .editors
            .iter()
            .find(|e| e.editor.entity_id() == editor_id)
            .map(|e| (e.target.clone(), e.editor.read(cx).text(cx)))
        else {
            return;
        };
        self.commit_edit(target, text, cx);
    }

    fn on_commit_field(&mut self, _: &CommitField, window: &mut Window, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &self.state else {
            return;
        };
        let focused = open
            .editors
            .iter()
            .find(|e| e.editor.focus_handle(cx).is_focused(window))
            .map(|e| e.editor.entity_id());
        if let Some(id) = focused {
            self.commit_editor(id, cx);
        }
    }

    /// The panel's key context: the panel identifier plus an
    /// `editing`/`not_editing` flag from live editor focus
    /// (project_panel's `dispatch_context` pattern) -- see
    /// [`bind_panel_keys`] for why space needs it.
    fn dispatch_context(&self, window: &Window, cx: &Context<Self>) -> KeyContext {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add(KEY_CONTEXT);
        let editing = match &self.state {
            ViewerState::Ready(open) => open
                .editors
                .iter()
                .any(|e| e.editor.focus_handle(cx).is_focused(window)),
            _ => false,
        };
        key_context.add(if editing { "editing" } else { "not_editing" });
        key_context
    }

    // ------------------------------------------------------------- render

    fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_rel = match &self.state {
            ViewerState::Ready(open) => Some(open.rel_path.clone()),
            ViewerState::Loading { rel_path } => Some(rel_path.clone()),
            _ => None,
        };
        h_flex()
            .flex_wrap()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when(self.sprites.is_empty(), |this| {
                let message = if self.project_root.is_some() {
                    "No sprites found"
                } else {
                    "No project open"
                };
                this.child(Label::new(message).color(Color::Muted))
            })
            .children(self.sprites.iter().enumerate().map(|(ix, rel)| {
                let selected = selected_rel.as_deref() == Some(rel.as_str());
                Button::new(("ggo-metasprite", ix), SharedString::from(rel.clone()))
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_sprite(ix, cx)))
            }))
    }

    fn render_message(&self, message: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }

    /// Transport row: play/pause, the clip selector, undo/redo/save, and
    /// the sprite name with world_panel's dirty dot.
    fn render_transport(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_transport is only called in the Ready state");
        };
        let playing = open.playing.is_some();
        let dirty = open.store.dirty();
        let state = open.store.state();
        let clip_label: SharedString = match open.active_clip.and_then(|i| state.clips.get(i)) {
            Some(c) => c.name.clone().into(),
            None => "All frames".into(),
        };
        let clip_names: Vec<String> = state.clips.iter().map(|c| c.name.clone()).collect();
        let title = format!("{}{}", open.rel_path, if dirty { " ●" } else { "" });
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            {
                let weak = weak.clone();
                menu = menu.entry("All frames", None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_clip(None, cx)).ok();
                });
            }
            for (ix, name) in clip_names.into_iter().enumerate() {
                let weak = weak.clone();
                menu = menu.entry(SharedString::from(name), None, move |_window, cx| {
                    weak.update(cx, |this, cx| this.select_clip(Some(ix), cx))
                        .ok();
                });
            }
            menu
        });
        h_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Button::new(
                    "ggo-metasprite-play",
                    if playing { "Pause" } else { "Play" },
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx))),
            )
            .child(DropdownMenu::new("ggo-metasprite-clip", clip_label, menu))
            .child(div().flex_1())
            .child(
                Label::new(SharedString::from(title))
                    .size(LabelSize::Small)
                    .color(if dirty { Color::Modified } else { Color::Muted }),
            )
            .child(
                IconButton::new("ggo-metasprite-undo", IconName::Undo)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Undo"))
                    .on_click(cx.listener(|this, _, _, cx| this.undo_impl(cx))),
            )
            .child(
                IconButton::new("ggo-metasprite-redo", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Redo"))
                    .on_click(cx.listener(|this, _, _, cx| this.redo_impl(cx))),
            )
            .child(
                Button::new("ggo-metasprite-save", "Save")
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, _, cx| this.save_impl(cx))),
            )
            .children(open.save_error.as_ref().map(|e| {
                Label::new(format!("save failed: {e}"))
                    .size(LabelSize::Small)
                    .color(Color::Error)
            }))
            .into_any_element()
    }

    /// The big center preview: the shown frame fit into a [`PREVIEW_PX`]
    /// box. An invisible overlay canvas records the image's on-screen
    /// bounds at prepaint (world_panel's `last_bounds` idiom -- gpui
    /// mouse listeners only get window coords), and a left click goes
    /// through [`Self::on_preview_click`]: pause-and-sync any running
    /// transport, then (with an active tile) map through
    /// `tiles::cell_at` to a `FrameTileSet` on the selected frame.
    fn render_preview(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_preview is only called in the Ready state");
        };
        let shown = open.shown_frame();
        let mut preview = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(cx.theme().colors().editor_background);
        if let Some(image) = open.frames.get(shown) {
            let (w, h) = image_px_size(image);
            let (fit_w, fit_h) = playback::fit_size(w, h, PREVIEW_PX);
            let bounds_cell = open.preview_bounds.clone();
            let overlay = gpui::canvas(
                move |bounds, _window, _cx| {
                    *bounds_cell.borrow_mut() = Some(bounds);
                },
                |_, (), _, _| {},
            )
            .absolute()
            .size_full();
            preview = preview.child(
                div()
                    .relative()
                    .w(px(fit_w))
                    .h(px(fit_h))
                    .child(img(image.clone()).w(px(fit_w)).h(px(fit_h)))
                    .child(overlay)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            // Take focus so Escape/undo bindings apply
                            // (and any in-flight field edit blur-commits).
                            window.focus(&this.focus_handle, cx);
                            this.on_preview_click(event.position, cx);
                        }),
                    ),
            );
        }
        preview.into_any_element()
    }

    /// The tile-palette section: every pool tile as a thumbnail, wrap
    /// grid, click to select (re-click to deselect), the active tile
    /// outlined; the hw budget line rides along on the header row.
    fn render_tiles(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_tiles is only called in the Ready state");
        };
        let state = open.store.state();
        let border = cx.theme().colors().border;
        let accent = cx.theme().colors().border_focused;
        let meter = tiles::hw_meter_line(state, state.frames.get(open.selected_frame));
        v_flex()
            .flex_none()
            .border_t_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap_1()
                    .px_1()
                    .pt_1()
                    .child(
                        Label::new("Tiles")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(SharedString::from(meter))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .id("ggo-metasprite-tiles")
                    .max_h(TILES_MAX_H)
                    .overflow_y_scroll()
                    .child(h_flex().flex_wrap().gap_1().p_1().children(
                        open.pool_tiles.iter().enumerate().map(|(ix, image)| {
                            let selected = open.selected_tile == Some(ix as u16);
                            div()
                                .id(("ggo-metasprite-tile", ix))
                                .p_0p5()
                                .border_1()
                                .rounded_sm()
                                .border_color(if selected { accent } else { border })
                                .child(img(image.clone()).w(px(TILE_THUMB_PX)).h(px(TILE_THUMB_PX)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_tile(ix as u16, cx)
                                }))
                        }),
                    )),
            )
            .into_any_element()
    }

    /// One field text input, in world_panel's minimal bordered box.
    fn editor_input(editor: Entity<Editor>, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .flex_1()
            .min_w_0()
            .px_1()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .bg(cx.theme().colors().editor_background)
            .child(editor)
            .into_any_element()
    }

    /// The clip-CRUD side column: per clip a name row (+ delete) and a
    /// from/to/loop row, an inline range error under the offending clip,
    /// and an add button.
    fn render_clips(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_clips is only called in the Ready state");
        };
        let state = open.store.state();
        let editor_for = |target: EditTarget| {
            open.editors
                .iter()
                .find(|e| e.target == target)
                .map(|e| e.editor.clone())
        };
        let mut col = v_flex().p_1().gap_1();
        for (i, clip) in state.clips.iter().enumerate() {
            let mut row = v_flex()
                .gap_0p5()
                .p_0p5()
                .border_1()
                .rounded_sm()
                .border_color(if open.active_clip == Some(i) {
                    cx.theme().colors().border_focused
                } else {
                    cx.theme().colors().border_variant
                })
                .child(
                    h_flex()
                        .gap_0p5()
                        .children(
                            editor_for(EditTarget::ClipName(i)).map(|e| Self::editor_input(e, cx)),
                        )
                        .child(
                            IconButton::new(("ggo-metasprite-clip-delete", i), IconName::Trash)
                                .icon_size(IconSize::Small)
                                .tooltip(ui::Tooltip::text("Delete clip"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.delete_clip(i, cx)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap_0p5()
                        .children(
                            editor_for(EditTarget::ClipFrom(i)).map(|e| Self::editor_input(e, cx)),
                        )
                        .children(
                            editor_for(EditTarget::ClipTo(i)).map(|e| Self::editor_input(e, cx)),
                        )
                        .child({
                            let weak = cx.weak_entity();
                            Checkbox::new(
                                ("ggo-metasprite-clip-loop", i),
                                ToggleState::from(clip.loop_),
                            )
                            .label("loop")
                            .on_click(move |toggle, _window, cx| {
                                let on = matches!(toggle, ToggleState::Selected);
                                weak.update(cx, |this, cx| this.set_clip_loop(i, on, cx))
                                    .ok();
                            })
                        }),
                );
            if let Some((at, message)) = &open.clip_error
                && *at == i
            {
                row = row.child(
                    Label::new(SharedString::from(message.clone()))
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                );
            }
            col = col.child(row);
        }
        col = col.child(
            Button::new("ggo-metasprite-clip-add", "+ Clip")
                .on_click(cx.listener(|this, _, _, cx| this.add_clip(cx))),
        );
        div()
            .id("ggo-metasprite-clips")
            .w(CLIPS_WIDTH)
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .overflow_y_scroll()
            .child(col)
            .into_any_element()
    }

    /// Frame-op row above the strip: add/duplicate/delete/move buttons
    /// acting on the selected frame, plus its duration editor.
    fn render_frame_ops(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_frame_ops is only called in the Ready state");
        };
        let len = open.store.state().frames.len();
        let selected = open.selected_frame;
        h_flex()
            .gap_1()
            .p_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("ggo-metasprite-frame-add", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Add blank frame"))
                    .on_click(cx.listener(|this, _, _, cx| this.add_blank_frame(cx))),
            )
            .child(
                IconButton::new("ggo-metasprite-frame-dup", IconName::Copy)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Duplicate frame"))
                    .on_click(cx.listener(|this, _, _, cx| this.duplicate_selected_frame(cx))),
            )
            .child(
                IconButton::new("ggo-metasprite-frame-delete", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Delete frame"))
                    .disabled(len <= 1)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected_frame(cx))),
            )
            .child(
                IconButton::new("ggo-metasprite-frame-left", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Move frame left"))
                    .disabled(selected == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.move_selected_frame(-1, cx))),
            )
            .child(
                IconButton::new("ggo-metasprite-frame-right", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text("Move frame right"))
                    .disabled(selected + 1 >= len)
                    .on_click(cx.listener(|this, _, _, cx| this.move_selected_frame(1, cx))),
            )
            .child(div().flex_1())
            .child(Label::new("ms").size(LabelSize::Small).color(Color::Muted))
            .child(
                div().w(px(56.)).flex_none().children(
                    open.editors
                        .iter()
                        .find(|e| e.target == EditTarget::Duration)
                        .map(|e| Self::editor_input(e.editor.clone(), cx)),
                ),
            )
            .children(open.op_error.as_ref().map(|e| {
                Label::new(SharedString::from(e.clone()))
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
            }))
            .into_any_element()
    }

    /// The bottom frame strip: one thumbnail + duration label per frame,
    /// click to select, selected frame outlined.
    fn render_strip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_strip is only called in the Ready state");
        };
        let selected = open.selected_frame;
        let border = cx.theme().colors().border;
        let accent = cx.theme().colors().border_focused;
        let state = open.store.state();
        div()
            .id("ggo-metasprite-strip")
            .flex_none()
            .p_1()
            .border_t_1()
            .border_color(border)
            .overflow_x_scroll()
            .child(
                h_flex()
                    .gap_1()
                    .children(state.frames.iter().enumerate().map(|(ix, frame)| {
                        let thumb = open.frames.get(ix).map(|image| {
                            let (w, h) = image_px_size(image);
                            let (fit_w, fit_h) = playback::fit_size(w, h, THUMB_PX);
                            img(image.clone()).w(px(fit_w)).h(px(fit_h))
                        });
                        v_flex()
                            .id(("ggo-metasprite-frame", ix))
                            .items_center()
                            .gap_0p5()
                            .p_0p5()
                            .border_1()
                            .rounded_sm()
                            .border_color(if ix == selected { accent } else { border })
                            .child(
                                div()
                                    .w(px(THUMB_PX))
                                    .h(px(THUMB_PX))
                                    .flex()
                                    .justify_center()
                                    .items_center()
                                    .children(thumb),
                            )
                            .child(
                                Label::new(format!("{} ms", frame.duration_ms))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.select_frame(ix, cx)))
                    })),
            )
            .into_any_element()
    }

    fn render_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .child(self.render_transport(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_preview(cx))
                    .child(self.render_clips(cx)),
            )
            .child(self.render_tiles(cx))
            .child(self.render_frame_ops(cx))
            .child(self.render_strip(cx))
            .into_any_element()
    }
}

/// A `RenderImage`'s pixel size (frame 0 -- worldlib composes are always
/// single-frame).
fn image_px_size(image: &Arc<RenderImage>) -> (u32, u32) {
    let size = image.size(0);
    (size.width.0 as u32, size.height.0 as u32)
}

impl Render for MetaSpritePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_editors(window, cx);
        let body = match &self.state {
            ViewerState::Empty => self.render_message("Select a sprite".to_string(), cx),
            ViewerState::Loading { rel_path } => {
                self.render_message(format!("Loading {rel_path}…"), cx)
            }
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(window, cx),
        };
        v_flex()
            .key_context(self.dispatch_context(window, cx))
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &PlayPause, _window, cx| this.toggle_play(cx)))
            .on_action(cx.listener(|this, _: &Undo, _window, cx| this.undo_impl(cx)))
            .on_action(cx.listener(|this, _: &Redo, _window, cx| this.redo_impl(cx)))
            .on_action(cx.listener(|this, _: &Save, _window, cx| this.save_impl(cx)))
            .on_action(cx.listener(|this, _: &DeselectTile, _window, cx| this.deselect_tile(cx)))
            .on_action(cx.listener(Self::on_commit_field))
            .bg(cx.theme().colors().panel_background)
            .child(self.render_picker(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for MetaSpritePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for MetaSpritePanel {}

impl Panel for MetaSpritePanel {
    fn persistent_name() -> &'static str {
        "GGO MetaSprite"
    }

    fn panel_key() -> &'static str {
        GGO_METASPRITE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call as `ggo_world_panel`: no settings persistence yet, and
        // Bottom isn't a sensible spot for a sprite/frame editor sidebar.
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        DEFAULT_WIDTH
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        // `IconName` has no Film/animation glyph in this fork; `Image`
        // reads closest to a sprite/frame asset (checked against
        // crates/icons/src/icons.rs -- Sparkle was the other candidate but
        // reads as "AI/magic", not sprites).
        Some(IconName::Image)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO MetaSprite")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8 (grep activation_priority across
        // crates/).
        9
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_sprites` needs to READ
            // the workspace to find the project root -- reading it
            // re-entrantly panics (same as `ggo_world_panel`).
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_sprites(cx)).ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::cow::{ClipEdit, Frame};
    use ggo_worldlib::sprites::hw::{TILE_BYTES, TILE_PX};
    use ggo_worldlib::sprites::io::{open_sprite, save_sprite};
    use ggo_worldlib::sprites::sprite_doc::DEFAULT_FRAME_DURATION_MS;
    use ggo_worldlib::sprites::timeline_ops::MIN_FRAME_MS;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    /// Proves the panel is registered on a real workspace, and that
    /// dispatching `ToggleFocus` opens the right dock and focuses the
    /// panel. Goes through `MultiWorkspace::test_new` rather than a bare
    /// `Workspace::test_new`, because `register_action` handlers (like
    /// `ToggleFocus`) are only mounted into the dispatch tree once
    /// something renders `Workspace::actions`, which in production is
    /// `MultiWorkspace`'s render (same lesson as `ggo_world_panel`'s test).
    #[gpui::test]
    async fn test_toggle_focus_opens_panel(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.panel::<MetaSpritePanel>(cx).is_some(),
                "MetaSpritePanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<MetaSpritePanel>(cx)
                .expect("MetaSpritePanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    /// Author a real 2-frame sprite trio (`.spr`/`.til`/`.pal`) via
    /// worldlib's own `save_sprite` -- one call persists all three files
    /// (the pool IS the tileset; `save_tileset` would only rewrite the
    /// same `.til`/`.pal` pair, so it isn't needed). Frame 0 shows the
    /// all-transparent tile 0; frame 1 shows tile 1, filled with palette
    /// index 1 (red) -- distinguishable thumbnails. One non-looping
    /// single-frame clip exercises the clip selector path.
    fn write_sprite_fixture(root: &std::path::Path) {
        let mut pool = vec![0u8; 2 * TILE_BYTES];
        for b in &mut pool[TILE_BYTES..] {
            *b = 0x11; // both nibbles = palette index 1
        }
        let mut palette = [0u16; 16];
        palette[1] = 0xF800; // pure 565 red
        let state = SpriteState {
            pool,
            tile_count: 2,
            session_tiles: std::collections::HashSet::new(),
            palette,
            frames: vec![
                Frame {
                    map: vec![0],
                    duration_ms: 100,
                },
                Frame {
                    map: vec![1],
                    duration_ms: 200,
                },
            ],
            clips: vec![ClipEdit {
                name: "walk".to_string(),
                from: 1,
                to: 1,
                loop_: false,
            }],
            w_tiles: 1,
            h_tiles: 1,
            pool_shared: false,
        };
        save_sprite(
            root,
            "sprites/hero.spr",
            &state,
            "sprites/hero.til",
            "sprites/hero.pal",
        )
        .unwrap();
    }

    /// Load the fixture sprite into a fresh panel and return it Ready.
    async fn ready_panel(
        cx: &mut TestAppContext,
        root: &std::path::Path,
    ) -> gpui::Entity<MetaSpritePanel> {
        write_sprite_fixture(root);
        let root = root.to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = MetaSpritePanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_sprites(cx);
            assert_eq!(panel.sprites, ["sprites/hero.spr"]);
            panel.select_sprite(0, cx);
        });
        cx.executor().run_until_parked();
        panel
    }

    /// End-to-end viewer load against a real-fs temp project: the picker
    /// enumerates the fixture `.spr`, selecting it runs the off-thread
    /// loader, and the panel reaches Ready with one composed thumbnail
    /// per frame (frame 0 all-transparent, frame 1 opaque red -- proving
    /// per-frame compose order survived the BGRA bridge) and the resolved
    /// sidecar rels the save path writes back to (M5's `LoadedSprite`
    /// extension).
    #[gpui::test]
    async fn test_select_sprite_reaches_ready_with_frame_thumbnails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, _cx| {
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready state after load");
            };
            assert_eq!(open.rel_path, "sprites/hero.spr");
            assert_eq!(open.til_path, "sprites/hero.til");
            assert_eq!(open.pal_path, "sprites/hero.pal");
            let state = open.store.state();
            assert_eq!(state.frames.len(), 2);
            assert_eq!(open.frames.len(), 2, "one thumbnail per frame");
            assert_eq!(state.frames[0].duration_ms, 100);
            assert_eq!(state.frames[1].duration_ms, 200);
            assert_eq!(state.clips.len(), 1);
            assert_eq!(open.shown_frame(), 0, "not playing => selected frame");
            assert!(!open.store.dirty(), "freshly opened => clean");

            let px_count = TILE_PX * TILE_PX;
            let f0 = open.frames[0].as_bytes(0).unwrap();
            assert_eq!(f0.len(), px_count * 4);
            assert!(
                f0.chunks_exact(4).all(|p| p[3] == 0),
                "frame 0 (tile 0, all index 0) must be fully transparent"
            );
            let f1 = open.frames[1].as_bytes(0).unwrap();
            assert!(
                f1.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                "frame 1 (tile 1, palette red) must be opaque red in BGRA"
            );
        });
    }

    /// Frame selection + clip selection state: strip clicks move the
    /// selected (and shown) frame, out-of-range clicks are ignored, and
    /// the clip selector round-trips through `active_clip`.
    #[gpui::test]
    async fn test_frame_and_clip_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.select_frame(1, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected_frame, 1);
            assert_eq!(open.shown_frame(), 1);

            panel.select_frame(9, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.selected_frame, 1, "out-of-range click is ignored");

            panel.select_clip(Some(0), cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.active_clip, Some(0));
            assert_eq!(
                playback::play_range(&open.store.state().clips, open.active_clip, 2),
                (1, 1)
            );

            panel.select_clip(None, cx);
            let ViewerState::Ready(open) = &panel.state else {
                panic!("expected Ready");
            };
            assert_eq!(open.active_clip, None);
        });
    }

    fn ready(panel: &MetaSpritePanel) -> &OpenSprite {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
    }

    /// Clip CRUD round trip through the store: add with ggo-ide's
    /// defaults, rename (trim + apply), retarget the range, toggle loop,
    /// delete (clearing the active selection) -- then undo each step back
    /// to the fixture and redo forward again, with the dirty flag
    /// tracking the whole way.
    #[gpui::test]
    async fn test_clip_add_edit_delete_undo_round_trip(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.add_clip(cx);
            {
                let open = ready(panel);
                let clips = &open.store.state().clips;
                assert_eq!(clips.len(), 2);
                assert_eq!(
                    clips[1],
                    ClipEdit {
                        name: "clip2".into(), // fixture has 1 clip -> clip{len+1}
                        from: 0,              // selected frame
                        to: 0,
                        loop_: false
                    }
                );
                assert!(open.store.dirty());
            }

            panel.commit_edit(EditTarget::ClipName(1), "  run  ".into(), cx);
            assert_eq!(ready(panel).store.state().clips[1].name, "run");

            panel.commit_edit(EditTarget::ClipTo(1), "1".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips[1].to, 1);
                assert!(open.clip_error.is_none());
            }

            panel.set_clip_loop(1, true, cx);
            assert!(ready(panel).store.state().clips[1].loop_);

            panel.select_clip(Some(1), cx);
            panel.delete_clip(1, cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips.len(), 1);
                assert_eq!(
                    open.active_clip, None,
                    "deleting the active clip clears the selection"
                );
            }

            panel.undo_impl(cx); // un-delete
            assert!(ready(panel).store.state().clips[1].loop_);
            panel.undo_impl(cx); // un-loop
            assert!(!ready(panel).store.state().clips[1].loop_);
            panel.undo_impl(cx); // un-retarget
            assert_eq!(ready(panel).store.state().clips[1].to, 0);
            panel.undo_impl(cx); // un-rename
            assert_eq!(ready(panel).store.state().clips[1].name, "clip2");
            panel.undo_impl(cx); // un-add
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips.len(), 1);
                assert!(
                    !open.store.dirty(),
                    "undo back to the loaded state => clean"
                );
            }

            panel.redo_impl(cx);
            assert_eq!(ready(panel).store.state().clips[1].name, "clip2");
        });
    }

    /// Clip-edit guards: an out-of-range or reversed typed range is shown
    /// inline and NOT applied; unparsable text is dropped silently; stale
    /// clip indices (undo raced a click) are ignored everywhere instead of
    /// reaching the store's panicking arms.
    #[gpui::test]
    async fn test_clip_edit_guards_and_stale_indices(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Fixture clip walk = (1, 1) over 2 frames.
            panel.commit_edit(EditTarget::ClipTo(0), "9".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips[0].to, 1, "not applied");
                let (at, msg) = open.clip_error.as_ref().expect("inline error set");
                assert_eq!(*at, 0);
                assert!(msg.contains("outside frames"), "range message: {msg}");
            }

            panel.commit_edit(EditTarget::ClipTo(0), "0".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(
                    open.store.state().clips[0].to,
                    1,
                    "reversed range not applied"
                );
                let (_, msg) = open.clip_error.as_ref().expect("inline error set");
                assert!(msg.contains('>'), "inversion message: {msg}");
            }

            panel.commit_edit(EditTarget::ClipFrom(0), "abc".into(), cx);
            assert_eq!(
                ready(panel).store.state().clips[0].from,
                1,
                "unparsable text dropped"
            );

            // A valid edit applies and clears the inline error.
            panel.commit_edit(EditTarget::ClipFrom(0), "0".into(), cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips[0].from, 0);
                assert!(open.clip_error.is_none());
            }

            // Whitespace-only rename is dropped; over-long is byte-clamped.
            panel.commit_edit(EditTarget::ClipName(0), "   ".into(), cx);
            assert_eq!(ready(panel).store.state().clips[0].name, "walk");
            let long = "z".repeat(200);
            panel.commit_edit(EditTarget::ClipName(0), long, cx);
            assert_eq!(
                ready(panel).store.state().clips[0].name.len(),
                ggo_worldlib::sprites::sprite_doc::SPR_CLIP_NAME_MAX
            );

            // Stale indices: no panic, no change.
            panel.delete_clip(7, cx);
            panel.set_clip_loop(7, true, cx);
            panel.commit_edit(EditTarget::ClipName(7), "x".into(), cx);
            panel.commit_edit(EditTarget::ClipFrom(7), "0".into(), cx);
            assert_eq!(ready(panel).store.state().clips.len(), 1);

            // Stale FRAME selection: force one, then run every frame op.
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selected_frame = 9;
            }
            panel.delete_selected_frame(cx);
            panel.duplicate_selected_frame(cx);
            panel.move_selected_frame(1, cx);
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
            assert_eq!(ready(panel).store.state().frames.len(), 2, "all ignored");
        });
    }

    /// Frame ops through the store: blank add, duplicate (selects the
    /// copy, thumbnail cache refreshed with the copied pixels), adjacent
    /// move (selection follows), duration floor, delete with ggo-ide's
    /// selection rule and last-frame guard -- then undo everything back to
    /// the fixture, thumbnails included.
    #[gpui::test]
    async fn test_frame_ops_with_undo(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Add blank: appended at the end, default duration.
            panel.add_blank_frame(cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 3);
                assert_eq!(state.frames[2].duration_ms, DEFAULT_FRAME_DURATION_MS);
                assert_eq!(open.frames.len(), 3, "thumbnail cache refreshed");
            }

            // Duplicate the red frame 1: copy lands at 2, selected, and
            // its RECOMPOSED thumbnail carries the copied pixels (the M5
            // invalidation hook actually recomposing, not just resizing).
            panel.select_frame(1, cx);
            panel.duplicate_selected_frame(cx);
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(state.frames.len(), 4);
                assert_eq!(open.selected_frame, 2);
                assert_eq!(state.frames[2].duration_ms, 200, "copy_of copies duration");
                let dup = open.frames[2].as_bytes(0).unwrap();
                assert!(
                    dup.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                    "duplicated frame's thumbnail is the copied red frame"
                );
            }

            // Move the copy left: neighbor swap, selection follows.
            panel.move_selected_frame(-1, cx);
            {
                let open = ready(panel);
                let durations: Vec<u16> = open
                    .store
                    .state()
                    .frames
                    .iter()
                    .map(|f| f.duration_ms)
                    .collect();
                assert_eq!(durations, [100, 200, 200, 100]);
                assert_eq!(open.selected_frame, 1);
            }
            // Move left at index 0 is a no-op.
            panel.select_frame(0, cx);
            panel.move_selected_frame(-1, cx);
            assert_eq!(ready(panel).store.state().frames[0].duration_ms, 100);

            // Duration: sub-floor input floors to MIN_FRAME_MS; a real
            // value sticks; the strip label data (doc) and cache agree.
            panel.select_frame(3, cx);
            panel.commit_edit(EditTarget::Duration, "1".into(), cx);
            assert_eq!(
                ready(panel).store.state().frames[3].duration_ms,
                MIN_FRAME_MS
            );
            panel.commit_edit(EditTarget::Duration, "320".into(), cx);
            assert_eq!(ready(panel).store.state().frames[3].duration_ms, 320);

            // Delete the selected last frame: selection clamps to the new
            // end (ggo-ide's rule), fixture clip survives untouched.
            panel.delete_selected_frame(cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames.len(), 3);
                assert_eq!(open.selected_frame, 2);
                assert_eq!(open.frames.len(), 3);
            }

            // Delete down to one, then verify the last-frame guard.
            panel.delete_selected_frame(cx);
            panel.delete_selected_frame(cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames.len(), 1);
                assert_eq!(open.selected_frame, 0);
                assert_eq!(
                    open.store.state().clips.len(),
                    0,
                    "deleting the walk clip's sole frame dropped the clip (store rule)"
                );
            }
            panel.delete_selected_frame(cx);
            assert_eq!(
                ready(panel).store.state().frames.len(),
                1,
                "the last frame can't be deleted"
            );

            // Undo everything: back to the loaded fixture, clean, with the
            // thumbnail cache re-derived from the restored doc.
            for _ in 0..20 {
                panel.undo_impl(cx);
            }
            {
                let open = ready(panel);
                let state = open.store.state();
                let durations: Vec<u16> = state.frames.iter().map(|f| f.duration_ms).collect();
                assert_eq!(durations, [100, 200]);
                assert_eq!(state.clips.len(), 1, "walk clip restored");
                assert_eq!(open.frames.len(), 2);
                assert!(!open.store.dirty());
                let f1 = open.frames[1].as_bytes(0).unwrap();
                assert!(f1.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
            }
        });
    }

    /// Tile setting end to end (M6): the pool-tile palette composes one
    /// pinned-byte thumbnail per tile; selection toggles (re-click and
    /// the Escape action's path both deselect) and ignores stale
    /// indices; a cell click with a tile active applies `FrameTileSet`
    /// on the SELECTED frame and the recomposed frame pixels become the
    /// assigned tile's pixels; unchanged-cell / out-of-range / no-tile
    /// clicks don't touch the store; undo restores the prior bytes and
    /// redo re-applies. Plus the window->cell mapping through manually
    /// stamped preview bounds (the headless panel never paints).
    #[gpui::test]
    async fn test_tile_palette_and_cell_set_with_undo(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                assert_eq!(open.pool_tiles.len(), 2, "one thumbnail per pool tile");
                let t0 = open.pool_tiles[0].as_bytes(0).unwrap();
                assert_eq!(t0.len(), TILE_PX * TILE_PX * 4);
                assert!(
                    t0.chunks_exact(4).all(|p| p[3] == 0),
                    "tile 0 (all index 0) must compose fully transparent"
                );
                let t1 = open.pool_tiles[1].as_bytes(0).unwrap();
                assert!(
                    t1.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                    "tile 1 (palette red) must compose opaque red in BGRA"
                );
                assert_eq!(open.selected_tile, None);
            }

            // Selection toggles; stale indices are ignored.
            panel.select_tile(1, cx);
            assert_eq!(ready(panel).selected_tile, Some(1));
            panel.select_tile(1, cx);
            assert_eq!(ready(panel).selected_tile, None, "re-click deselects");
            panel.select_tile(9, cx);
            assert_eq!(ready(panel).selected_tile, None, "stale tile index ignored");
            panel.select_tile(1, cx);

            // Guarded clicks leave the store untouched: out-of-range
            // cell; a cell that already shows the selected tile.
            panel.set_tile_on_cell(5, cx);
            assert!(!ready(panel).store.dirty(), "out-of-range cell dropped");
            panel.select_frame(1, cx); // frame 1's sole cell is already tile 1
            panel.set_tile_on_cell(0, cx);
            assert!(!ready(panel).store.dirty(), "same-tile click drops the op");

            // The real edit: frame 0 cell 0 -> tile 1.
            panel.select_frame(0, cx);
            panel.set_tile_on_cell(0, cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames[0].map, vec![1]);
                assert!(open.store.dirty());
                let f0 = open.frames[0].as_bytes(0).unwrap();
                assert!(
                    f0.chunks_exact(4).all(|p| p == [0, 0, 255, 255]),
                    "frame 0 recomposed to the assigned tile's red pixels"
                );
            }

            // Escape's action body; with nothing selected, clicks stop
            // mutating.
            panel.deselect_tile(cx);
            assert_eq!(ready(panel).selected_tile, None);
            panel.set_tile_on_cell(0, cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1]);

            // Undo restores the prior composed bytes; redo re-applies.
            panel.undo_impl(cx);
            {
                let open = ready(panel);
                assert_eq!(open.store.state().frames[0].map, vec![0]);
                assert!(!open.store.dirty(), "back to the loaded state");
                let f0 = open.frames[0].as_bytes(0).unwrap();
                assert!(
                    f0.chunks_exact(4).all(|p| p[3] == 0),
                    "undo recomposed frame 0 back to transparent"
                );
            }
            panel.redo_impl(cx);
            assert_eq!(ready(panel).store.state().frames[0].map, vec![1]);

            // Window->cell mapping over stamped bounds: a 1x1-tile sprite
            // maps every in-box point to cell 0 and rejects outside.
            *ready(panel).preview_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(10.), px(20.)),
                gpui::size(px(240.), px(240.)),
            ));
            assert_eq!(
                panel.preview_cell_at(gpui::point(px(10.), px(20.))),
                Some(0)
            );
            assert_eq!(
                panel.preview_cell_at(gpui::point(px(249.), px(259.))),
                Some(0)
            );
            assert_eq!(panel.preview_cell_at(gpui::point(px(9.), px(20.))), None);
            assert_eq!(panel.preview_cell_at(gpui::point(px(250.), px(20.))), None);
        });
    }

    /// M6 fix-forward: a preview click while the transport is running
    /// pauses it, adopts the transport's SHOWN frame as the selection,
    /// and only then maps the click -- so the edit lands on the frame
    /// the user saw, not the stale pre-playback selection (which here
    /// would have been silently dropped as a same-tile click).
    #[gpui::test]
    async fn test_preview_click_during_playback_pauses_and_edits_shown_frame(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            // Tile 0 active; frame 0 selected (map [0] -- a click on it
            // would be a same-tile no-op); the transport is showing
            // frame 1 (map [1]).
            panel.select_tile(0, cx);
            if let ViewerState::Ready(open) = &mut panel.state {
                open.selected_frame = 0;
                open.playing = Some(Playing {
                    started: Instant::now(),
                    start_offset_ms: 0,
                    frame: 1,
                });
            }
            *ready(panel).preview_bounds.borrow_mut() = Some(gpui::bounds(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(240.), px(240.)),
            ));

            panel.on_preview_click(gpui::point(px(10.), px(10.)), cx);
            {
                let open = ready(panel);
                assert!(open.playing.is_none(), "the click pauses the transport");
                assert!(open._tick_task.is_none(), "the tick loop is dropped");
                assert_eq!(
                    open.selected_frame, 1,
                    "selection adopts the transport's shown frame"
                );
                assert_eq!(
                    open.store.state().frames[1].map,
                    vec![0],
                    "the edit hit the DISPLAYED frame"
                );
                assert_eq!(
                    open.store.state().frames[0].map,
                    vec![0],
                    "the stale pre-playback selection is untouched"
                );
                assert_eq!(open.shown_frame(), 1, "preview stays on the edited frame");
            }

            // Not playing: the same click path edits the selected frame
            // directly (undo the playback edit first so the cell click
            // isn't a same-tile drop).
            panel.undo_impl(cx);
            panel.select_frame(1, cx);
            panel.on_preview_click(gpui::point(px(10.), px(10.)), cx);
            assert_eq!(
                ready(panel).store.state().frames[1].map,
                vec![0],
                "a click with no transport running still edits the selected frame"
            );
        });
    }

    /// Save: `save_sprite` -> `set_saved_state` clears dirty without an
    /// undo entry; the written trio `open_sprite`-round-trips equal to the
    /// store's state; undo after save still steps back through the user's
    /// edits (and re-dirties, since the saved generation moved on).
    #[gpui::test]
    async fn test_save_round_trips_through_open_sprite_and_clears_dirty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.commit_edit(EditTarget::Duration, "40".into(), cx);
            panel.add_clip(cx);
            assert!(ready(panel).store.dirty());

            panel.save_impl(cx);
            {
                let open = ready(panel);
                assert!(!open.store.dirty(), "save clears dirty");
                assert!(open.save_error.is_none());
            }
        });

        let reopened = open_sprite(dir.path(), "sprites/hero.spr").unwrap();
        panel.update(cx, |panel, cx| {
            {
                let open = ready(panel);
                let state = open.store.state();
                assert_eq!(reopened.state.frames, state.frames);
                assert_eq!(reopened.state.clips, state.clips);
                assert_eq!(reopened.state.palette, state.palette);
                assert_eq!(reopened.state.pool, state.pool);
                assert_eq!(reopened.state.tile_count, state.tile_count);
                assert_eq!(reopened.state.w_tiles, state.w_tiles);
                assert_eq!(reopened.state.h_tiles, state.h_tiles);
                assert_eq!(reopened.til_path, open.til_path);
                assert_eq!(reopened.pal_path, open.pal_path);
                assert_eq!(reopened.state.frames[0].duration_ms, 40);
                assert_eq!(reopened.state.clips.len(), 2);
            }

            panel.undo_impl(cx); // fold-back wasn't an undo entry; this undoes add_clip
            {
                let open = ready(panel);
                assert_eq!(open.store.state().clips.len(), 1);
                assert!(open.store.dirty(), "undo past the save point re-dirties");
            }
        });
    }
}
