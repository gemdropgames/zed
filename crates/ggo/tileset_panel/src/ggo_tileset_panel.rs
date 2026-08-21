//! GGO Tileset panel (F4 task X2): a READ-ONLY viewer for a `.til` tile
//! sheet -- the composed tile grid, the tile count, the 16-slot palette
//! the sheet is drawn through, and the source `.til`/`.pal` rels.
//!
//! Deliberately read-only. worldlib exposes a full editing surface for
//! tilesets (`tileset_doc::TilesetDocStore` + `TilesetOp::Paint`/`Fill`/
//! `SetPalette`/... and `io::save_tileset`), and NONE of it is wired up
//! here: no store, no ops, no save, no dirty state. That is a product
//! decision, not an oversight -- pixel authoring was scoped out of the
//! migration entirely. This comment used to say "pixel editing stays in
//! ggo-ide for now"; ggo-ide was deleted in ggo `281fd557` (F5.5), so there
//! is no "for now" and no fallback -- pixel editing happens in an external
//! editor plus the `.png` import path (`ggo_import_panel`).
//! The direct consequence for this module: there is nothing this panel
//! can lose, so unlike `ggo_world_panel`/`ggo_sprite_panel` it has no
//! `Panel::prepare_to_close` override and never calls
//! `ggo_common::prepare_to_close_dirty`. If a `TilesetOp` is ever applied
//! from this panel, BOTH must come back with it.
//!
//! Which tileset is open is driven ENTIRELY by the file explorer (F4 X1):
//! clicking a `.til` there routes here through [`intercept_tileset_open`];
//! the panel has no picker of its own.
//!
//! Structural mirror of `ggo_charts_panel`/`ggo_sprite_panel`: `Panel`
//! impl, `ToggleFocus`, `observe_new` registration into every new
//! workspace, a `KeymapEventChannel` observer scaffold, and off-thread
//! loading behind a load-generation staleness guard. `loader` owns
//! everything off the UI thread (the `.til` open, the grid compose) plus
//! the pure grid geometry; this module owns the panel entity and the gpui
//! glue.

mod loader;

use std::path::PathBuf;

use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels, Render,
    Styled, Task, WeakEntity, Window, actions, div, img, px, rgb, rgba,
};
use project::ProjectPath;
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use ggo_worldlib::sprites::palette565::PAL_SLOTS;

use loader::LoadedTileset;

actions!(
    ggo_tileset,
    [
        /// Toggles focus on the GGO tileset panel.
        ToggleFocus
    ]
);

const GGO_TILESET_PANEL_KEY: &str = "GGOTilesetPanel";

/// The panel's key-dispatch context identifier. No bindings are scoped to
/// it yet (see [`bind_panel_keys`]) -- zoom is button-only so far -- but
/// it exists so a later keyboard affordance lands in a context without
/// touching this module's `init`/`Render` wiring.
const KEY_CONTEXT: &str = "GgoTilesetPanel";

/// Fixed default width until the panel grows real settings persistence
/// (same call every other GGO panel made at this stage).
const DEFAULT_WIDTH: Pixels = px(360.);

/// Integer zoom bounds for the grid view. Integer only: the sheet is
/// pixel art, and a non-integer scale would resample 16x16 tiles into
/// blur. 1x is unreadably small on a HiDPI panel, so the default is 2x.
const MIN_ZOOM: usize = 1;
const MAX_ZOOM: usize = 8;
const DEFAULT_ZOOM: usize = 2;

/// Palette swatch box (px, square).
const SWATCH_PX: f32 = 16.0;

/// The tileset extension this panel claims from the file explorer.
const TILESET_EXT: &str = "til";

/// Empty-state text. The panel has no picker of its own by design (F4 X1):
/// tilesets arrive by clicking a `.til` in the project panel.
const EMPTY_MESSAGE: &str = "Open a .til file from the project panel";

pub fn init(cx: &mut App) {
    bind_panel_keys(cx);
    // Same rule as every other GGO panel's `init`: `zed::reload_keymaps`
    // clears and rebuilds ALL key bindings on every keymap/settings change
    // (including once at startup), and keymap assets are upstream files
    // this fork doesn't edit. Re-running `bind_panel_keys` on
    // `KeymapEventChannel` keeps the panel's bindings alive across
    // reloads -- required scaffolding for any panel with keybinds, kept
    // now rather than retrofitted when the first one lands.
    cx.observe_global::<keymap_editor::KeymapEventChannel>(bind_panel_keys)
        .detach();

    // Explorer-driven routing: clicking a `.til` in the project panel loads
    // it HERE instead of opening a (binary, unreadable) editor tab. This is
    // the panel's only way in -- there is no in-panel file picker.
    workspace::register_path_open_interceptor(cx, intercept_tileset_open);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| TilesetPanel::new(Some(weak_workspace), cx));
        workspace.add_panel(panel, window, cx);

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TilesetPanel>(window, cx);
        });
    })
    .detach();
}

/// No panel-specific keybinds exist yet: the viewer's only interaction is
/// the zoom pair, both clickable. Kept as its own fn (rather than inlined
/// into `init`) so it matches the other GGO panels' shape exactly: `init`
/// calls it once at startup AND the `KeymapEventChannel` observer calls it
/// again on every reload.
fn bind_panel_keys(cx: &mut App) {
    cx.bind_keys([]);
}

/// `workspace::PathOpenInterceptor` for `*.til`: claim the path, open the
/// panel, and load it. Declines (so the normal open path runs) for any other
/// file, for a path outside the primary worktree, and when no panel is
/// docked.
///
/// Note the extension split with `ggo_sprite_panel`: a sprite's `.til`
/// is its tile POOL and is opened through the `.spr` that names it, so
/// clicking the `.til` itself lands here (the sheet, read-only) rather than
/// in the sprite editor. Both interceptors key off disjoint extensions, so
/// registration order between them doesn't matter.
fn intercept_tileset_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !path
        .path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(TILESET_EXT))
    {
        return false;
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    ggo_common::open_in_panel(
        workspace,
        window,
        cx,
        move |panel: &mut TilesetPanel, window, cx| panel.open_rel_path(&rel, window, cx),
    )
}

// ------------------------------------------------------------- view state

/// The open tileset: worldlib's snapshot plus the view state (zoom) that
/// must survive a re-click on the already-open file.
struct OpenTileset {
    rel_path: String,
    loaded: LoadedTileset,
    zoom: usize,
}

impl OpenTileset {
    fn new(rel_path: String, loaded: LoadedTileset) -> Self {
        Self {
            rel_path,
            loaded,
            zoom: DEFAULT_ZOOM,
        }
    }

    /// The grid's on-screen size at the current zoom.
    fn zoomed_size(&self) -> (f32, f32) {
        let (w, h) = self.loaded.grid_size;
        let z = self.zoom as f32;
        (w as f32 * z, h as f32 * z)
    }
}

enum ViewerState {
    /// Nothing opened yet.
    Empty,
    Loading {
        rel_path: String,
    },
    Ready(Box<OpenTileset>),
    Error(String),
}

pub struct TilesetPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    root_override: Option<PathBuf>,
    project_root: Option<PathBuf>,
    state: ViewerState,
    load_generation: u64,
    _load_task: Option<Task<()>>,
}

impl TilesetPanel {
    pub fn new(workspace: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            workspace,
            root_override: None,
            project_root: None,
            state: ViewerState::Empty,
            load_generation: 0,
            _load_task: None,
        }
    }

    /// Re-discover the project root (the workspace's first visible
    /// worktree). MUST NOT run while the workspace itself is mid-update
    /// (it reads the workspace entity) -- see the deferral in `set_active`
    /// and in [`Self::open_rel_path`].
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        cx.notify();
    }

    /// Load the project-relative `.til` path `rel`. This is the panel's
    /// entry point from the file explorer ([`intercept_tileset_open`]);
    /// there is no in-panel picker.
    ///
    /// No unsaved-document guard, unlike the world/sprite panels: this
    /// viewer holds no edits (see the module doc), so there is nothing a
    /// switch could discard.
    ///
    /// The load runs on a spawned task, deliberately: the interceptor calls
    /// this from INSIDE the workspace's own update, and [`Self::refresh_root`]
    /// has to read that same workspace entity.
    pub fn open_rel_path(&mut self, rel: &str, _window: &mut Window, cx: &mut Context<Self>) {
        // Clicking the file that is ALREADY open is how you bring the panel
        // back into focus, and upstream's semantics for that click on a tab
        // are "activate the existing item", not "reload it". The interceptor
        // has already revealed and focused the dock by the time we get here,
        // so there is nothing left to do -- and reloading would drop the
        // view state (zoom, and the scroll position gpui keeps per element
        // id) for no reason.
        if let ViewerState::Ready(open) = &self.state
            && open.rel_path == rel
        {
            return;
        }
        let rel = rel.to_string();
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.refresh_root(cx);
                this.load_rel_path(&rel, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Kick off the off-thread load of `rel`. A stale result (superseded by
    /// a later open) is dropped by generation check.
    fn load_rel_path(&mut self, rel: &str, cx: &mut Context<Self>) {
        let rel = rel.to_string();
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
            cx.background_spawn(async move { loader::load_tileset(&root, &rel) })
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(loaded) => ViewerState::Ready(Box::new(OpenTileset::new(rel, loaded))),
                    Err(e) => ViewerState::Error(e),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// The `.til` currently open, as the worktree-relative path it was opened
    /// WITH -- `None` unless the viewer is Ready.
    ///
    /// Public purely as an observation point for the panels that hand a
    /// tileset OFF to this one: `ggo_import_panel` opens the sheet it just
    /// wrote here, and the rel it passes has to be the worktree-relative one
    /// (an asset-root-relative rel names no file in the worktree -- the F4
    /// `ggo-sprfix` distinction). Without this, that hand-off could only be
    /// asserted from inside this crate, which is not where the bug would be.
    pub fn open_rel_path_now(&self) -> Option<&str> {
        match &self.state {
            ViewerState::Ready(open) => Some(open.rel_path.as_str()),
            _ => None,
        }
    }

    /// Step the zoom by `delta` steps, clamped to [`MIN_ZOOM`]..=[`MAX_ZOOM`].
    fn zoom_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ViewerState::Ready(open) = &mut self.state else {
            return;
        };
        let next =
            (open.zoom as isize + delta).clamp(MIN_ZOOM as isize, MAX_ZOOM as isize) as usize;
        if next != open.zoom {
            open.zoom = next;
            cx.notify();
        }
    }

    // ------------------------------------------------------------- render

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

    /// Header: the source rels, tile count / grid geometry, and the zoom
    /// pair.
    fn render_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_header is only called in the Ready state");
        };
        let (w, h) = open.loaded.grid_size;
        let summary = format!(
            "{} tiles · {}x{} px · {} cols",
            open.loaded.tile_count, w, h, open.loaded.cols
        );
        let zoom_label = format!("{}x", open.zoom);
        v_flex()
            .gap_0p5()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(open.rel_path.clone()).size(LabelSize::Small))
            .child(
                Label::new(open.loaded.pal_path.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new(summary)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("ggo-tileset-zoom-out", IconName::Dash)
                            .icon_size(IconSize::XSmall)
                            .disabled(open.zoom <= MIN_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_by(-1, cx))),
                    )
                    .child(Label::new(zoom_label).size(LabelSize::XSmall))
                    .child(
                        IconButton::new("ggo-tileset-zoom-in", IconName::Plus)
                            .icon_size(IconSize::XSmall)
                            .disabled(open.zoom >= MAX_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_by(1, cx))),
                    ),
            )
            .into_any_element()
    }

    /// The 16-slot palette row. Slot 0 is the locked transparent entry, so
    /// it renders as an outlined empty box rather than whatever color the
    /// `.pal` happens to store there -- matching how the grid draws it.
    fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_palette is only called in the Ready state");
        };
        let palette = open.loaded.palette;
        let note = if open.loaded.missing_pal {
            "Palette (no .pal found — 16-gray fallback)"
        } else {
            "Palette"
        };
        v_flex()
            .gap_0p5()
            .p_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_0p5()
                    .children((0..PAL_SLOTS).map(|slot| {
                        let [r, g, b, a] = loader::swatch_rgba(&palette, slot);
                        let color = u32::from_be_bytes([0, r, g, b]);
                        div()
                            .id(("ggo-tileset-swatch", slot))
                            .w(px(SWATCH_PX))
                            .h(px(SWATCH_PX))
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .rounded_sm()
                            // Slot 0 is transparent: show the panel through
                            // it instead of painting a misleading color.
                            .when(a != 0, |el| el.bg(rgb(color)))
                            .when(a == 0, |el| el.bg(rgba(0x00000000)))
                            .tooltip(ui::Tooltip::text(format!(
                                "{slot}: #{:04X}{}",
                                palette[slot],
                                if a == 0 { " (transparent)" } else { "" }
                            )))
                    })),
            )
            .into_any_element()
    }

    /// The composed tile sheet, scaled by the integer zoom and scrollable
    /// in both axes (an 8-col sheet at 8x is far wider than a dock).
    fn render_grid(&self) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_grid is only called in the Ready state");
        };
        let (w, h) = open.zoomed_size();
        div()
            .id("ggo-tileset-grid")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .child(
                div()
                    .p_1()
                    .child(img(open.loaded.grid.clone()).nearest(true).w(px(w)).h(px(h))),
            )
            .into_any_element()
    }

    fn render_ready(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(self.render_grid())
            .child(self.render_palette(cx))
            .into_any_element()
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.state {
            ViewerState::Empty => self.render_message(EMPTY_MESSAGE.to_string(), cx),
            ViewerState::Loading { rel_path } => {
                self.render_message(format!("Loading {rel_path}…"), cx)
            }
            ViewerState::Error(e) => self.render_message(format!("Failed to load: {e}"), cx),
            ViewerState::Ready(_) => self.render_ready(cx),
        }
    }
}

impl Render for TilesetPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.render_body(cx);
        v_flex()
            .key_context(KEY_CONTEXT)
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for TilesetPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TilesetPanel {}

impl Panel for TilesetPanel {
    fn persistent_name() -> &'static str {
        "GGO Tileset"
    }

    fn panel_key() -> &'static str {
        GGO_TILESET_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // Same call as every other GGO panel: no settings persistence yet,
        // and Bottom isn't a sensible spot for a tall tile sheet.
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
        // `Blocks` is the only tile/grid-shaped glyph in this checkout
        // (`assets/icons/blocks.svg` -- interlocking rectangles; grep for
        // grid/table/tile in assets/icons finds nothing else), and no
        // other panel uses it as its dock icon. `Image` is taken by
        // `ggo_sprite_panel`.
        Some(IconName::Blocks)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("GGO Tileset")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Verified free at checkout: built-in panels use 0-7,
        // `ggo_world_panel` took 8, `ggo_sprite_panel` 9,
        // `ggo_charts_panel` 10, `ggo_emu_panel` 11 (grep
        // activation_priority across crates/).
        12
    }

    // No `prepare_to_close`: this panel is read-only and holds nothing
    // unsaved -- see the module doc.

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            // Deferred: `set_active` fires inside the workspace's own
            // update (dock toggle), and `refresh_root` needs to READ the
            // workspace to find the project root -- reading it
            // re-entrantly panics (same as every other GGO panel).
            let this = cx.weak_entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.refresh_root(cx)).ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::io::{open_tileset, save_tileset};
    use ggo_worldlib::sprites::tileset_doc::{TILE_PIXELS, TILE_PX};
    use gpui::{Entity, TestAppContext};
    use project::{FakeFs, Project, WorktreeId};
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
    /// `MultiWorkspace`'s render (same lesson as the other GGO panels').
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
                workspace.panel::<TilesetPanel>(cx).is_some(),
                "TilesetPanel should have been added to the workspace by init()"
            );
            assert!(
                !workspace.right_dock().read(cx).is_open(),
                "right dock should start closed"
            );
        });

        cx.dispatch_action(ToggleFocus);

        workspace.update(cx, |workspace, cx| {
            let panel = workspace
                .panel::<TilesetPanel>(cx)
                .expect("TilesetPanel should still be registered");
            assert_eq!(panel.read(cx).position, DockPosition::Right);
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "ToggleFocus should have opened the right dock"
            );
        });
    }

    /// The fixture tile count: 3 tiles is deliberately NOT a multiple of
    /// the 8-column fallback, so `grid_cols`'s short-sheet clamp is
    /// exercised end to end.
    const FIXTURE_TILES: usize = 3;

    /// Author a real `.til`/`.pal` pair via worldlib's own `save_tileset`
    /// (one call writes both). Tile 0 stays all index 0 (transparent),
    /// tiles 1 and 2 are solid palette index 1 (red) -- so the composed
    /// grid is provably non-blank and the transparent/opaque split is
    /// visible in the pixels.
    fn write_tileset_fixture(root: &std::path::Path, stem: &str) {
        let mut indices = vec![0u8; FIXTURE_TILES * TILE_PIXELS];
        for b in &mut indices[TILE_PIXELS..] {
            *b = 1;
        }
        let mut palette = [0u16; PAL_SLOTS];
        palette[1] = 0xF800; // pure 565 red
        save_tileset(
            root,
            &format!("tiles/{stem}.til"),
            &indices,
            FIXTURE_TILES,
            &palette,
        )
        .unwrap();
    }

    /// Load the fixture tileset into a fresh panel and return it Ready.
    async fn ready_panel(cx: &mut TestAppContext, root: &std::path::Path) -> Entity<TilesetPanel> {
        write_tileset_fixture(root, "world");
        let root = root.to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
        });
        cx.executor().run_until_parked();
        panel
    }

    fn ready(panel: &TilesetPanel) -> &OpenTileset {
        match &panel.state {
            ViewerState::Ready(open) => open,
            _ => panic!("expected Ready"),
        }
    }

    /// End-to-end viewer load against a real-fs temp project: opening the
    /// fixture `.til` by rel path runs the off-thread loader and the panel
    /// reaches Ready with the expected tile count, the clamped column
    /// count, the fixture's own palette (not the grayscale fallback), the
    /// derived `.pal` rel, and a non-empty composed grid whose pixels prove
    /// tile 0 transparent / tile 1 red through the BGRA bridge.
    #[gpui::test]
    async fn test_open_til_reaches_ready_with_a_composed_grid(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            assert_eq!(open.rel_path, "tiles/world.til");
            assert_eq!(open.loaded.pal_path, "tiles/world.pal");
            assert_eq!(open.loaded.tile_count, FIXTURE_TILES);
            assert!(!open.loaded.missing_pal, "the fixture wrote a .pal");
            assert_eq!(open.loaded.palette[1], 0xF800);
            assert_eq!(
                open.loaded.cols, FIXTURE_TILES,
                "a sheet shorter than one 8-col row lays out at its own width"
            );
            assert_eq!(
                open.loaded.grid_size,
                ((FIXTURE_TILES * TILE_PX) as u32, TILE_PX as u32)
            );
            assert_eq!(open.zoom, DEFAULT_ZOOM);

            let bytes = open.loaded.grid.as_bytes(0).unwrap();
            assert_eq!(
                bytes.len(),
                FIXTURE_TILES * TILE_PIXELS * 4,
                "the grid image must not be empty"
            );
            // Row 0: tile 0's 16 px transparent, then tiles 1-2 opaque red
            // (BGRA, so red is [0, 0, 255, 255]).
            assert!(
                bytes[..TILE_PX * 4].chunks_exact(4).all(|p| p[3] == 0),
                "tile 0 (all index 0) must be transparent"
            );
            assert!(
                bytes[TILE_PX * 4..FIXTURE_TILES * TILE_PX * 4]
                    .chunks_exact(4)
                    .all(|p| p == [0, 0, 255, 255]),
                "tiles 1-2 (palette index 1) must be opaque red in BGRA"
            );
        });
    }

    /// A `.til` with no companion `.pal` still loads -- worldlib swaps in
    /// its 16-gray fallback and flags it, which the header surfaces.
    #[gpui::test]
    async fn test_missing_pal_still_loads_with_the_fallback_palette(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "world");
        std::fs::remove_file(dir.path().join("tiles/world.pal")).unwrap();
        let root = dir.path().to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            let open = ready(panel);
            assert!(open.loaded.missing_pal);
            assert_eq!(open.loaded.tile_count, FIXTURE_TILES);
        });
    }

    /// A bad `.til` lands in Error, not a panic -- and the panel stays
    /// usable.
    #[gpui::test]
    async fn test_a_malformed_til_reports_an_error(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tiles")).unwrap();
        // One byte short of a whole tile.
        std::fs::write(dir.path().join("tiles/bad.til"), vec![0u8; 127]).unwrap();
        let root = dir.path().to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });
        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/bad.til", cx);
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert!(
                matches!(&panel.state, ViewerState::Error(_)),
                "a malformed .til must surface as Error"
            );
        });
    }

    /// Zoom is integer and clamped at both ends.
    #[gpui::test]
    async fn test_zoom_steps_and_clamps(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;

        panel.update(cx, |panel, cx| {
            panel.zoom_by(1, cx);
            assert_eq!(ready(panel).zoom, DEFAULT_ZOOM + 1);
            for _ in 0..20 {
                panel.zoom_by(1, cx);
            }
            assert_eq!(ready(panel).zoom, MAX_ZOOM);
            let (w, _) = ready(panel).zoomed_size();
            assert_eq!(w, (FIXTURE_TILES * TILE_PX * MAX_ZOOM) as f32);
            for _ in 0..20 {
                panel.zoom_by(-1, cx);
            }
            assert_eq!(ready(panel).zoom, MIN_ZOOM);
        });
    }

    // ------------------------------------------ explorer-driven routing

    /// A fake-fs project with one visible worktree holding the same file
    /// names the real-fs `root` fixture does: the interceptor only needs a
    /// worktree id and a rel path, while the panel loads the actual tileset
    /// bytes through `std::fs` from `root` (`root_override`).
    async fn routed_project(
        cx: &mut TestAppContext,
        root: &std::path::Path,
        run_init: bool,
    ) -> Entity<Project> {
        write_tileset_fixture(root, "world");
        write_tileset_fixture(root, "other");
        cx.update(|cx| {
            AppState::test(cx);
            if run_init {
                init(cx);
            }
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({ "tiles": { "world.til": "", "other.til": "" }, "notes.txt": "" }),
        )
        .await;
        Project::test(fs, ["/proj".as_ref()], cx).await
    }

    fn worktree_id(project: &Entity<Project>, cx: &mut gpui::VisualTestContext) -> WorktreeId {
        project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("one visible worktree")
                .read(cx)
                .id()
        })
    }

    fn project_path(worktree_id: WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: path::rel_path::rel_path(rel).into_arc(),
        }
    }

    /// The registered `.til` predicate claims the path (so the project
    /// panel opens NO pane item for it), opens the dock, and loads the
    /// tileset. A non-`.til` in the same worktree is declined.
    #[gpui::test]
    async fn test_til_click_routes_into_the_panel_and_is_claimed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let project = routed_project(cx, dir.path(), true).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<TilesetPanel>(cx)
                .expect("init() adds the panel")
        });
        let root = dir.path().to_path_buf();
        panel.update(cx, |panel, _| panel.root_override = Some(root));

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "tiles/world.til"), window, cx)
        });
        assert!(claimed, "a .til must be claimed, suppressing the pane item");
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(ready(panel).rel_path, "tiles/world.til");
        });
        workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace.right_dock().read(cx).is_open(),
                "routing must open the panel's dock even if it was closed"
            );
        });

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "notes.txt"), window, cx)
        });
        assert!(!claimed, "everything but .til opens the normal way");
    }

    /// Switching documents just loads the new one -- there is no dirty
    /// state to prompt about.
    #[gpui::test]
    async fn test_open_rel_path_switches_documents_without_prompting(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "other");
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("tiles/other.til", window, cx)
            })
        });
        assert!(
            !cx.has_pending_prompt(),
            "a read-only viewer must never prompt on switch"
        );
        cx.run_until_parked();
        panel.update(cx, |panel, _cx| {
            assert_eq!(ready(panel).rel_path, "tiles/other.til");
        });
    }

    /// Clicking the file that is ALREADY open must be a pure focus/reveal:
    /// no reload, so the view state survives. The zoom assertion is the
    /// load-bearing one -- a reload would rebuild `OpenTileset` and snap
    /// the zoom back to its default.
    #[gpui::test]
    async fn test_open_rel_path_on_the_open_tileset_does_not_reload(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let panel = ready_panel(cx, dir.path()).await;
        let cx = cx.add_empty_window();

        panel.update(cx, |panel, cx| panel.zoom_by(3, cx));
        let generation = panel.read_with(cx, |panel, _| panel.load_generation);

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.open_rel_path("tiles/world.til", window, cx)
            })
        });
        cx.run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                panel.load_generation, generation,
                "an already-open click must not start another load"
            );
            assert_eq!(
                ready(panel).zoom,
                DEFAULT_ZOOM + 3,
                "the view state must survive an already-open click"
            );
        });
    }

    /// REAL root discovery, with no `root_override`: the panel wired into a
    /// workspace whose (fake) worktree is mounted AT the real-fs fixture
    /// root must find that root through `set_active`'s deferred
    /// `refresh_root` -- the branch every other test bypasses -- and then
    /// load the fixture through it to Ready.
    #[gpui::test]
    async fn test_set_active_discovers_the_root_through_the_workspace(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "world");
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });

        // Mount the fake worktree AT the real-fs fixture root (the
        // `ggo_sprite_panel::routed_project` trick): the workspace hands the
        // panel this abs path as its root, and the loader then does real
        // `std::fs` work there -- one path serves both.
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            dir.path().to_str().expect("utf8 tempdir path"),
            serde_json::json!({ "tiles": { "world.til": "" } }),
        )
        .await;
        let project = Project::test(fs, [dir.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<TilesetPanel>(cx)
                .expect("init() adds the panel")
        });

        panel.update_in(cx, |panel, window, cx| {
            assert!(panel.root_override.is_none(), "discovery must be real");
            assert!(
                panel.project_root.is_none(),
                "no root before the panel is first activated"
            );
            panel.set_active(true, window, cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.project_root.is_some(),
                "set_active's deferred refresh must discover the worktree root"
            );
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.open_rel_path("tiles/world.til", window, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                ready(panel).rel_path,
                "tiles/world.til",
                "the discovered root must be the one the loader reads from"
            );
        });
    }

    /// Two loads issued back to back: whatever order the executor completes
    /// them in (hence the iterations), the SECOND is the one that must land
    /// -- a completed stale first load can never overwrite it.
    #[gpui::test(iterations = 10)]
    async fn test_a_stale_load_is_dropped_by_the_generation_guard(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "world");
        write_tileset_fixture(dir.path(), "other");
        let root = dir.path().to_path_buf();
        let panel = cx.update(|cx| {
            cx.new(|cx| {
                let mut panel = TilesetPanel::new(None, cx);
                panel.root_override = Some(root);
                panel
            })
        });

        panel.update(cx, |panel, cx| {
            panel.refresh_root(cx);
            panel.load_rel_path("tiles/world.til", cx);
            panel.load_rel_path("tiles/other.til", cx);
            assert!(
                matches!(&panel.state, ViewerState::Loading { rel_path } if rel_path == "tiles/other.til"),
                "the second load owns the Loading state immediately"
            );
        });
        cx.executor().run_until_parked();

        panel.update(cx, |panel, _cx| {
            assert_eq!(
                ready(panel).rel_path,
                "tiles/other.til",
                "the superseded first load must never win, whichever finishes last"
            );
        });
    }

    /// Sanity: the fixture really is on disk in the format worldlib reads
    /// back (guards the test fixture itself, mirroring worldlib's own
    /// `save_tileset` round-trip).
    #[test]
    fn fixture_round_trips_through_worldlib() {
        let dir = tempfile::tempdir().unwrap();
        write_tileset_fixture(dir.path(), "world");
        let r = open_tileset(dir.path(), "tiles/world.til").unwrap();
        assert_eq!(r.tile_count, FIXTURE_TILES);
        assert_eq!(r.indices[0], 0);
        assert_eq!(r.indices[TILE_PIXELS], 1);
        assert!(!r.missing_pal);
    }
}
